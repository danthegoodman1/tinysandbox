//! Cross-layer regressions use observable effects and independent shell behavior.
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use tinysandbox::sandbox::{CommandResult, Limits, Sandbox};
use tinysandbox::vfs::{
    DirEntry, Errno, FileHandle, InMemoryVfs, Metadata, OpenMode, Vfs, VfsResult, VfsSnapshot,
};
use tokio::io::AsyncWriteExt;

#[cfg(unix)]
#[tokio::test]
async fn null_commands_assignments_and_status_match_bash_effects() {
    // Bash is the independent oracle; diagnostics may differ, but success,
    // stdout, assignment state, and every redirect's file effect must agree.
    for script in [
        "> empty",
        "X=value > assignment",
        "X=value < missing",
        "X=before; X=after > first < missing",
        "> first > second",
        "false || echo $?",
        "false && echo no; echo $?",
        "false || false || echo $?",
        "X=before; X=after > assignment | cat; echo X=$X",
        "X=first; X=second > $X",
        "X=first; X=second > $X | cat",
        "echo data | X=pipe; echo X=$X",
    ] {
        let temp = Scratch::new("bash");
        let script_with_state = format!("{script}; echo __state__ X=$X status=$?");
        let reference = std::process::Command::new("/bin/bash")
            .args(["--noprofile", "--norc", "-c", &script_with_state])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .current_dir(&temp.0)
            .output()
            .expect("run independent Bash reference");
        let sandbox = Sandbox::builder().persist_session(true).build();
        let first = sandbox.exec(script).await;
        let state = sandbox.exec("echo __state__ X=$X status=$?").await;
        assert_eq!(
            format!("{}{}", first.stdout, state.stdout).as_bytes(),
            reference.stdout,
            "stdout/state mismatch for {script}"
        );
        assert_eq!(
            state.exit_code,
            reference.status.code().unwrap(),
            "{script}"
        );
        assert_eq!(
            first.stderr.is_empty(),
            reference.stderr.is_empty(),
            "diagnostic mismatch for {script}"
        );
        for path in ["empty", "assignment", "first", "second"] {
            let expected = std::fs::read(temp.0.join(path));
            let actual = sandbox.fs().read_file(path).await;
            match (actual, expected) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "{script}: {path}"),
                (Err(actual), Err(expected)) => {
                    assert_eq!(actual.errno(), Errno::ENOENT, "{script}: {path}");
                    assert_eq!(expected.kind(), std::io::ErrorKind::NotFound);
                }
                values => panic!("file effect mismatch for {script}: {path}: {values:?}"),
            }
        }
    }
}

#[tokio::test]
async fn assignment_pipeline_budget_rejects_before_creating_pipes_or_dispatching() {
    let invoked = Arc::new(AtomicBool::new(false));
    let command_flag = invoked.clone();
    let sandbox = Sandbox::builder()
        .limits(Limits {
            max_commands: 1,
            ..Limits::default()
        })
        .command("observe", move |_| {
            let flag = command_flag.clone();
            async move {
                flag.store(true, Ordering::Relaxed);
                CommandResult::success()
            }
        })
        .build();
    let assignments = (0..100)
        .map(|n| format!("X={n}"))
        .collect::<Vec<_>>()
        .join(" | ");
    for script in [assignments, "observe | X=unused".to_owned()] {
        let result = sandbox.exec(&script).await;
        assert_eq!(result.exit_code, 125, "{}", result.stderr);
        assert!(result.stderr.contains("maximum command count exceeded"));
        assert!(
            result.metrics.pipe_bytes.is_empty(),
            "rejected pipeline allocated pipe metrics"
        );
        assert!(
            !invoked.load(Ordering::Relaxed),
            "a rejected stage was dispatched"
        );
    }
}

#[tokio::test]
async fn assignment_pipeline_stage_closes_input_without_draining() {
    let written = Arc::new(AtomicU64::new(0));
    let counter = written.clone();
    let sandbox = Sandbox::builder()
        .command("produce", move |mut ctx| {
            let counter = counter.clone();
            async move {
                let chunk = vec![b'x'; 64 * 1024];
                for _ in 0..1024 {
                    if ctx.stdout.write_all(&chunk).await.is_err() {
                        return CommandResult::failure();
                    }
                    counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
                CommandResult::success()
            }
        })
        .build();
    let result = tokio::time::timeout(Duration::from_secs(2), sandbox.exec("produce | X=done"))
        .await
        .expect("null pipeline stage must close its upstream pipe");
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert!(result.stdout.is_empty());
    assert!(
        written.load(Ordering::Relaxed) <= 128 * 1024,
        "null stage drained {} bytes",
        written.load(Ordering::Relaxed)
    );
}

#[tokio::test]
async fn mixed_mount_dispatch_uses_resolved_path_and_handle_backend() {
    let runtime_thread = std::thread::current().id();
    let fast = Arc::new(ThreadObserver::new(true));
    let slow = Arc::new(ThreadObserver::new(false));
    let sandbox = Sandbox::builder()
        .mount_arc("workspace", fast.clone())
        .mount_arc("slow", slow.clone())
        .command("probe", |ctx| async move {
            for path in ["/workspace/file", "/slow/file"] {
                ctx.fs.stat(path).await.unwrap();
                let handle = ctx.fs.open(path, OpenMode::read_write()).await.unwrap();
                let (_, n) = ctx.fs.read_at(handle, 0, vec![0; 3]).await.unwrap();
                assert_eq!(n, 3);
                assert_eq!(
                    ctx.fs.write_at(handle, 0, b"new".to_vec()).await.unwrap(),
                    3
                );
                ctx.fs.truncate(handle, 3).await.unwrap();
                ctx.fs.close(handle).await.unwrap();
            }
            CommandResult::success()
        })
        .build();
    let result = sandbox.exec("probe").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    for (observer, inline) in [(fast, true), (slow, false)] {
        let observed = observer.calls.lock().unwrap();
        for operation in ["stat", "open", "read", "write", "truncate", "close"] {
            assert!(
                observed.iter().any(|(name, _)| *name == operation),
                "missing {operation}"
            );
        }
        assert!(
            observed
                .iter()
                .all(|(_, thread)| (*thread == runtime_thread) == inline),
            "wrong dispatch classification: inline={inline}, {observed:?}"
        );
    }
}

#[test]
fn memory_depth_ceiling_survives_snapshot_clone_branch_restore_and_drop() {
    let vfs = InMemoryVfs::default();
    let mut directory = String::new();
    // The deepest file has exactly 256 components. Exercise recursive internal
    // snapshot/clone/drop paths at the admitted ceiling, not unbounded input.
    for _ in 0..255 {
        directory.push_str("/d");
        vfs.mkdir(&directory).unwrap();
    }
    let path = format!("{directory}/file");
    let handle = vfs.open(&path, OpenMode::write_only().create()).unwrap();
    vfs.write_at(handle, 0, b"snapshot").unwrap();
    vfs.close(handle).unwrap();
    let snapshot = vfs.snapshot().unwrap();
    let cloned = snapshot.clone();
    let branch = vfs.branch(&cloned).unwrap();
    vfs.unlink(&path).unwrap();
    assert_eq!(branch.stat(&path).unwrap().len, 8);
    vfs.restore(&cloned).unwrap();
    assert_eq!(vfs.stat(&path).unwrap().len, 8);
    let too_deep = format!("{directory}/extra/too-deep");
    assert_eq!(vfs.mkdir(&too_deep).unwrap_err().errno(), Errno::EINVAL);
    assert_eq!(vfs.stat(&too_deep).unwrap_err().errno(), Errno::EINVAL);
    drop(branch);
    drop(cloned);
    drop(snapshot);
    drop(vfs);
}

#[tokio::test]
async fn execution_path_depth_can_be_stricter_than_backend_ceiling() {
    let sandbox = Sandbox::builder()
        .limits(Limits {
            max_path_depth: 3,
            ..Limits::default()
        })
        .build();
    assert_eq!(sandbox.exec("mkdir -p a/b").await.exit_code, 0);
    let result = sandbox.exec("touch a/b/rejected").await;
    assert_eq!(result.exit_code, 1);
    assert_eq!(
        sandbox
            .vfs()
            .stat("/workspace/a/b/rejected")
            .unwrap_err()
            .errno(),
        Errno::ENOENT
    );
}

#[derive(Debug)]
struct ThreadObserver {
    inner: InMemoryVfs,
    fast: bool,
    calls: Mutex<Vec<(&'static str, ThreadId)>>,
}
impl ThreadObserver {
    fn new(fast: bool) -> Self {
        let inner = InMemoryVfs::default();
        let handle = inner
            .open("/file", OpenMode::write_only().create())
            .unwrap();
        inner.write_at(handle, 0, b"old").unwrap();
        inner.close(handle).unwrap();
        Self {
            inner,
            fast,
            calls: Mutex::new(Vec::new()),
        }
    }
    fn record(&self, operation: &'static str) {
        self.calls
            .lock()
            .unwrap()
            .push((operation, std::thread::current().id()));
    }
}
impl Vfs for ThreadObserver {
    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        self.record("stat");
        self.inner.stat(path)
    }
    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        self.inner.readdir(path)
    }
    fn mkdir(&self, path: &str) -> VfsResult<()> {
        self.inner.mkdir(path)
    }
    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        self.inner.rename(from, to)
    }
    fn unlink(&self, path: &str) -> VfsResult<()> {
        self.inner.unlink(path)
    }
    fn rmdir(&self, path: &str) -> VfsResult<()> {
        self.inner.rmdir(path)
    }
    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        self.record("open");
        self.inner.open(path, mode)
    }
    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.record("read");
        self.inner.read_at(handle, offset, buf)
    }
    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        self.record("write");
        self.inner.write_at(handle, offset, data)
    }
    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        self.record("truncate");
        self.inner.truncate(handle, len)
    }
    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        self.record("close");
        self.inner.close(handle)
    }
    fn is_fast(&self) -> bool {
        self.fast
    }
}

#[cfg(unix)]
struct Scratch(PathBuf);
#[cfg(unix)]
impl Scratch {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tinysandbox-boundary-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
#[cfg(unix)]
impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[tokio::test]
async fn shell_expansion_rejects_amplification_before_redirect_mutation() {
    let sandbox = Sandbox::builder()
        .limits(Limits {
            shell_input_bytes: 1024,
            host_input_bytes: 1024,
            ..Limits::default()
        })
        .build();
    let program = format!(
        "X=x; {}; echo unreachable > /workspace/created",
        vec!["X=$X$X"; 20].join("; ")
    );
    let result = sandbox.exec(&program).await;
    assert_eq!(result.exit_code, 125);
    assert!(result.stderr.contains("expansion limit"));
    assert!(sandbox.fs().stat("/workspace/created").await.is_err());
    let sandbox = Sandbox::builder()
        .env("FIELDS", "x ".repeat(400))
        .limits(Limits {
            shell_input_bytes: 1024,
            host_input_bytes: 1024,
            ..Limits::default()
        })
        .build();
    let result = sandbox.exec("echo $FIELDS > /workspace/created").await;
    assert_eq!(result.exit_code, 125);
    assert!(sandbox.fs().stat("/workspace/created").await.is_err());
}

#[tokio::test]
async fn pipeline_expansion_budget_covers_all_retained_stages_before_redirects() {
    let invoked = Arc::new(AtomicBool::new(false));
    let command_flag = invoked.clone();
    let sandbox = Sandbox::builder()
        .limits(Limits {
            shell_input_bytes: 1024,
            host_input_bytes: 1024,
            ..Limits::default()
        })
        .command("observe", move |_| {
            let flag = command_flag.clone();
            async move {
                flag.store(true, Ordering::Relaxed);
                CommandResult::success()
            }
        })
        .build();
    // Each individual stage fits in 1 KiB. Retaining sixteen copies of the
    // guest-created environment does not, even though the shell source fits.
    let pipeline = (0..16)
        .map(|index| format!("observe > /workspace/out{index}"))
        .collect::<Vec<_>>()
        .join(" | ");
    let script = format!("X={}; {pipeline}", "x".repeat(128));
    assert!(script.len() < 1024);
    let result = sandbox.exec(&script).await;
    assert_eq!(result.exit_code, 125, "{}", result.stderr);
    assert!(result.stderr.contains("expansion limit"));
    assert!(result.metrics.pipe_bytes.is_empty());
    assert!(!invoked.load(Ordering::Relaxed));
    for index in 0..16 {
        assert!(
            sandbox
                .fs()
                .stat(&format!("/workspace/out{index}"))
                .await
                .is_err(),
            "a rejected pipeline created redirect {index}"
        );
    }
}

#[tokio::test]
async fn null_redirect_admission_uses_assigned_variable_values() {
    let sandbox = Sandbox::builder()
        .env("X", "x".repeat(128))
        .limits(Limits {
            shell_input_bytes: 1024,
            host_input_bytes: 1024,
            ..Limits::default()
        })
        .build();
    let program = format!("Y=$X > \"{}\"", "$Y".repeat(64));
    let result = sandbox.exec(&program).await;
    assert_eq!(result.exit_code, 125);
    assert!(result.stderr.contains("expansion limit"));
    assert!(sandbox.fs().readdir("/workspace").await.unwrap().is_empty());
}
