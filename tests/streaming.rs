use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tinysandbox::sandbox::{Limits, Sandbox};
use tinysandbox::vfs::{
    DirEntry, Errno, FileHandle, FileType, InMemoryVfs, Metadata, OpenMode, Vfs, VfsError,
    VfsResult,
};

const HUGE: &str = "/huge";
const LINE_LEN: u64 = 1024;

#[tokio::test]
async fn head_stops_generated_vfs_after_small_prefix() {
    // Proves early downstream exit closes the pipe before the virtual GiB-scale
    // input is fully read.
    let vfs = Arc::new(GeneratingVfs::new(4 * 1024 * 1024 * 1024));
    let sandbox = Sandbox::builder().vfs_arc(vfs.clone()).build();

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        sandbox.exec("cat /huge | head -n 1"),
    )
    .await
    .expect("pipeline should not deadlock");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.len(), LINE_LEN as usize);
    assert!(result.stderr.is_empty());
    assert!(vfs.served() <= 2 * 1024 * 1024, "served {}", vfs.served());
    assert_eq!(
        result
            .metrics
            .commands
            .last()
            .expect("head timing")
            .exit_code,
        0
    );
    assert!(
        result
            .metrics
            .commands
            .iter()
            .any(|timing| timing.name == "cat" && timing.exit_code == 141)
    );
}

#[tokio::test]
async fn full_scan_streams_data_larger_than_pipe_capacity() {
    // `wc` and `grep -c` must scan the full generated file without relying on
    // whole-file materialization in the VFS.
    let size = 64 * 1024 * 1024;
    let vfs = Arc::new(GeneratingVfs::new(size));
    let sandbox = Sandbox::builder().vfs_arc(vfs.clone()).build();

    let wc = tokio::time::timeout(Duration::from_secs(5), sandbox.exec("wc -c < /huge"))
        .await
        .expect("wc should finish");
    assert_eq!(wc.exit_code, 0);
    assert_eq!(wc.stdout.trim(), size.to_string());

    vfs.reset_served();
    let grep = tokio::time::timeout(
        Duration::from_secs(5),
        sandbox.exec("grep -c pattern /huge"),
    )
    .await
    .expect("grep should finish");
    assert_eq!(grep.exit_code, 0);
    assert_eq!(grep.stdout.trim(), (size / LINE_LEN).to_string());
    assert_eq!(vfs.served(), size);
}

#[tokio::test]
async fn redirect_write_streams_to_vfs_file() {
    // Large redirect output is written through the handle API while the command
    // runs, not accumulated as a command-sized buffer first.
    let size = 8 * 1024 * 1024;
    let vfs = Arc::new(GeneratingVfs::new(size));
    let sandbox = Sandbox::builder().vfs_arc(vfs.clone()).build();

    let result = tokio::time::timeout(Duration::from_secs(5), sandbox.exec("cat /huge > /out"))
        .await
        .expect("redirect should finish");
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.is_empty());
    assert_eq!(vfs.stat("/out").expect("out stat").len, size);

    let handle = vfs
        .open("/out", OpenMode::read_only())
        .expect("open redirected output");
    let mut sample = vec![0; LINE_LEN as usize];
    let read = vfs.read_at(handle, 0, &mut sample).expect("read sample");
    vfs.close(handle).expect("close sample");
    assert_eq!(read, sample.len());
    assert_eq!(sample[0], b'p');
    assert_eq!(sample[LINE_LEN as usize - 1], b'\n');
}

#[tokio::test]
async fn deadlock_regressions_complete_under_timeout() {
    // Covers fast producer with slow/early consumers, a middle stage exiting
    // immediately, and command-budget rejection before a pipeline is spawned.
    let vfs = Arc::new(GeneratingVfs::new(16 * 1024 * 1024));
    let sandbox = Sandbox::builder().vfs_arc(vfs.clone()).build();

    let head = tokio::time::timeout(
        Duration::from_secs(2),
        sandbox.exec("cat /huge | head -n 10"),
    )
    .await
    .expect("head pipeline should not deadlock");
    assert_eq!(head.exit_code, 0);

    let fail_mid = tokio::time::timeout(
        Duration::from_secs(2),
        sandbox.exec("cat /huge | false | wc -c"),
    )
    .await
    .expect("failed middle stage should not deadlock");
    assert_eq!(fail_mid.exit_code, 0);
    assert_eq!(fail_mid.stdout.trim(), "0");

    let limited = Sandbox::builder()
        .vfs_arc(vfs)
        .limits(Limits {
            max_commands: 1,
            ..Limits::default()
        })
        .build();
    let limit = tokio::time::timeout(Duration::from_secs(2), limited.exec("cat /huge | wc -c"))
        .await
        .expect("limit hit should not spawn a stuck pipeline");
    assert_eq!(limit.exit_code, 125);
    assert!(limit.stderr.contains("maximum command count exceeded"));
}

#[tokio::test]
async fn output_cap_truncates_while_draining_stream() {
    // The capture stays small while `cat` still drains the full generated input.
    let size = 8 * 1024 * 1024;
    let vfs = Arc::new(GeneratingVfs::new(size));
    let sandbox = Sandbox::builder()
        .vfs_arc(vfs.clone())
        .limits(Limits {
            stdout_bytes: 1024,
            ..Limits::default()
        })
        .build();

    let result = tokio::time::timeout(Duration::from_secs(5), sandbox.exec("cat /huge"))
        .await
        .expect("capped output should finish");
    assert_eq!(result.exit_code, 0);
    assert!(result.metrics.stdout_truncated);
    assert!(result.stdout.contains("output truncated"));
    assert!(result.stdout.len() <= 1024);
    assert_eq!(vfs.served(), size);
}

#[tokio::test]
async fn redirected_stdin_in_later_stage_closes_upstream_pipe() {
    // A later-stage stdin redirect replaces the pipe input, so the abandoned
    // upstream pipe reader must close instead of keeping the producer blocked.
    let vfs = Arc::new(GeneratingVfs::new(16 * 1024 * 1024));
    vfs.write_inner("/small", b"abc");
    let sandbox = Sandbox::builder().vfs_arc(vfs).build();

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        sandbox.exec("cat /huge | wc -c < /small"),
    )
    .await
    .expect("redirected later stage should not deadlock");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "3");
    assert!(result.stderr.is_empty());
    assert!(
        result
            .metrics
            .commands
            .iter()
            .any(|timing| timing.name == "cat" && timing.exit_code == 141)
    );
}

#[tokio::test]
async fn pipeline_shell_builtins_do_not_mutate_session_but_still_write_stdout() {
    // Bash runs multi-stage pipeline members in subshells: mutations vanish,
    // but builtins like `export` still execute and can feed the pipe.
    let sandbox = Sandbox::builder().build();

    assert_eq!(sandbox.exec("mkdir /sub").await.exit_code, 0);
    let cd = sandbox.exec("cd /sub | cat; pwd").await;
    assert_eq!(cd.exit_code, 0);
    assert_eq!(cd.stdout, "/\n");

    let assignment = sandbox.exec("FOO=bar | cat; echo $FOO").await;
    assert_eq!(assignment.exit_code, 0);
    assert_eq!(assignment.stdout, "\n");

    let export = sandbox.exec("export FOO=bar; export | cat").await;
    assert_eq!(export.exit_code, 0);
    assert!(export.stdout.contains("declare -x FOO=\"bar\"\n"));
}

#[tokio::test]
async fn assignment_only_pipeline_stage_preserves_topology() {
    // Assignment-only stages consume stdin and close stdout, so upstream output
    // does not get rerouted to the final capture.
    let sandbox = Sandbox::builder().build();

    let result = sandbox.exec("echo data | FOO=1").await;

    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.is_empty());
    assert!(result.stderr.is_empty());
}

#[tokio::test]
async fn per_stage_redirect_failure_does_not_abort_pipeline() {
    // A failed redirect skips only that stage; downstream commands still see
    // EOF on their pipe and determine the pipeline status.
    let sandbox = Sandbox::builder().build();

    let result = sandbox.exec("cat < /missing | wc -l").await;

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "0");
    assert!(
        result
            .stderr
            .contains("cat: /missing: No such file or directory")
    );
}

#[tokio::test]
async fn timeout_aborts_spawned_pipeline_tasks() {
    // If pipeline tasks are detached, this command writes after the exec-wide
    // timeout returns; abort-on-drop prevents that late mutation.
    let vfs = Arc::new(InMemoryVfs::default());
    let sandbox = Sandbox::builder()
        .vfs_arc(vfs.clone())
        .limits(Limits {
            wall_time: Duration::from_millis(50),
            ..Limits::default()
        })
        .command("latewrite", |ctx| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            ctx.fs
                .write_file("/late", b"too late", false)
                .await
                .expect("late write");
            tinysandbox::sandbox::CommandResult::success()
        })
        .build();

    let result = sandbox.exec("latewrite | cat").await;
    assert_eq!(result.exit_code, 124);

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        vfs.stat("/late")
            .expect_err("late write was aborted")
            .errno(),
        Errno::ENOENT
    );
}

#[tokio::test]
async fn mid_stream_vfs_read_errors_are_reported() {
    // The reader must surface failures after already returning some bytes; EOF
    // would make these commands falsely succeed with truncated input.
    let vfs = Arc::new(GeneratingVfs::new(4 * 1024 * 1024).fail_after(128 * 1024));
    let sandbox = Sandbox::builder().vfs_arc(vfs.clone()).build();

    let wc = sandbox.exec("wc -c < /huge").await;
    assert_eq!(wc.exit_code, 1);
    assert!(wc.stderr.contains("wc: -: Invalid argument"));

    let cat = sandbox.exec("cat /huge > /out").await;
    assert_eq!(cat.exit_code, 1);
    assert!(cat.stderr.contains("cat: /huge: Invalid argument"));
    assert!(vfs.stat("/out").expect("partial out exists").len < 4 * 1024 * 1024);
}

#[tokio::test]
async fn grep_closed_pipe_stops_without_stderr() {
    // GNU grep exits silently on SIGPIPE when a downstream command stops early.
    let vfs = Arc::new(GeneratingVfs::new(8 * 1024 * 1024));
    let sandbox = Sandbox::builder().vfs_arc(vfs).build();

    let result = sandbox.exec("grep p /huge | head -n 1").await;

    assert_eq!(result.exit_code, 0);
    assert!(result.stderr.is_empty());
    assert!(result.stdout.starts_with("pattern"));
}

#[tokio::test]
async fn long_no_newline_streams_are_bounded() {
    // Plain cat can stream an over-limit line in chunks, while line-oriented
    // commands reject the same input before buffering it all.
    let size = 2 * 1024 * 1024;
    let vfs = Arc::new(GeneratingVfs::new_without_newlines(size));
    let sandbox = Sandbox::builder().vfs_arc(vfs).build();

    let cat = sandbox.exec("cat /huge | wc -c").await;
    assert_eq!(cat.exit_code, 0);
    assert_eq!(cat.stdout.trim(), size.to_string());
    assert!(cat.stderr.is_empty());

    let grep = sandbox.exec("grep z /huge").await;
    assert_eq!(grep.exit_code, 2);
    assert!(grep.stderr.contains("grep: /huge: line too long"));
}

#[tokio::test]
async fn wc_word_count_uses_c_locale_ascii_separators() {
    // Matches GNU wc under LC_ALL=C: UTF-8 separators are bytes inside a word,
    // while ASCII space still splits words.
    let vfs = Arc::new(InMemoryVfs::default());
    write_file(vfs.as_ref(), "/nbsp", "a\u{00a0}b\n".as_bytes());
    write_file(vfs.as_ref(), "/u2028", "a\u{2028}b\n".as_bytes());
    write_file(vfs.as_ref(), "/space", b"a b\n");
    let sandbox = Sandbox::builder().vfs_arc(vfs).build();

    assert_eq!(sandbox.exec("wc -w /nbsp").await.stdout.trim(), "1 /nbsp");
    assert_eq!(sandbox.exec("wc -w /u2028").await.stdout.trim(), "1 /u2028");
    assert_eq!(sandbox.exec("wc -w /space").await.stdout.trim(), "2 /space");
}

fn write_file(vfs: &dyn Vfs, path: &str, data: &[u8]) {
    let handle = vfs
        .open(path, OpenMode::write_only().create_new())
        .expect("create test file");
    vfs.write_at(handle, 0, data).expect("write test file");
    vfs.close(handle).expect("close test file");
}

#[derive(Debug)]
struct GeneratingVfs {
    inner: InMemoryVfs,
    size: u64,
    newlines: bool,
    fail_after: Option<u64>,
    served: AtomicU64,
    next_handle: AtomicU64,
    handles: Mutex<BTreeMap<FileHandle, GeneratedHandle>>,
}

impl GeneratingVfs {
    fn new(size: u64) -> Self {
        Self {
            inner: InMemoryVfs::default(),
            size: size / LINE_LEN * LINE_LEN,
            newlines: true,
            fail_after: None,
            served: AtomicU64::new(0),
            next_handle: AtomicU64::new(1_000_000),
            handles: Mutex::new(BTreeMap::new()),
        }
    }

    fn new_without_newlines(size: u64) -> Self {
        Self {
            inner: InMemoryVfs::default(),
            size,
            newlines: false,
            fail_after: None,
            served: AtomicU64::new(0),
            next_handle: AtomicU64::new(1_000_000),
            handles: Mutex::new(BTreeMap::new()),
        }
    }

    fn fail_after(mut self, bytes: u64) -> Self {
        self.fail_after = Some(bytes);
        self
    }

    fn write_inner(&self, path: &str, data: &[u8]) {
        let handle = self
            .inner
            .open(path, OpenMode::write_only().create_new())
            .expect("create inner file");
        self.inner
            .write_at(handle, 0, data)
            .expect("write inner file");
        self.inner.close(handle).expect("close inner file");
    }

    fn served(&self) -> u64 {
        self.served.load(Ordering::Relaxed)
    }

    fn reset_served(&self) {
        self.served.store(0, Ordering::Relaxed);
    }

    fn handles(&self) -> std::sync::MutexGuard<'_, BTreeMap<FileHandle, GeneratedHandle>> {
        self.handles.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Clone, Copy)]
struct GeneratedHandle {
    readable: bool,
}

impl Vfs for GeneratingVfs {
    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        if path == HUGE {
            return Ok(Metadata {
                file_type: FileType::File,
                len: self.size,
            });
        }
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
        if path == HUGE {
            if !mode.read || mode.write {
                return Err(VfsError::new(Errno::EACCES));
            }
            let handle = FileHandle::new(self.next_handle.fetch_add(1, Ordering::Relaxed));
            self.handles().insert(
                handle,
                GeneratedHandle {
                    readable: mode.read,
                },
            );
            return Ok(handle);
        }
        self.inner.open(path, mode)
    }

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if let Some(generated) = self.handles().get(&handle).copied() {
            if !generated.readable {
                return Err(VfsError::new(Errno::EBADF));
            }
            if offset >= self.size {
                return Ok(0);
            }
            if self.fail_after.is_some_and(|limit| offset >= limit) {
                return Err(VfsError::new(Errno::EINVAL));
            }
            let n = buf
                .len()
                .min(usize::try_from(self.size - offset).unwrap_or(usize::MAX));
            let n = if let Some(limit) = self.fail_after {
                n.min(usize::try_from(limit.saturating_sub(offset)).unwrap_or(usize::MAX))
            } else {
                n
            };
            for (index, byte) in buf[..n].iter_mut().enumerate() {
                *byte = generated_byte(offset + index as u64, self.newlines);
            }
            self.served.fetch_add(n as u64, Ordering::Relaxed);
            return Ok(n);
        }
        self.inner.read_at(handle, offset, buf)
    }

    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        self.inner.write_at(handle, offset, data)
    }

    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        self.inner.truncate(handle, len)
    }

    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        if self.handles().remove(&handle).is_some() {
            return Ok(());
        }
        self.inner.close(handle)
    }

    fn is_fast(&self) -> bool {
        true
    }

    fn stats(&self) -> Option<VfsResult<tinysandbox::vfs::VfsStats>> {
        Some(self.inner.stats())
    }
}

fn generated_byte(offset: u64, newlines: bool) -> u8 {
    if !newlines {
        return b'a';
    }
    let pos = offset % LINE_LEN;
    match pos {
        0 => b'p',
        1 => b'a',
        2 => b't',
        3 => b't',
        4 => b'e',
        5 => b'r',
        6 => b'n',
        pos if pos + 1 == LINE_LEN => b'\n',
        _ => b'a',
    }
}
