use std::sync::Arc;
use std::time::{Duration, Instant};

use thinbox::machine::{CommandResult, Limits, Machine};
use thinbox::vfs::{
    DirEntry, FileHandle, InMemoryVfs, Metadata, OpenMode, Vfs, VfsQuota, VfsResult,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn pipelines_redirects_and_session_state_run_through_machine() {
    // Exercises the shell executor surface: buffered pipes, redirect writes,
    // `&&`/`||`, persistent cwd, and persistent shell env.
    let machine = Machine::builder().build();

    assert_eq!(machine.exec("mkdir -p /workspace").await.exit_code, 0);
    assert_eq!(machine.exec("cd /workspace").await.exit_code, 0);
    assert_eq!(machine.exec("pwd").await.stdout, "/workspace\n");

    assert_eq!(
        machine
            .exec("echo TODO one > notes.txt; echo done >> notes.txt; echo TODO two >> notes.txt")
            .await
            .exit_code,
        0
    );
    let result = machine.exec("cat notes.txt | grep TODO | wc -l").await;
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "2");
    assert_eq!(result.metrics.pipe_bytes.len(), 2);

    assert_eq!(machine.exec("NAME=thinbox").await.exit_code, 0);
    assert_eq!(machine.exec("echo $NAME").await.stdout, "thinbox\n");
    assert_eq!(
        machine.exec("false && echo no || echo yes").await.stdout,
        "yes\n"
    );
    assert_eq!(machine.exec("echo status=$?").await.stdout, "status=0\n");
}

#[tokio::test]
async fn custom_commands_use_same_registry_and_pipelines_as_builtins() {
    // Confirms third-party commands receive stream-shaped stdio and are visible
    // through the synthesized `/bin` registry.
    let machine = Machine::builder()
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

    let result = machine.exec("echo hello | upper").await;
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "HELLO\n");

    let bin = machine.exec("ls /bin | grep upper").await;
    assert_eq!(bin.exit_code, 0);
    assert_eq!(bin.stdout, "upper\n");
}

#[tokio::test]
async fn builtin_text_tools_match_supported_gnu_shapes() {
    // Covers representative supported flags for text builtins without relying
    // on host BSD/GNU tool availability.
    let machine = Machine::builder().build();
    let unsupported = machine.exec("printf unsupported").await;
    assert_eq!(unsupported.exit_code, 127);
    assert!(unsupported.stderr.contains("command not found"));

    assert_eq!(machine.exec("echo -n hi").await.stdout, "hi");
    assert_eq!(
        machine.exec("echo -e 'a\\nb' | grep -n b").await.stdout,
        "2:b\n"
    );
    assert_eq!(
        machine
            .exec("echo -e '3\\n1\\n2\\n2' | sort -n -u")
            .await
            .stdout,
        "1\n2\n3\n"
    );
    assert_eq!(
        machine.exec("echo -e 'a\\na\\nb' | uniq -c").await.stdout,
        "      2 a\n      1 b\n"
    );
    assert_eq!(
        machine.exec("echo cat | sed 's/c/C/'").await.stdout,
        "Cat\n"
    );
    assert_eq!(
        machine.exec("echo -e 'a\\nb\\nc' | tail -n 2").await.stdout,
        "b\nc\n"
    );
    assert_eq!(
        machine.exec("echo -e 'a\\nb\\nc' | head -n 1").await.stdout,
        "a\n"
    );
    assert_eq!(machine.exec("grep -z x").await.exit_code, 2);
    assert_eq!(machine.exec("sort -z").await.exit_code, 2);
}

#[test]
fn reserved_shell_builtins_cannot_be_shadowed() {
    for name in ["cd", "export", "unset"] {
        let result = std::panic::catch_unwind(|| {
            Machine::builder().command(name, |_ctx| async { CommandResult::success() })
        });
        assert!(result.is_err(), "{name} should be reserved");
    }
}

#[tokio::test]
async fn builtin_golden_fixture_matches_gnu_verified_shapes() {
    for case in parse_golden_fixture(include_str!("fixtures/machine_builtins_golden.txt")) {
        let machine = Machine::builder().build();
        let result = machine.exec(&case.command).await;
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
    let machine = Machine::builder().build();

    let listing = machine.exec("ls /bin").await;
    assert_eq!(listing.exit_code, 0);
    assert!(listing.stdout.lines().any(|line| line == "cat"));

    assert_eq!(machine.exec("stat /bin/cat").await.exit_code, 0);
    let denied = machine.exec("echo x > /bin/cat").await;
    assert_ne!(denied.exit_code, 0);
    assert!(denied.stderr.contains("Permission denied"));
}

#[tokio::test]
async fn limits_truncate_output_and_surface_vfs_quota_errors() {
    // Verifies machine-level output capping and that ENOSPC from the VFS
    // reaches stderr as an errno-shaped command failure.
    let limits = Limits {
        stdout_bytes: 24,
        ..Limits::default()
    };
    let machine = Machine::builder().limits(limits).build();
    let result = machine.exec("echo 123456789012345678901234567890").await;
    assert!(result.metrics.stdout_truncated);
    assert!(result.stdout.contains("output truncated"));

    let tiny = Machine::builder()
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
    let limited = Machine::builder()
        .limits(Limits {
            max_commands: 1,
            ..Limits::default()
        })
        .build();
    let limit = limited.exec("true; true").await;
    assert_eq!(limit.exit_code, 125);
    assert!(limit.stderr.contains("maximum command count exceeded"));

    let machine = Machine::builder().build();
    let result = machine.exec("echo one; echo two").await;
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
    assert_eq!(machine.stats().commands_run, 2);
    assert!(machine.stats().vfs.is_some());
}

#[tokio::test]
async fn redirects_follow_bash_fd_order_and_preflight_timing() {
    let machine = Machine::builder()
        .command("both", |mut ctx| async move {
            ctx.stdout.write_all(b"out\n").await.expect("write stdout");
            ctx.stderr.write_all(b"err\n").await.expect("write stderr");
            CommandResult::success()
        })
        .build();

    let split = machine.exec("both 2>&1 > /out").await;
    assert_eq!(split.exit_code, 0);
    assert_eq!(split.stdout, "err\n");
    assert_eq!(machine.exec("cat /out").await.stdout, "out\n");

    let joined = machine.exec("both > /joined 2>&1").await;
    assert_eq!(joined.exit_code, 0);
    assert_eq!(joined.stdout, "");
    assert_eq!(machine.exec("cat /joined").await.stdout, "out\nerr\n");

    assert_eq!(machine.exec("both 2> /err").await.exit_code, 0);
    assert_eq!(machine.exec("both 2>> /err").await.exit_code, 0);
    assert_eq!(machine.exec("cat /err").await.stdout, "err\nerr\n");

    let unsupported_fd = machine.exec("both 3> /bad").await;
    assert_eq!(unsupported_fd.exit_code, 1);
    assert!(unsupported_fd.stderr.contains("Invalid argument"));

    let missing_input = machine.exec("echo ran > /preflight < /missing").await;
    assert_eq!(missing_input.exit_code, 1);
    assert!(missing_input.stderr.contains("No such file or directory"));
    assert_eq!(machine.exec("cat /preflight").await.stdout, "");
}

#[tokio::test]
async fn shell_field_splitting_redirect_expansion_and_env_persist() {
    let machine = Machine::builder().build();

    assert_eq!(machine.exec("X=' '").await.exit_code, 0);
    assert_eq!(machine.exec("echo \"1\"$X\"2\"").await.stdout, "1 2\n");

    assert_eq!(machine.exec("OUT=/var-target").await.exit_code, 0);
    assert_eq!(machine.exec("echo redirected > $OUT").await.exit_code, 0);
    assert_eq!(machine.exec("cat /var-target").await.stdout, "redirected\n");

    assert_eq!(machine.exec("export KEEP=1").await.exit_code, 0);
    assert_eq!(machine.exec("echo $KEEP").await.stdout, "1\n");
    assert!(
        machine
            .exec("export")
            .await
            .stdout
            .contains("declare -x KEEP=\"1\"")
    );
    assert_eq!(machine.exec("unset KEEP").await.exit_code, 0);
    assert_eq!(machine.exec("echo x$KEEP").await.stdout, "x\n");
}

#[tokio::test]
async fn cd_updates_session_pwd_and_uses_home() {
    let machine = Machine::builder().env("HOME", "/home").build();

    assert_eq!(machine.exec("mkdir -p /home /tmp").await.exit_code, 0);
    assert_eq!(machine.exec("cd /tmp").await.exit_code, 0);
    assert_eq!(machine.exec("echo $PWD").await.stdout, "/tmp\n");
    assert_eq!(machine.exec("cd").await.exit_code, 0);
    assert_eq!(
        machine.exec("pwd; echo $PWD").await.stdout,
        "/home\n/home\n"
    );

    let no_home = Machine::builder().build();
    let result = no_home.exec("cd").await;
    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stderr, "cd: HOME not set\n");
}

#[tokio::test]
async fn wall_clock_timeout_exits_124() {
    // Custom commands participate in the exec-wide timeout budget.
    let limits = Limits {
        wall_time: Duration::from_millis(25),
        ..Limits::default()
    };
    let machine = Machine::builder()
        .limits(limits)
        .command("nap", |_ctx| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            CommandResult::success()
        })
        .build();

    let result = machine.exec("nap").await;
    assert_eq!(result.exit_code, 124);
}

#[tokio::test(flavor = "current_thread")]
async fn slow_vfs_dispatch_uses_blocking_threads() {
    // On a current-thread runtime, inline sleeping VFS calls would serialize
    // concurrent execs; spawn_blocking lets both touch operations overlap.
    let vfs = Arc::new(SleepingVfs::new(Duration::from_millis(75)));
    let machine = Arc::new(Machine::builder().vfs_arc(vfs).build());

    let start = Instant::now();
    let tasks: Vec<_> = ["/a", "/b", "/c", "/d"]
        .into_iter()
        .map(|path| {
            let machine = Arc::clone(&machine);
            tokio::spawn(async move { machine.exec(&format!("touch {path}")).await })
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

    fn stats(&self) -> Option<VfsResult<thinbox::vfs::VfsStats>> {
        Some(self.inner.stats())
    }
}
