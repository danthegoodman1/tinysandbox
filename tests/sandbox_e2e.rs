use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tinysandbox::sandbox::{CommandResult, Limits, Sandbox};
use tinysandbox::vfs::{
    DirEntry, FileHandle, InMemoryVfs, Metadata, OpenMode, Vfs, VfsQuota, VfsResult,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn pipelines_redirects_and_session_state_run_through_sandbox() {
    // Exercises the shell executor surface: buffered pipes, redirect writes,
    // `&&`/`||`, and the opt-in persistent shell session mode.
    let sandbox = Sandbox::builder().persist_session(true).build();

    assert_eq!(sandbox.exec("mkdir -p /workspace").await.exit_code, 0);
    assert_eq!(sandbox.exec("cd /workspace").await.exit_code, 0);
    assert_eq!(sandbox.exec("pwd").await.stdout, "/workspace\n");

    assert_eq!(
        sandbox
            .exec("echo TODO one > notes.txt; echo done >> notes.txt; echo TODO two >> notes.txt")
            .await
            .exit_code,
        0
    );
    let result = sandbox.exec("cat notes.txt | grep TODO | wc -l").await;
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "2");
    assert_eq!(result.metrics.pipe_bytes.len(), 2);

    assert_eq!(sandbox.exec("NAME=tinysandbox").await.exit_code, 0);
    assert_eq!(sandbox.exec("echo $NAME").await.stdout, "tinysandbox\n");
    assert_eq!(
        sandbox.exec("false && echo no || echo yes").await.stdout,
        "yes\n"
    );
    assert_eq!(sandbox.exec("echo status=$?").await.stdout, "status=0\n");
}

#[tokio::test]
async fn custom_commands_use_same_registry_and_pipelines_as_builtins() {
    // Confirms third-party commands receive stream-shaped stdio and are visible
    // through the synthesized `/bin` registry.
    let sandbox = Sandbox::builder()
        .command("upper", |mut ctx| async move {
            let mut input = Vec::new();
            ctx.stdin.read_to_end(&mut input).await.expect("read stdin");
            let upper = String::from_utf8_lossy(&input).to_uppercase();
            ctx.stdout
                .write_all(upper.as_bytes())
                .await
                .expect("write stdout");
            CommandResult::success()
        })
        .build();

    let result = sandbox.exec("echo hello | upper").await;
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "HELLO\n");

    let bin = sandbox.exec("ls /bin | grep upper").await;
    assert_eq!(bin.exit_code, 0);
    assert_eq!(bin.stdout, "upper\n");
}

#[tokio::test]
async fn builtin_text_tools_match_supported_gnu_shapes() {
    // Covers representative supported flags for text builtins without relying
    // on host BSD/GNU tool availability.
    let sandbox = Sandbox::builder().build();
    let unsupported = sandbox.exec("printf unsupported").await;
    assert_eq!(unsupported.exit_code, 127);
    assert!(unsupported.stderr.contains("command not found"));

    assert_eq!(sandbox.exec("echo -n hi").await.stdout, "hi");
    assert_eq!(
        sandbox.exec("echo -e 'a\\nb' | grep -n b").await.stdout,
        "2:b\n"
    );
    assert_eq!(
        sandbox
            .exec("echo -e '3\\n1\\n2\\n2' | sort -n -u")
            .await
            .stdout,
        "1\n2\n3\n"
    );
    assert_eq!(
        sandbox.exec("echo -e 'a\\na\\nb' | uniq -c").await.stdout,
        "      2 a\n      1 b\n"
    );
    assert_eq!(
        sandbox.exec("echo cat | sed 's/c/C/'").await.stdout,
        "Cat\n"
    );
    assert_eq!(
        sandbox.exec("echo -e 'a\\nb\\nc' | tail -n 2").await.stdout,
        "b\nc\n"
    );
    assert_eq!(
        sandbox.exec("echo -e 'a\\nb\\nc' | head -n 1").await.stdout,
        "a\n"
    );
    let exotic = sandbox
        .exec(r"echo -e '\0101|\x41|\u03bb|\U0001f642|\a|\cignored'")
        .await;
    assert_eq!(
        exotic.stdout,
        format!(
            "A|A|{}|{}|\u{0007}|",
            '\u{03bb}',
            char::from_u32(0x1f642).expect("valid scalar")
        )
    );
    assert_eq!(
        sandbox.exec(r"echo -e 'pre\0post|\x|\u|\U'").await.stdout,
        "pre\0post|\\x|\\u|\\U\n"
    );
    assert_eq!(
        sandbox
            .exec(r"echo -e '\r|\b|\e|\E|\f|\v|\\|\z'")
            .await
            .stdout,
        "\r|\u{0008}|\u{001b}|\u{001b}|\u{000c}|\u{000b}|\\|\\z\n"
    );
    assert_eq!(sandbox.exec("stat").await.stderr, "stat: missing operand\n");
    assert_eq!(sandbox.exec("grep -z x").await.exit_code, 2);
    assert_eq!(sandbox.exec("sort -z").await.exit_code, 2);
}

#[test]
fn reserved_shell_builtins_cannot_be_shadowed() {
    for name in ["cd", "export", "unset"] {
        let result = std::panic::catch_unwind(|| {
            Sandbox::builder().command(name, |_ctx| async { CommandResult::success() })
        });
        assert!(result.is_err(), "{name} should be reserved");
    }
}

#[tokio::test]
async fn builtin_golden_fixture_matches_gnu_verified_shapes() {
    for case in parse_golden_fixture(include_str!("fixtures/sandbox_builtins_golden.txt")) {
        let sandbox = Sandbox::builder().build();
        let result = sandbox.exec(&case.command).await;
        assert_eq!(
            result.exit_code, case.exit_code,
            "exit mismatch in {}",
            case.name
        );
        assert_eq!(
            result.stdout, case.stdout,
            "stdout mismatch in {}",
            case.name
        );
        assert_eq!(
            result.stderr, case.stderr,
            "stderr mismatch in {}",
            case.name
        );
    }
}

#[tokio::test]
async fn bin_is_synthesized_and_read_only() {
    // `/bin` is registry-backed, not stored in the raw VFS, but shell-visible
    // probes should behave like normal files and reject writes.
    let sandbox = Sandbox::builder().build();

    let listing = sandbox.exec("ls /bin").await;
    assert_eq!(listing.exit_code, 0);
    assert!(listing.stdout.lines().any(|line| line == "cat"));

    assert_eq!(sandbox.exec("stat /bin/cat").await.exit_code, 0);
    let denied = sandbox.exec("echo x > /bin/cat").await;
    assert_ne!(denied.exit_code, 0);
    assert!(denied.stderr.contains("Permission denied"));
}

#[tokio::test]
async fn file_basenames_are_used_for_ls_cp_and_mv_directory_targets() {
    // File-display and directory-target naming both flow through basename,
    // including paths with trailing slashes after normalization.
    let sandbox = Sandbox::builder().build();

    assert_eq!(sandbox.exec("mkdir /dir; touch /leaf").await.exit_code, 0);
    assert_eq!(sandbox.exec("ls /leaf///").await.stdout, "leaf\n");
    assert_eq!(sandbox.exec("cp /leaf/// /dir").await.exit_code, 0);
    assert_eq!(
        sandbox.exec("mv /dir/leaf/// /dir/moved").await.exit_code,
        0
    );
    assert_eq!(sandbox.exec("ls /dir").await.stdout, "moved\n");
}

#[tokio::test]
async fn limits_truncate_output_and_surface_vfs_quota_errors() {
    // Verifies sandbox-level output capping and that ENOSPC from the VFS
    // reaches stderr as an errno-shaped command failure.
    let limits = Limits {
        stdout_bytes: 24,
        ..Limits::default()
    };
    let sandbox = Sandbox::builder().limits(limits).build();
    let result = sandbox.exec("echo 123456789012345678901234567890").await;
    assert!(result.metrics.stdout_truncated);
    assert!(result.stdout.contains("output truncated"));

    let tiny = Sandbox::builder()
        .vfs(InMemoryVfs::new(VfsQuota {
            max_bytes: 3,
            max_files: 4,
            max_file_size: 3,
        }))
        .build();
    let quota = tiny.exec("echo abcdef > /file").await;
    assert_ne!(quota.exit_code, 0);
    assert!(quota.stderr.contains("No space left on device"));
}

#[tokio::test]
async fn limits_stats_and_metrics_are_reported() {
    let limited = Sandbox::builder()
        .limits(Limits {
            max_commands: 1,
            ..Limits::default()
        })
        .build();
    let limit = limited.exec("true; true").await;
    assert_eq!(limit.exit_code, 125);
    assert!(limit.stderr.contains("maximum command count exceeded"));

    let sandbox = Sandbox::builder().build();
    let result = sandbox.exec("echo one; echo two").await;
    assert_eq!(result.exit_code, 0);
    assert!(result.metrics.wall_time > Duration::ZERO);
    assert_eq!(result.metrics.commands.len(), 2);
    assert!(
        result
            .metrics
            .commands
            .iter()
            .all(|timing| !timing.name.is_empty())
    );
    assert_eq!(sandbox.stats().commands_run, 2);
    assert!(sandbox.stats().vfs.is_some());
}

#[tokio::test]
async fn redirects_follow_bash_fd_order_and_preflight_timing() {
    let sandbox = Sandbox::builder()
        .command("both", |mut ctx| async move {
            ctx.stdout.write_all(b"out\n").await.expect("write stdout");
            ctx.stderr.write_all(b"err\n").await.expect("write stderr");
            CommandResult::success()
        })
        .build();

    let split = sandbox.exec("both 2>&1 > /out").await;
    assert_eq!(split.exit_code, 0);
    assert_eq!(split.stdout, "err\n");
    assert_eq!(sandbox.exec("cat /out").await.stdout, "out\n");

    let joined = sandbox.exec("both > /joined 2>&1").await;
    assert_eq!(joined.exit_code, 0);
    assert_eq!(joined.stdout, "");
    assert_eq!(sandbox.exec("cat /joined").await.stdout, "out\nerr\n");

    assert_eq!(sandbox.exec("both 2> /err").await.exit_code, 0);
    assert_eq!(sandbox.exec("both 2>> /err").await.exit_code, 0);
    assert_eq!(sandbox.exec("cat /err").await.stdout, "err\nerr\n");

    let unsupported_fd = sandbox.exec("both 3> /bad").await;
    assert_eq!(unsupported_fd.exit_code, 1);
    assert!(unsupported_fd.stderr.contains("Invalid argument"));

    let missing_input = sandbox.exec("echo ran > /preflight < /missing").await;
    assert_eq!(missing_input.exit_code, 1);
    assert!(missing_input.stderr.contains("No such file or directory"));
    assert_eq!(sandbox.exec("cat /preflight").await.stdout, "");
}

#[tokio::test]
async fn redirect_setup_failures_close_opened_handles() {
    // A later redirect failure must clean up earlier input handles and output sinks.
    let input_vfs = Arc::new(TrackingVfs::default());
    input_vfs.write_seed("/input", b"hello\n");
    let input_sandbox = Sandbox::builder().vfs_arc(input_vfs.clone()).build();

    let input_failure = input_sandbox.exec("cat < /input > /missing/out").await;
    assert_ne!(input_failure.exit_code, 0);
    assert_eq!(input_vfs.live_handles(), 0);

    let output_vfs = Arc::new(TrackingVfs::default());
    output_vfs.fail_second_open_for("/stderr");
    let output_sandbox = Sandbox::builder()
        .vfs_arc(output_vfs.clone())
        .command("both", |mut ctx| async move {
            ctx.stdout.write_all(b"out\n").await.expect("write stdout");
            ctx.stderr.write_all(b"err\n").await.expect("write stderr");
            CommandResult::success()
        })
        .build();

    let output_failure = output_sandbox.exec("both > /stdout 2> /stderr").await;
    assert_ne!(output_failure.exit_code, 0);
    assert_eq!(output_vfs.live_handles(), 0);
}

#[tokio::test]
async fn shell_field_splitting_redirect_expansion_and_env_persist() {
    // This test intentionally enables legacy session persistence to verify
    // values stored by one exec are available to the next exec.
    let sandbox = Sandbox::builder().persist_session(true).build();

    assert_eq!(sandbox.exec("X=' '").await.exit_code, 0);
    assert_eq!(sandbox.exec("echo \"1\"$X\"2\"").await.stdout, "1 2\n");

    assert_eq!(sandbox.exec("OUT=/var-target").await.exit_code, 0);
    assert_eq!(sandbox.exec("echo redirected > $OUT").await.exit_code, 0);
    assert_eq!(sandbox.exec("cat /var-target").await.stdout, "redirected\n");

    assert_eq!(sandbox.exec("export KEEP=1").await.exit_code, 0);
    assert_eq!(sandbox.exec("echo $KEEP").await.stdout, "1\n");
    assert!(
        sandbox
            .exec("export")
            .await
            .stdout
            .contains("declare -x KEEP=\"1\"")
    );
    assert_eq!(sandbox.exec("unset KEEP").await.exit_code, 0);
    assert_eq!(sandbox.exec("echo x$KEEP").await.stdout, "x\n");
}

#[tokio::test]
async fn cd_updates_session_pwd_and_uses_home() {
    // Persistent mode keeps cwd/PWD updates between exec calls.
    let sandbox = Sandbox::builder()
        .env("HOME", "/home")
        .persist_session(true)
        .build();

    assert_eq!(sandbox.exec("mkdir -p /home /tmp").await.exit_code, 0);
    assert_eq!(sandbox.exec("cd /tmp").await.exit_code, 0);
    assert_eq!(sandbox.exec("echo $PWD").await.stdout, "/tmp\n");
    assert_eq!(sandbox.exec("cd").await.exit_code, 0);
    assert_eq!(
        sandbox.exec("pwd; echo $PWD").await.stdout,
        "/home\n/home\n"
    );

    let no_home = Sandbox::builder().build();
    let result = no_home.exec("cd").await;
    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stderr, "cd: HOME not set\n");

    let after_failed_cd = sandbox.exec("cd /missing; pwd; echo $PWD").await;
    assert_eq!(after_failed_cd.exit_code, 0);
    assert_eq!(after_failed_cd.stdout, "/home\n/home\n");
}

#[tokio::test]
async fn default_execs_discard_session_mutations_but_keep_vfs_changes() {
    // Default sandboxes isolate cwd/env/status per exec, while VFS writes remain
    // shared so files created by one exec are visible to later execs.
    let sandbox = Sandbox::builder().build();

    assert_eq!(sandbox.exec("mkdir /work").await.exit_code, 0);
    assert_eq!(sandbox.exec("cd /work").await.exit_code, 0);
    assert_eq!(sandbox.exec("pwd; echo $PWD").await.stdout, "/\n/\n");

    assert_eq!(sandbox.exec("export FOO=bar").await.exit_code, 0);
    assert_eq!(sandbox.exec("FOO=baz").await.exit_code, 0);
    assert_eq!(sandbox.exec("echo x$FOO").await.stdout, "x\n");

    assert_eq!(sandbox.exec("false").await.exit_code, 1);
    assert_eq!(sandbox.exec("echo status=$?").await.stdout, "status=0\n");

    assert_eq!(sandbox.exec("echo persisted > /file").await.exit_code, 0);
    assert_eq!(sandbox.exec("cat /file").await.stdout, "persisted\n");
}

#[tokio::test]
async fn persist_session_opt_in_keeps_cwd_env_and_status() {
    // Opt-in persistence restores the pre-0.3 session behavior for callers that
    // want one logical shell across multiple exec calls.
    let sandbox = Sandbox::builder().persist_session(true).build();

    assert_eq!(sandbox.exec("mkdir /work && cd /work").await.exit_code, 0);
    assert_eq!(sandbox.exec("pwd").await.stdout, "/work\n");
    assert_eq!(sandbox.exec("export FOO=bar").await.exit_code, 0);
    assert_eq!(sandbox.exec("echo $FOO").await.stdout, "bar\n");
    assert_eq!(sandbox.exec("false").await.exit_code, 1);
    assert_eq!(sandbox.exec("echo status=$?").await.stdout, "status=1\n");
}

#[tokio::test]
async fn wall_clock_timeout_exits_124() {
    // Custom commands participate in the exec-wide timeout budget.
    let limits = Limits {
        wall_time: Duration::from_millis(25),
        ..Limits::default()
    };
    let sandbox = Sandbox::builder()
        .limits(limits)
        .command("nap", |_ctx| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            CommandResult::success()
        })
        .build();

    let result = sandbox.exec("nap").await;
    assert_eq!(result.exit_code, 124);
}

#[tokio::test(flavor = "current_thread")]
async fn slow_vfs_dispatch_uses_blocking_threads() {
    // On a current-thread runtime, inline sleeping VFS calls would serialize
    // concurrent execs; spawn_blocking lets both touch operations overlap.
    let vfs = Arc::new(SleepingVfs::new(Duration::from_millis(75)));
    let sandbox = Arc::new(Sandbox::builder().vfs_arc(vfs).build());

    let start = Instant::now();
    let tasks: Vec<_> = ["/a", "/b", "/c", "/d"]
        .into_iter()
        .map(|path| {
            let sandbox = Arc::clone(&sandbox);
            tokio::spawn(async move { sandbox.exec(&format!("touch {path}")).await })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.expect("touch task").exit_code, 0);
    }

    assert!(start.elapsed() < Duration::from_millis(350));
}

#[derive(Debug)]
struct GoldenCase {
    name: String,
    command: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

fn parse_golden_fixture(input: &str) -> Vec<GoldenCase> {
    let lines: Vec<_> = input.lines().collect();
    let mut cases = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name = line
            .strip_prefix("CASE ")
            .unwrap_or_else(|| panic!("expected CASE, got {line}"))
            .to_owned();
        expect_line(&lines, &mut index, "COMMAND");
        let command = read_block(&lines, &mut index, "END_COMMAND");
        let exit_line = lines[index];
        index += 1;
        let exit_code = exit_line
            .strip_prefix("EXIT ")
            .unwrap_or_else(|| panic!("expected EXIT, got {exit_line}"))
            .parse()
            .expect("valid exit code");
        expect_line(&lines, &mut index, "STDOUT");
        let stdout = read_block(&lines, &mut index, "END_STDOUT");
        expect_line(&lines, &mut index, "STDERR");
        let stderr = read_block(&lines, &mut index, "END_STDERR");
        expect_line(&lines, &mut index, "END");
        cases.push(GoldenCase {
            name,
            command,
            exit_code,
            stdout,
            stderr,
        });
    }
    cases
}

fn expect_line(lines: &[&str], index: &mut usize, expected: &str) {
    let actual = lines
        .get(*index)
        .unwrap_or_else(|| panic!("expected {expected}, got eof"));
    assert_eq!(*actual, expected);
    *index += 1;
}

fn read_block(lines: &[&str], index: &mut usize, terminator: &str) -> String {
    let mut out = String::new();
    while let Some(line) = lines.get(*index) {
        *index += 1;
        if *line == terminator {
            return out;
        }
        out.push_str(line);
        out.push('\n');
    }
    panic!("unterminated block {terminator}");
}

#[derive(Debug, Default)]
struct TrackingVfs {
    inner: InMemoryVfs,
    live_handles: Mutex<BTreeSet<FileHandle>>,
    opens_by_path: Mutex<BTreeMap<String, usize>>,
    fail_second_open_path: Mutex<Option<String>>,
}

impl TrackingVfs {
    fn write_seed(&self, path: &str, data: &[u8]) {
        let handle = self
            .inner
            .open(path, OpenMode::write_only().create_new())
            .expect("create seed file");
        self.inner
            .write_at(handle, 0, data)
            .expect("write seed file");
        self.inner.close(handle).expect("close seed file");
    }

    fn fail_second_open_for(&self, path: &str) {
        *self
            .fail_second_open_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(path.to_owned());
    }

    fn live_handles(&self) -> usize {
        self.live_handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl Vfs for TrackingVfs {
    fn stat(&self, path: &str) -> VfsResult<Metadata> {
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
        let open_count = {
            let mut opens = self
                .opens_by_path
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let count = opens.entry(path.to_owned()).or_default();
            *count += 1;
            *count
        };
        let fail_path = self
            .fail_second_open_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if fail_path.as_deref() == Some(path) && open_count == 2 {
            return Err(tinysandbox::vfs::VfsError::new(
                tinysandbox::vfs::Errno::EACCES,
            ));
        }

        let handle = self.inner.open(path, mode)?;
        self.live_handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(handle);
        Ok(handle)
    }

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.inner.read_at(handle, offset, buf)
    }

    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        self.inner.write_at(handle, offset, data)
    }

    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        self.inner.truncate(handle, len)
    }

    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        let result = self.inner.close(handle);
        if result.is_ok() {
            self.live_handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&handle);
        }
        result
    }

    fn stats(&self) -> Option<VfsResult<tinysandbox::vfs::VfsStats>> {
        Some(self.inner.stats())
    }
}

#[derive(Debug)]
struct SleepingVfs {
    inner: InMemoryVfs,
    delay: Duration,
}

impl SleepingVfs {
    fn new(delay: Duration) -> Self {
        Self {
            inner: InMemoryVfs::default(),
            delay,
        }
    }

    fn sleep(&self) {
        std::thread::sleep(self.delay);
    }
}

impl Vfs for SleepingVfs {
    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        self.sleep();
        self.inner.stat(path)
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        self.sleep();
        self.inner.readdir(path)
    }

    fn mkdir(&self, path: &str) -> VfsResult<()> {
        self.sleep();
        self.inner.mkdir(path)
    }

    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        self.sleep();
        self.inner.rename(from, to)
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        self.sleep();
        self.inner.unlink(path)
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        self.sleep();
        self.inner.rmdir(path)
    }

    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        self.sleep();
        self.inner.open(path, mode)
    }

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.sleep();
        self.inner.read_at(handle, offset, buf)
    }

    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        self.sleep();
        self.inner.write_at(handle, offset, data)
    }

    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        self.sleep();
        self.inner.truncate(handle, len)
    }

    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        self.sleep();
        self.inner.close(handle)
    }

    fn stats(&self) -> Option<VfsResult<tinysandbox::vfs::VfsStats>> {
        Some(self.inner.stats())
    }
}
