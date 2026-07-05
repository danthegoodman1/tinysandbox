//! In-process shell sandbox over a VFS.
//!
//! ```
//! use tinysandbox::sandbox::Sandbox;
//! use tinysandbox::vfs::{InMemoryVfs, VfsQuota};
//!
//! # fn main() {
//! # tokio::runtime::Builder::new_current_thread()
//! #     .enable_time()
//! #     .build()
//! #     .unwrap()
//! #     .block_on(async {
//! let sandbox = Sandbox::builder().vfs(InMemoryVfs::new(VfsQuota::unlimited())).build();
//!
//! let result = sandbox.exec("echo hello").await;
//! assert_eq!(result.exit_code, 0);
//! assert_eq!(result.stdout, "hello\n");
//! #     });
//! # }
//! ```

mod builtins;
pub mod command;
pub mod fs;
#[cfg(feature = "js")]
pub mod syscall;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::{self, Cursor};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use fs::{Fs, STREAM_CHUNK_BYTES, errno_message, normalize_absolute};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::{task, time};

use crate::shell::{
    self, AndOrList, AndOrOp, Command as AstCommand, Pipeline, Redirect, RedirectOp,
    RedirectTarget, Segment, SimpleCommand, Word,
};
use crate::vfs::{Errno, FileType, InMemoryVfs, Metadata, Vfs, VfsError, VfsStats};

pub use command::{
    BoxAsyncRead, BoxAsyncWrite, Command, CommandContext, CommandFuture, CommandResult, Limits,
};
#[cfg(feature = "js")]
pub use syscall::{Syscall, SyscallError, SyscallFuture};

const PIPE_CAPACITY_BYTES: usize = STREAM_CHUNK_BYTES;
const TRUNCATION_MARKER: &[u8] = b"\n[tinysandbox: output truncated]\n";

/// Result of a sandbox `exec` call.
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Captured stdout, possibly truncated according to [`Limits::stdout_bytes`].
    pub stdout: String,
    /// Captured stderr, possibly truncated according to [`Limits::stderr_bytes`].
    pub stderr: String,
    /// Process-like exit code for the executed shell program.
    pub exit_code: i32,
    /// Timing, pipe, truncation, and JS memory metrics.
    pub metrics: ExecMetrics,
}

/// Metrics collected during one sandbox `exec`.
#[derive(Debug, Clone)]
pub struct ExecMetrics {
    /// Total wall-clock time for the exec call.
    pub wall_time: Duration,
    /// Per-command timings in pipeline order; stages in the same pipeline may
    /// overlap, so these durations are not expected to sum to `wall_time`.
    pub commands: Vec<CommandTiming>,
    /// Bytes accepted by each pipeline pipe, in left-to-right pipe order.
    pub pipe_bytes: Vec<usize>,
    /// Whether captured stdout exceeded `Limits::stdout_bytes` while streaming.
    pub stdout_truncated: bool,
    /// Whether captured stderr exceeded `Limits::stderr_bytes` while streaming.
    pub stderr_truncated: bool,
    /// Peak WebAssembly memory reported by JS commands that ran in this exec.
    pub peak_wasm_memory_bytes: Option<usize>,
}

/// Timing for one command stage.
#[derive(Debug, Clone)]
pub struct CommandTiming {
    /// Command name.
    pub name: String,
    /// Wall-clock duration observed for the command stage.
    pub duration: Duration,
    /// Exit code returned by the command stage.
    pub exit_code: i32,
}

/// Aggregate sandbox state useful for observability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxStats {
    /// VFS quota usage, when the configured VFS reports it.
    pub vfs: Option<VfsStats>,
    /// Number of commands run by this sandbox.
    pub commands_run: u64,
}

/// In-process shell sandbox backed by a virtual filesystem.
pub struct Sandbox {
    vfs: Arc<dyn Vfs>,
    commands: Arc<BTreeMap<String, Arc<dyn Command>>>,
    command_names: Arc<BTreeSet<String>>,
    #[cfg(feature = "js")]
    syscalls: Arc<BTreeMap<String, Arc<dyn Syscall>>>,
    #[cfg(feature = "js")]
    js_prelude: Arc<str>,
    limits: Limits,
    session: Mutex<Session>,
    persist_session: bool,
    commands_run: AtomicU64,
}

/// Builder for [`Sandbox`].
pub struct SandboxBuilder {
    vfs: Arc<dyn Vfs>,
    commands: BTreeMap<String, Arc<dyn Command>>,
    #[cfg(feature = "js")]
    syscalls: Vec<(String, Arc<dyn Syscall>)>,
    #[cfg(feature = "js")]
    js_prelude: Option<String>,
    limits: Limits,
    cwd: String,
    env: BTreeMap<String, String>,
    persist_session: bool,
}

impl Sandbox {
    /// Creates a builder with the default in-memory VFS and builtins.
    pub fn builder() -> SandboxBuilder {
        SandboxBuilder::new()
    }

    /// Returns the shared VFS backing this sandbox.
    pub fn vfs(&self) -> Arc<dyn Vfs> {
        Arc::clone(&self.vfs)
    }

    /// Returns an async filesystem facade rooted at the sandbox's base cwd.
    pub fn fs(&self) -> Fs {
        let session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        Fs::new(
            Arc::clone(&self.vfs),
            Arc::clone(&self.command_names),
            session.cwd.clone(),
        )
    }

    /// Returns aggregate sandbox statistics.
    pub fn stats(&self) -> SandboxStats {
        SandboxStats {
            vfs: self.vfs.stats().and_then(Result::ok),
            commands_run: self.commands_run.load(Ordering::Relaxed),
        }
    }

    /// Executes a shell program against this sandbox's VFS and shell session.
    ///
    /// The wall-clock timeout is exec-wide. When it fires, partial stdout,
    /// stderr, metrics, and session mutations are discarded and the result
    /// exits 124. Blocking host calls already running on worker threads are not
    /// cancelled by that timeout, so VFS implementations should keep individual
    /// operations bounded.
    ///
    /// By default, each exec starts from the sandbox's base session: the
    /// builder-configured cwd and environment (defaults: `/`, `PWD=/`). Shell mutations
    /// such as `cd`, `export`, assignments, and `$?` updates are visible within
    /// that exec and discarded afterward, so concurrent default execs have no
    /// session last-writer-wins hazard. Filesystem mutations always persist.
    ///
    /// With [`SandboxBuilder::persist_session`] set to `true`, each exec snapshots the
    /// stored session at start and stores the mutated session at completion; if
    /// multiple execs overlap, the last completed exec wins for session state.
    pub async fn exec(&self, input: &str) -> ExecResult {
        let started = Instant::now();
        let future = self.exec_inner(input);
        match time::timeout(self.limits.wall_time, future).await {
            Ok(mut result) => {
                result.metrics.wall_time = started.elapsed();
                result
            }
            Err(_) => ExecResult {
                stdout: String::new(),
                stderr: "tinysandbox: command timed out\n".to_owned(),
                exit_code: 124,
                metrics: ExecMetrics {
                    wall_time: started.elapsed(),
                    commands: Vec::new(),
                    pipe_bytes: Vec::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    peak_wasm_memory_bytes: None,
                },
            },
        }
    }

    async fn exec_inner(&self, input: &str) -> ExecResult {
        let program = match shell::parse(input) {
            Ok(program) => program,
            Err(err) => {
                return ExecResult {
                    stdout: String::new(),
                    stderr: format!("{err}\n"),
                    exit_code: 2,
                    metrics: ExecMetrics::empty(),
                };
            }
        };

        let mut session = self.session_snapshot();
        let mut exec = ExecState::new(session.last_status, self.limits);
        for list in &program.lists {
            exec.last_status = self.exec_and_or_list(list, &mut session, &mut exec).await;
            if exec.limit_hit {
                break;
            }
        }

        session.last_status = exec.last_status;
        if self.persist_session {
            self.store_session(session);
        }
        self.commands_run.fetch_add(
            u64::try_from(exec.command_count).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );

        let (stdout, stdout_truncated) = exec.stdout.finish();
        let (stderr, stderr_truncated) = exec.stderr.finish();
        ExecResult {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code: exec.last_status,
            metrics: ExecMetrics {
                wall_time: Duration::ZERO,
                commands: exec.timings,
                pipe_bytes: exec.pipe_bytes,
                stdout_truncated,
                stderr_truncated,
                peak_wasm_memory_bytes: exec.peak_wasm_memory_bytes,
            },
        }
    }

    async fn exec_and_or_list(
        &self,
        list: &AndOrList,
        session: &mut Session,
        exec: &mut ExecState,
    ) -> i32 {
        let mut status = self.exec_pipeline(&list.first, session, exec).await;
        for item in &list.rest {
            let should_run = match item.op {
                AndOrOp::And => status == 0,
                AndOrOp::Or => status != 0,
            };
            if should_run {
                status = self.exec_pipeline(&item.pipeline, session, exec).await;
            }
            if exec.limit_hit {
                break;
            }
        }
        status
    }

    async fn exec_pipeline(
        &self,
        pipeline: &Pipeline,
        session: &mut Session,
        exec: &mut ExecState,
    ) -> i32 {
        let remaining = self.limits.max_commands.saturating_sub(exec.command_count);
        let command_cost = pipeline_command_cost(pipeline, session, exec.last_status);
        if command_cost > remaining {
            exec.write_stderr(b"tinysandbox: maximum command count exceeded\n");
            exec.limit_hit = true;
            return 125;
        }

        if pipeline.commands.len() == 1 {
            let AstCommand::Simple(simple) = &pipeline.commands[0];
            return self.exec_single_simple(simple, session, exec).await;
        }

        let mut stages = Vec::new();
        for command in &pipeline.commands {
            let AstCommand::Simple(simple) = command;
            stages.push(self.prepare_stage(simple, session, exec).await);
        }
        exec.command_count += stages.iter().filter(|stage| stage.counts_command).count();
        self.run_pipeline_stages(stages, exec).await
    }

    async fn exec_single_simple(
        &self,
        simple: &SimpleCommand,
        session: &mut Session,
        exec: &mut ExecState,
    ) -> i32 {
        if exec.command_count >= self.limits.max_commands {
            exec.write_stderr(b"tinysandbox: maximum command count exceeded\n");
            exec.limit_hit = true;
            return 125;
        }

        let assignment_values =
            expand_assignments(&simple.assignments, &session.env, exec.last_status);
        let words = expand_words(&simple.words, &session.env, exec.last_status);
        if words.is_empty() {
            for (name, value) in assignment_values {
                session.env.insert(name, value);
            }
            return 0;
        }
        exec.command_count += 1;

        let command_name = words[0].clone();
        let args = words[1..].to_vec();
        let fs = Fs::new(
            Arc::clone(&self.vfs),
            Arc::clone(&self.command_names),
            session.cwd.clone(),
        );
        let mut redirects = match prepare_redirects(simple, &fs, &session.env, exec.last_status)
            .await
        {
            Ok(redirects) => redirects,
            Err((path, err)) => {
                exec.write_stderr(
                    format!("{command_name}: {path}: {}\n", errno_message(err.errno())).as_bytes(),
                );
                return 1;
            }
        };

        let mut command_env = session.env.clone();
        for (name, value) in assignment_values {
            command_env.insert(name, value);
        }
        command_env.insert("?".to_owned(), exec.last_status.to_string());

        let mut special_stdout = Vec::new();
        let mut special_stderr = Vec::new();
        let started = Instant::now();
        let shell_ctx = ShellBuiltinContext {
            session,
            fs: &fs,
            env: &mut command_env,
            stdout: &mut special_stdout,
            stderr: &mut special_stderr,
        };
        if let Some(mut status) = run_shell_builtin_stage(&command_name, &args, shell_ctx).await {
            close_stdin_redirect(&fs, redirects.stdin.take()).await;
            let Some((mut stdout, stdout_sinks, _)) = writer_for_destination_or_report(
                &command_name,
                &fs,
                &redirects.stdout,
                &exec.stdout,
                &exec.stderr,
                None,
            )
            .await
            else {
                return 1;
            };
            let Some((mut stderr, stderr_sinks, _)) = writer_for_destination_or_report(
                &command_name,
                &fs,
                &redirects.stderr,
                &exec.stdout,
                &exec.stderr,
                None,
            )
            .await
            else {
                drop(stdout);
                drain_file_sinks(stdout_sinks).await;
                return 1;
            };
            let _ = stdout.write_all(&special_stdout).await;
            let _ = stderr.write_all(&special_stderr).await;
            drop(stdout);
            drop(stderr);
            for sink in stdout_sinks.into_iter().chain(stderr_sinks) {
                if let Err((path, err)) = await_file_sink(sink).await {
                    exec.write_stderr(
                        format!("{command_name}: {path}: {}\n", errno_message(err.errno()))
                            .as_bytes(),
                    );
                    status = 1;
                }
            }
            exec.timings.push(CommandTiming {
                name: command_name,
                duration: started.elapsed(),
                exit_code: status,
            });
            return status;
        }

        let stdin = match stdin_for(&fs, redirects.stdin.take()).await {
            Ok(stdin) => stdin,
            Err((path, err)) => {
                exec.write_stderr(
                    format!("{command_name}: {path}: {}\n", errno_message(err.errno())).as_bytes(),
                );
                return 1;
            }
        };
        let Some((stdout, stdout_sinks, _)) = writer_for_destination_or_report(
            &command_name,
            &fs,
            &redirects.stdout,
            &exec.stdout,
            &exec.stderr,
            None,
        )
        .await
        else {
            return 1;
        };
        let Some((stderr, stderr_sinks, _)) = writer_for_destination_or_report(
            &command_name,
            &fs,
            &redirects.stderr,
            &exec.stdout,
            &exec.stderr,
            None,
        )
        .await
        else {
            drop(stdout);
            drain_file_sinks(stdout_sinks).await;
            return 1;
        };

        let result = run_registered_stage(
            PreparedStage {
                name: command_name.clone(),
                args,
                env: command_env,
                cwd: session.cwd.clone(),
                fs: fs.clone(),
                command: self.commands.get(&command_name).cloned(),
                shell_builtin: false,
                redirects,
                limits: self.limits,
                commands: Arc::clone(&self.command_names),
                #[cfg(feature = "js")]
                js_syscalls: Arc::clone(&self.syscalls),
                #[cfg(feature = "js")]
                js_prelude: Arc::clone(&self.js_prelude),
                counts_command: true,
                kind: StageKind::Command,
            },
            stdin,
            stdout,
            stderr,
        )
        .await;
        if let Some(bytes) = result.peak_wasm_memory_bytes {
            exec.record_peak_wasm_memory(bytes);
        }
        let mut status = result.exit_code;
        let duration = started.elapsed();

        for sink in stdout_sinks.into_iter().chain(stderr_sinks) {
            if let Err((path, err)) = await_file_sink(sink).await {
                exec.write_stderr(
                    format!("{command_name}: {path}: {}\n", errno_message(err.errno())).as_bytes(),
                );
                status = 1;
            }
        }

        exec.timings.push(CommandTiming {
            name: command_name,
            duration,
            exit_code: status,
        });
        status
    }

    async fn prepare_stage(
        &self,
        simple: &SimpleCommand,
        session: &Session,
        exec: &ExecState,
    ) -> PreparedStage {
        let assignment_values =
            expand_assignments(&simple.assignments, &session.env, exec.last_status);
        let words = expand_words(&simple.words, &session.env, exec.last_status);
        let name = words.first().cloned().unwrap_or_else(|| {
            simple
                .assignments
                .first()
                .map(|assignment| assignment.name.clone())
                .unwrap_or_else(|| "<empty>".to_owned())
        });
        let fs = Fs::new(
            Arc::clone(&self.vfs),
            Arc::clone(&self.command_names),
            session.cwd.clone(),
        );
        let redirects = match prepare_redirects(simple, &fs, &session.env, exec.last_status).await {
            Ok(redirects) => redirects,
            Err((path, err)) => {
                return PreparedStage {
                    name: name.clone(),
                    args: Vec::new(),
                    env: session.env.clone(),
                    cwd: session.cwd.clone(),
                    fs,
                    command: None,
                    shell_builtin: false,
                    redirects: PreparedRedirects::default(),
                    limits: self.limits,
                    commands: Arc::clone(&self.command_names),
                    #[cfg(feature = "js")]
                    js_syscalls: Arc::clone(&self.syscalls),
                    #[cfg(feature = "js")]
                    js_prelude: Arc::clone(&self.js_prelude),
                    counts_command: !words.is_empty(),
                    kind: StageKind::Failed {
                        message: format!("{name}: {path}: {}\n", errno_message(err.errno())),
                    },
                };
            }
        };
        let mut env = session.env.clone();
        for (name, value) in assignment_values {
            env.insert(name, value);
        }
        env.insert("?".to_owned(), exec.last_status.to_string());

        let kind = if words.is_empty() {
            StageKind::AssignmentOnly
        } else {
            StageKind::Command
        };
        let args = words.get(1..).unwrap_or_default().to_vec();
        PreparedStage {
            name: name.clone(),
            args,
            env,
            cwd: session.cwd.clone(),
            fs,
            command: self.commands.get(&name).cloned(),
            shell_builtin: is_shell_builtin_name(&name),
            redirects,
            limits: self.limits,
            commands: Arc::clone(&self.command_names),
            #[cfg(feature = "js")]
            js_syscalls: Arc::clone(&self.syscalls),
            #[cfg(feature = "js")]
            js_prelude: Arc::clone(&self.js_prelude),
            counts_command: matches!(kind, StageKind::Command),
            kind,
        }
    }

    async fn run_pipeline_stages(&self, stages: Vec<PreparedStage>, exec: &mut ExecState) -> i32 {
        if stages.is_empty() {
            return 0;
        }

        let pipe_count = stages.len().saturating_sub(1);
        let mut pipe_readers: Vec<Option<BoxAsyncRead>> = Vec::with_capacity(pipe_count);
        let mut pipe_writers: Vec<Option<PipeDestination>> = Vec::with_capacity(pipe_count);
        let mut pipe_counts = Vec::with_capacity(pipe_count);
        for _ in 0..pipe_count {
            let (reader, writer) = tokio::io::duplex(PIPE_CAPACITY_BYTES);
            let count = Arc::new(AtomicUsize::new(0));
            let broken = Arc::new(AtomicBool::new(false));
            pipe_readers.push(Some(Box::pin(reader)));
            pipe_writers.push(Some(PipeDestination {
                writer: SharedCountingPipeWriter {
                    inner: Arc::new(Mutex::new(writer)),
                    bytes: Arc::clone(&count),
                    broken: Arc::clone(&broken),
                },
                broken,
            }));
            pipe_counts.push(count);
        }

        let total = stages.len();
        let mut tasks = task::JoinSet::new();
        let mut outcomes: Vec<Option<StageOutcome>> = (0..total).map(|_| None).collect();
        for (index, stage) in stages.into_iter().enumerate() {
            let mut stage = stage;
            let input_pipe = if index == 0 {
                None
            } else {
                Some(
                    pipe_readers[index - 1]
                        .take()
                        .expect("pipeline reader is consumed once"),
                )
            };
            let stdin = match if let Some(redirect) = stage.redirects.stdin.take() {
                drop(input_pipe);
                stdin_for(&stage.fs, Some(redirect)).await
            } else if let Some(input_pipe) = input_pipe {
                Ok(input_pipe)
            } else {
                Ok(Box::pin(Cursor::new(Vec::new())) as BoxAsyncRead)
            } {
                Ok(stdin) => stdin,
                Err((path, err)) => {
                    drop(pipe_writers.get_mut(index).and_then(Option::take));
                    outcomes[index] = Some(StageOutcome::failed(
                        index,
                        stage.name,
                        stage.counts_command,
                        format!("{path}: {}\n", errno_message(err.errno())),
                    ));
                    continue;
                }
            };

            let default_pipe = if index + 1 < total {
                pipe_writers[index].take()
            } else {
                None
            };
            let stdout_pipe = default_pipe.clone();
            let stderr_pipe = if matches!(
                stage.redirects.stderr,
                OutputDestination::Capture(CaptureFd::Stdout)
            ) {
                default_pipe
            } else {
                None
            };
            let (stdout, stdout_sinks, stdout_pipe_broken) = match writer_for_destination(
                &stage.fs,
                &stage.redirects.stdout,
                &exec.stdout,
                &exec.stderr,
                stdout_pipe,
            )
            .await
            {
                Ok(writer) => writer,
                Err((path, err)) => {
                    outcomes[index] = Some(StageOutcome::failed(
                        index,
                        stage.name,
                        stage.counts_command,
                        format!("{path}: {}\n", errno_message(err.errno())),
                    ));
                    continue;
                }
            };
            let (stderr, stderr_sinks, stderr_pipe_broken) = match writer_for_destination(
                &stage.fs,
                &stage.redirects.stderr,
                &exec.stdout,
                &exec.stderr,
                stderr_pipe,
            )
            .await
            {
                Ok(writer) => writer,
                Err((path, err)) => {
                    drop(stdout);
                    drain_file_sinks(stdout_sinks).await;
                    outcomes[index] = Some(StageOutcome::failed(
                        index,
                        stage.name,
                        stage.counts_command,
                        format!("{path}: {}\n", errno_message(err.errno())),
                    ));
                    continue;
                }
            };
            let pipe_broken = stdout_pipe_broken.or(stderr_pipe_broken);
            let sinks = stdout_sinks.into_iter().chain(stderr_sinks).collect();

            tasks.spawn(async move {
                run_stage_task(index, stage, stdin, stdout, stderr, sinks, pipe_broken).await
            });
        }

        while let Some(task) = tasks.join_next().await {
            let outcome = match task {
                Ok(outcome) => outcome,
                Err(err) => StageOutcome {
                    index: 0,
                    timing: CommandTiming {
                        name: "<task>".to_owned(),
                        duration: Duration::ZERO,
                        exit_code: 1,
                    },
                    exit_code: 1,
                    peak_wasm_memory_bytes: None,
                    redirect_errors: vec![format!("tinysandbox: command task failed: {err}\n")],
                    counts_command: true,
                },
            };
            let index = outcome.index;
            outcomes[index] = Some(outcome);
        }

        let mut status = 0;
        for outcome in outcomes.into_iter().flatten() {
            for error in &outcome.redirect_errors {
                exec.write_stderr(error.as_bytes());
            }
            if let Some(bytes) = outcome.peak_wasm_memory_bytes {
                exec.record_peak_wasm_memory(bytes);
            }
            status = outcome.exit_code;
            if outcome.counts_command {
                exec.timings.push(outcome.timing);
            }
        }
        exec.pipe_bytes.extend(
            pipe_counts
                .into_iter()
                .map(|count| count.load(Ordering::Relaxed)),
        );
        status
    }

    fn session_snapshot(&self) -> Session {
        self.session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn store_session(&self, session: Session) {
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = session;
    }
}

impl SandboxBuilder {
    fn new() -> Self {
        let mut commands = BTreeMap::new();
        builtins::register(&mut commands);
        #[cfg(feature = "js")]
        crate::js::register(&mut commands);
        commands.insert(
            "cd".to_owned(),
            Arc::new(|_ctx: CommandContext| {
                Box::pin(async { CommandResult::success() }) as CommandFuture
            }),
        );
        commands.insert(
            "export".to_owned(),
            Arc::new(|_ctx: CommandContext| {
                Box::pin(async { CommandResult::success() }) as CommandFuture
            }),
        );
        commands.insert(
            "unset".to_owned(),
            Arc::new(|_ctx: CommandContext| {
                Box::pin(async { CommandResult::success() }) as CommandFuture
            }),
        );

        let mut env = BTreeMap::new();
        env.insert("PWD".to_owned(), "/".to_owned());
        Self {
            vfs: Arc::new(InMemoryVfs::default()),
            commands,
            #[cfg(feature = "js")]
            syscalls: Vec::new(),
            #[cfg(feature = "js")]
            js_prelude: None,
            limits: Limits::default(),
            cwd: "/".to_owned(),
            env,
            persist_session: false,
        }
    }

    /// Replaces the VFS with a concrete implementation.
    pub fn vfs(mut self, vfs: impl Vfs + 'static) -> Self {
        self.vfs = Arc::new(vfs);
        self
    }

    /// Replaces the VFS with a shared trait object.
    pub fn vfs_arc(mut self, vfs: Arc<dyn Vfs>) -> Self {
        self.vfs = vfs;
        self
    }

    /// Registers a custom command by name.
    pub fn command<F, Fut>(mut self, name: impl Into<String>, command: F) -> Self
    where
        F: Fn(CommandContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = CommandResult> + Send + 'static,
    {
        let name = name.into();
        assert_not_reserved(&name);
        self.commands.insert(name, Arc::new(command));
        self
    }

    /// Registers a custom command object by name.
    pub fn command_obj(mut self, name: impl Into<String>, command: impl Command + 'static) -> Self {
        let name = name.into();
        assert_not_reserved(&name);
        self.commands.insert(name, Arc::new(command));
        self
    }

    /// Registers a host syscall callable from sandboxed JavaScript.
    #[cfg(feature = "js")]
    pub fn syscall<F, Fut>(mut self, name: impl Into<String>, syscall: F) -> Self
    where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<serde_json::Value, SyscallError>> + Send + 'static,
    {
        self.syscalls.push((name.into(), Arc::new(syscall)));
        self
    }

    /// Sets JavaScript code evaluated before each sandboxed JavaScript script.
    /// Preludes run before CommonJS globals exist, so define globals instead of using `require`.
    #[cfg(feature = "js")]
    pub fn js_prelude(mut self, code: impl Into<String>) -> Self {
        self.js_prelude = Some(code.into());
        self
    }

    /// Sets resource limits for the sandbox.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets an initial session environment variable.
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Sets the initial session current working directory.
    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = normalize_absolute(cwd.into());
        self.env.insert("PWD".to_owned(), self.cwd.clone());
        self
    }

    /// Enables or disables shell session persistence across exec calls.
    ///
    /// The default is `false`: every exec starts from the builder-provided cwd
    /// and env, with `$?` reset to zero. Set this to `true` to keep `cd`,
    /// assignment/export/unset, and `$?` changes between exec calls.
    pub fn persist_session(mut self, persist: bool) -> Self {
        self.persist_session = persist;
        self
    }

    /// Builds the sandbox.
    pub fn build(self) -> Sandbox {
        #[cfg(feature = "js")]
        let syscalls = Arc::new(build_syscall_registry(self.syscalls));
        #[cfg(feature = "js")]
        let js_prelude = Arc::<str>::from(self.js_prelude.unwrap_or_default());
        let command_names = Arc::new(self.commands.keys().cloned().collect());
        Sandbox {
            vfs: self.vfs,
            commands: Arc::new(self.commands),
            command_names,
            #[cfg(feature = "js")]
            syscalls,
            #[cfg(feature = "js")]
            js_prelude,
            limits: self.limits,
            session: Mutex::new(Session {
                cwd: self.cwd,
                env: self.env,
                last_status: 0,
            }),
            persist_session: self.persist_session,
            commands_run: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Clone)]
struct Session {
    cwd: String,
    env: BTreeMap<String, String>,
    last_status: i32,
}

struct ShellBuiltinContext<'a> {
    session: &'a mut Session,
    fs: &'a Fs,
    env: &'a mut BTreeMap<String, String>,
    stdout: &'a mut Vec<u8>,
    stderr: &'a mut Vec<u8>,
}

async fn run_shell_builtin_stage(
    name: &str,
    args: &[String],
    ctx: ShellBuiltinContext<'_>,
) -> Option<i32> {
    match name {
        "cd" => {
            if args.len() > 1 {
                ctx.stderr.extend_from_slice(b"cd: too many arguments\n");
                return Some(1);
            }
            let target = if let Some(target) = args.first() {
                target.clone()
            } else if let Some(home) = ctx.session.env.get("HOME") {
                home.clone()
            } else {
                ctx.stderr.extend_from_slice(b"cd: HOME not set\n");
                return Some(1);
            };
            let path = ctx.fs.resolve(&target);
            match ctx.fs.stat(&path).await {
                Ok(Metadata {
                    file_type: FileType::Directory,
                    ..
                }) => {
                    let old_pwd = ctx.session.cwd.clone();
                    ctx.session.cwd = path;
                    ctx.session.env.insert("OLDPWD".to_owned(), old_pwd.clone());
                    ctx.session
                        .env
                        .insert("PWD".to_owned(), ctx.session.cwd.clone());
                    ctx.env.insert("OLDPWD".to_owned(), old_pwd);
                    ctx.env.insert("PWD".to_owned(), ctx.session.cwd.clone());
                    Some(0)
                }
                Ok(_) => {
                    ctx.stderr
                        .extend_from_slice(format!("cd: {target}: Not a directory\n").as_bytes());
                    Some(1)
                }
                Err(err) => {
                    ctx.stderr.extend_from_slice(
                        format!("cd: {target}: {}\n", errno_message(err.errno())).as_bytes(),
                    );
                    Some(1)
                }
            }
        }
        "export" => {
            if args.is_empty() {
                for (key, value) in &ctx.session.env {
                    ctx.stdout
                        .extend_from_slice(format!("declare -x {key}=\"{value}\"\n").as_bytes());
                }
                return Some(0);
            }
            for arg in args {
                if let Some((name, value)) = arg.split_once('=') {
                    if is_assignment_name(name) {
                        ctx.session.env.insert(name.to_owned(), value.to_owned());
                        ctx.env.insert(name.to_owned(), value.to_owned());
                    } else {
                        ctx.stderr.extend_from_slice(
                            format!("export: `{arg}': not a valid identifier\n").as_bytes(),
                        );
                        return Some(1);
                    }
                } else if is_assignment_name(arg) {
                    ctx.session.env.entry(arg.clone()).or_default();
                } else {
                    ctx.stderr.extend_from_slice(
                        format!("export: `{arg}': not a valid identifier\n").as_bytes(),
                    );
                    return Some(1);
                }
            }
            Some(0)
        }
        "unset" => {
            for arg in args {
                if is_assignment_name(arg) {
                    ctx.session.env.remove(arg);
                    ctx.env.remove(arg);
                } else {
                    ctx.stderr.extend_from_slice(
                        format!("unset: `{arg}': not a valid identifier\n").as_bytes(),
                    );
                    return Some(1);
                }
            }
            Some(0)
        }
        _ => None,
    }
}

struct ExecState {
    stdout: CaptureWriter,
    stderr: CaptureWriter,
    timings: Vec<CommandTiming>,
    pipe_bytes: Vec<usize>,
    last_status: i32,
    command_count: usize,
    limit_hit: bool,
    peak_wasm_memory_bytes: Option<usize>,
}

impl ExecState {
    fn new(last_status: i32, limits: Limits) -> Self {
        Self {
            stdout: CaptureWriter::new(limits.stdout_bytes),
            stderr: CaptureWriter::new(limits.stderr_bytes),
            timings: Vec::new(),
            pipe_bytes: Vec::new(),
            last_status,
            command_count: 0,
            limit_hit: false,
            peak_wasm_memory_bytes: None,
        }
    }

    fn write_stderr(&self, data: &[u8]) {
        self.stderr.append(data);
    }

    fn record_peak_wasm_memory(&mut self, bytes: usize) {
        self.peak_wasm_memory_bytes = Some(
            self.peak_wasm_memory_bytes
                .map_or(bytes, |current| current.max(bytes)),
        );
    }
}

impl ExecMetrics {
    fn empty() -> Self {
        Self {
            wall_time: Duration::ZERO,
            commands: Vec::new(),
            pipe_bytes: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            peak_wasm_memory_bytes: None,
        }
    }
}

#[derive(Clone)]
struct CaptureWriter {
    inner: Arc<Mutex<CappedOutput>>,
}

impl CaptureWriter {
    fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(CappedOutput::new(cap))),
        }
    }

    fn append(&self, data: &[u8]) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write(data);
    }

    fn boxed(&self) -> BoxAsyncWrite {
        Box::pin(self.clone())
    }

    fn finish(&self) -> (Vec<u8>, bool) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .finish()
    }
}

impl AsyncWrite for CaptureWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.append(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct CappedOutput {
    cap: usize,
    total: usize,
    pre_truncation: Vec<u8>,
    head: Vec<u8>,
    tail: Vec<u8>,
    truncated: bool,
}

impl CappedOutput {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            total: 0,
            pre_truncation: Vec::new(),
            head: Vec::new(),
            tail: Vec::new(),
            truncated: false,
        }
    }

    fn write(&mut self, mut data: &[u8]) {
        if data.is_empty() {
            return;
        }

        if !self.truncated {
            let remaining = self.cap.saturating_sub(self.total);
            if data.len() <= remaining {
                self.pre_truncation.extend_from_slice(data);
                self.total += data.len();
                return;
            }

            self.pre_truncation.extend_from_slice(&data[..remaining]);
            self.total += remaining;
            data = &data[remaining..];
            self.truncated = true;

            if self.cap > TRUNCATION_MARKER.len() {
                let keep = self.cap - TRUNCATION_MARKER.len();
                let head_len = keep / 2;
                let tail_len = keep - head_len;
                self.head.extend_from_slice(
                    &self.pre_truncation[..head_len.min(self.pre_truncation.len())],
                );
                let tail_start = head_len.min(self.pre_truncation.len());
                let preserved = self.pre_truncation[tail_start..].to_vec();
                self.push_tail(&preserved, tail_len);
            }
            self.pre_truncation.clear();
        }

        self.total += data.len();
        if self.cap > TRUNCATION_MARKER.len() {
            let keep = self.cap - TRUNCATION_MARKER.len();
            let tail_len = keep - keep / 2;
            self.push_tail(data, tail_len);
        }
    }

    fn push_tail(&mut self, data: &[u8], limit: usize) {
        if limit == 0 {
            return;
        }
        if data.len() >= limit {
            self.tail.clear();
            self.tail.extend_from_slice(&data[data.len() - limit..]);
            return;
        }
        let overflow = self
            .tail
            .len()
            .saturating_add(data.len())
            .saturating_sub(limit);
        if overflow > 0 {
            self.tail.drain(..overflow);
        }
        self.tail.extend_from_slice(data);
    }

    fn finish(&self) -> (Vec<u8>, bool) {
        if !self.truncated {
            return (self.pre_truncation.clone(), false);
        }
        if self.cap <= TRUNCATION_MARKER.len() {
            return (TRUNCATION_MARKER.to_vec(), true);
        }
        let mut out = Vec::with_capacity(self.cap);
        out.extend_from_slice(&self.head);
        out.extend_from_slice(TRUNCATION_MARKER);
        out.extend_from_slice(&self.tail);
        (out, true)
    }
}

struct PreparedStage {
    name: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    fs: Fs,
    command: Option<Arc<dyn Command>>,
    shell_builtin: bool,
    redirects: PreparedRedirects,
    limits: Limits,
    commands: Arc<BTreeSet<String>>,
    #[cfg(feature = "js")]
    js_syscalls: Arc<BTreeMap<String, Arc<dyn Syscall>>>,
    #[cfg(feature = "js")]
    js_prelude: Arc<str>,
    counts_command: bool,
    kind: StageKind,
}

enum StageKind {
    Command,
    AssignmentOnly,
    Failed { message: String },
}

struct StageOutcome {
    index: usize,
    timing: CommandTiming,
    exit_code: i32,
    peak_wasm_memory_bytes: Option<usize>,
    redirect_errors: Vec<String>,
    counts_command: bool,
}

impl StageOutcome {
    fn failed(index: usize, name: String, counts_command: bool, message: String) -> Self {
        Self {
            index,
            timing: CommandTiming {
                name: name.clone(),
                duration: Duration::ZERO,
                exit_code: 1,
            },
            exit_code: 1,
            peak_wasm_memory_bytes: None,
            redirect_errors: vec![format!("{name}: {message}")],
            counts_command,
        }
    }
}

#[derive(Clone)]
struct PipeDestination {
    writer: SharedCountingPipeWriter,
    broken: Arc<AtomicBool>,
}

#[derive(Clone)]
struct SharedCountingPipeWriter {
    inner: Arc<Mutex<DuplexStream>>,
    bytes: Arc<AtomicUsize>,
    broken: Arc<AtomicBool>,
}

impl AsyncWrite for SharedCountingPipeWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match Pin::new(&mut *inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                self.bytes.fetch_add(n, Ordering::Relaxed);
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(err)) if err.kind() == io::ErrorKind::BrokenPipe => {
                self.broken.store(true, Ordering::Relaxed);
                Poll::Ready(Err(err))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        Pin::new(&mut *inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        Pin::new(&mut *inner).poll_shutdown(cx)
    }
}

struct FileSink {
    handle: Option<task::JoinHandle<Result<(), (String, VfsError)>>>,
}

impl FileSink {
    fn new(handle: task::JoinHandle<Result<(), (String, VfsError)>>) -> Self {
        Self {
            handle: Some(handle),
        }
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

async fn stdin_for(
    fs: &Fs,
    redirected: Option<InputRedirect>,
) -> Result<BoxAsyncRead, (String, VfsError)> {
    if let Some(redirect) = redirected {
        Ok(fs.stream_reader_from_handle(redirect.handle))
    } else {
        Ok(Box::pin(Cursor::new(Vec::new())))
    }
}

async fn close_stdin_redirect(fs: &Fs, redirected: Option<InputRedirect>) {
    if let Some(redirect) = redirected {
        let _ = fs.close(redirect.handle).await;
    }
}

async fn writer_for_destination(
    fs: &Fs,
    destination: &OutputDestination,
    stdout: &CaptureWriter,
    stderr: &CaptureWriter,
    pipe: Option<PipeDestination>,
) -> Result<(BoxAsyncWrite, Vec<FileSink>, Option<Arc<AtomicBool>>), (String, VfsError)> {
    match destination {
        OutputDestination::Capture(CaptureFd::Stdout) => {
            if let Some(pipe) = pipe {
                Ok((Box::pin(pipe.writer), Vec::new(), Some(pipe.broken)))
            } else {
                Ok((stdout.boxed(), Vec::new(), None))
            }
        }
        OutputDestination::Capture(CaptureFd::Stderr) => Ok((stderr.boxed(), Vec::new(), None)),
        OutputDestination::File(target) => {
            let (writer, sink) = file_writer(fs, target).await?;
            Ok((writer, vec![sink], None))
        }
    }
}

async fn writer_for_destination_or_report(
    command_name: &str,
    fs: &Fs,
    destination: &OutputDestination,
    stdout: &CaptureWriter,
    stderr: &CaptureWriter,
    pipe: Option<PipeDestination>,
) -> Option<(BoxAsyncWrite, Vec<FileSink>, Option<Arc<AtomicBool>>)> {
    match writer_for_destination(fs, destination, stdout, stderr, pipe).await {
        Ok(writer) => Some(writer),
        Err((path, err)) => {
            stderr.append(
                format!("{command_name}: {path}: {}\n", errno_message(err.errno())).as_bytes(),
            );
            None
        }
    }
}

async fn file_writer(
    fs: &Fs,
    target: &RedirectFile,
) -> Result<(BoxAsyncWrite, FileSink), (String, VfsError)> {
    let mode = if target.append {
        crate::vfs::OpenMode::write_only().create().append()
    } else {
        crate::vfs::OpenMode::write_only()
    };
    let handle = fs
        .open(&target.path, mode)
        .await
        .map_err(|err| (target.path.clone(), err))?;
    let path = target.path.clone();
    let fs = fs.clone();
    let (mut reader, writer) = tokio::io::duplex(PIPE_CAPACITY_BYTES);
    let sink = FileSink::new(task::spawn(async move {
        let mut offset = 0_u64;
        let mut buf = vec![0; STREAM_CHUNK_BYTES];
        let mut result = Ok(());
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let mut written = 0;
            while written < n {
                match fs.write_at(handle, offset, buf[written..n].to_vec()).await {
                    Ok(0) => {
                        result = Err(VfsError::new(Errno::ENOSPC));
                        break;
                    }
                    Ok(bytes) => {
                        written += bytes;
                        offset = offset.saturating_add(bytes as u64);
                    }
                    Err(err) => {
                        result = Err(err);
                        break;
                    }
                }
            }
            if result.is_err() {
                break;
            }
        }
        let close = fs.close(handle).await;
        match (result, close) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(err), _) | (_, Err(err)) => Err((path, err)),
        }
    }));
    Ok((Box::pin(writer), sink))
}

async fn await_file_sink(mut sink: FileSink) -> Result<(), (String, VfsError)> {
    let handle = sink.handle.take().expect("file sink awaited once");
    handle
        .await
        .unwrap_or_else(|_| Err(("redirect".to_owned(), VfsError::new(Errno::EINVAL))))
}

async fn drain_file_sinks(sinks: Vec<FileSink>) {
    for sink in sinks {
        let _ = await_file_sink(sink).await;
    }
}

async fn run_stage_task(
    index: usize,
    stage: PreparedStage,
    mut stdin: BoxAsyncRead,
    stdout: BoxAsyncWrite,
    stderr: BoxAsyncWrite,
    sinks: Vec<FileSink>,
    pipe_broken: Option<Arc<AtomicBool>>,
) -> StageOutcome {
    let started = Instant::now();
    let name = stage.name.clone();
    let counts_command = stage.counts_command;
    let result = if let StageKind::Failed { message } = &stage.kind {
        let mut stderr = stderr;
        let _ = stderr.write_all(message.as_bytes()).await;
        CommandResult::failure()
    } else if matches!(&stage.kind, StageKind::AssignmentOnly) {
        let _ = tokio::io::copy(&mut stdin, &mut tokio::io::sink()).await;
        CommandResult::success()
    } else {
        run_registered_stage(stage, stdin, stdout, stderr).await
    };
    let mut exit_code = result.exit_code;
    let mut redirect_errors = Vec::new();
    let mut had_redirect_error = false;
    for sink in sinks {
        if let Err((path, err)) = await_file_sink(sink).await {
            redirect_errors.push(format!("{name}: {path}: {}\n", errno_message(err.errno())));
            exit_code = 1;
            had_redirect_error = true;
        }
    }
    if !had_redirect_error
        && exit_code != 0
        && pipe_broken.is_some_and(|broken| broken.load(Ordering::Relaxed))
    {
        exit_code = 141;
    }
    StageOutcome {
        index,
        timing: CommandTiming {
            name,
            duration: started.elapsed(),
            exit_code,
        },
        exit_code,
        peak_wasm_memory_bytes: result.peak_wasm_memory_bytes,
        redirect_errors,
        counts_command,
    }
}

async fn run_registered_stage(
    stage: PreparedStage,
    stdin: BoxAsyncRead,
    mut stdout: BoxAsyncWrite,
    mut stderr: BoxAsyncWrite,
) -> CommandResult {
    if stage.shell_builtin {
        let mut session = Session {
            cwd: stage.cwd.clone(),
            env: stage.env.clone(),
            last_status: stage
                .env
                .get("?")
                .and_then(|status| status.parse().ok())
                .unwrap_or(0),
        };
        let mut env = stage.env.clone();
        let mut special_stdout = Vec::new();
        let mut special_stderr = Vec::new();
        let ctx = ShellBuiltinContext {
            session: &mut session,
            fs: &stage.fs,
            env: &mut env,
            stdout: &mut special_stdout,
            stderr: &mut special_stderr,
        };
        let status = run_shell_builtin_stage(&stage.name, &stage.args, ctx)
            .await
            .unwrap_or(127);
        if stdout.write_all(&special_stdout).await.is_err() {
            return CommandResult::failure();
        }
        if stderr.write_all(&special_stderr).await.is_err() {
            return CommandResult::failure();
        }
        return CommandResult::new(status);
    }

    if let Some(command) = stage.command {
        let ctx = CommandContext {
            args: stage.args,
            env: stage.env,
            cwd: stage.cwd,
            stdin,
            stdout,
            stderr,
            fs: stage.fs,
            limits: stage.limits,
            commands: stage.commands,
            #[cfg(feature = "js")]
            js_syscalls: stage.js_syscalls,
            #[cfg(feature = "js")]
            js_prelude: stage.js_prelude,
        };
        return command.run(ctx).await;
    }

    let _ = stderr
        .write_all(format!("{}: command not found\n", stage.name).as_bytes())
        .await;
    let _ = stdout.shutdown().await;
    CommandResult::new(127)
}

#[derive(Debug, Clone)]
struct PreparedRedirects {
    stdin: Option<InputRedirect>,
    stdout: OutputDestination,
    stderr: OutputDestination,
}

impl Default for PreparedRedirects {
    fn default() -> Self {
        Self {
            stdin: None,
            stdout: OutputDestination::Capture(CaptureFd::Stdout),
            stderr: OutputDestination::Capture(CaptureFd::Stderr),
        }
    }
}

#[derive(Debug, Clone)]
struct InputRedirect {
    path: String,
    handle: crate::vfs::FileHandle,
}

#[derive(Debug, Clone)]
enum OutputDestination {
    Capture(CaptureFd),
    File(RedirectFile),
}

#[derive(Debug, Clone, Copy)]
enum CaptureFd {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
struct RedirectFile {
    path: String,
    append: bool,
}

async fn prepare_redirects(
    simple: &SimpleCommand,
    fs: &Fs,
    env: &BTreeMap<String, String>,
    last_status: i32,
) -> Result<PreparedRedirects, (String, VfsError)> {
    let mut redirects = PreparedRedirects::default();

    for redirect in &simple.redirects {
        match &redirect.target {
            RedirectTarget::Fd(fd) => {
                if let Err(err) = apply_fd_redirect(&mut redirects, redirect, *fd) {
                    close_stdin_redirect(fs, redirects.stdin.take()).await;
                    return Err(err);
                }
            }
            RedirectTarget::Word(word) => {
                let path = match redirect_target(word, env, last_status) {
                    Ok(path) => path,
                    Err(err) => {
                        close_stdin_redirect(fs, redirects.stdin.take()).await;
                        return Err(err);
                    }
                };
                match (
                    redirect.fd.unwrap_or(default_redirect_fd(redirect.op)),
                    redirect.op,
                ) {
                    (0, RedirectOp::Read) => {
                        let handle = match fs.open(&path, crate::vfs::OpenMode::read_only()).await {
                            Ok(handle) => handle,
                            Err(err) => {
                                close_stdin_redirect(fs, redirects.stdin.take()).await;
                                return Err((path.clone(), err));
                            }
                        };
                        if let Some(previous) = redirects.stdin.replace(InputRedirect {
                            path: path.clone(),
                            handle,
                        }) && let Err(err) = fs.close(previous.handle).await
                        {
                            close_stdin_redirect(fs, redirects.stdin.take()).await;
                            return Err((previous.path, err));
                        }
                    }
                    (1, RedirectOp::Write) => {
                        if let Err(err) = preflight_output(fs, &path, false).await {
                            close_stdin_redirect(fs, redirects.stdin.take()).await;
                            return Err(err);
                        }
                        redirects.stdout = OutputDestination::File(RedirectFile {
                            path,
                            append: false,
                        });
                    }
                    (1, RedirectOp::Append) => {
                        if let Err(err) = preflight_output(fs, &path, true).await {
                            close_stdin_redirect(fs, redirects.stdin.take()).await;
                            return Err(err);
                        }
                        redirects.stdout =
                            OutputDestination::File(RedirectFile { path, append: true });
                    }
                    (2, RedirectOp::Write) => {
                        if let Err(err) = preflight_output(fs, &path, false).await {
                            close_stdin_redirect(fs, redirects.stdin.take()).await;
                            return Err(err);
                        }
                        redirects.stderr = OutputDestination::File(RedirectFile {
                            path,
                            append: false,
                        });
                    }
                    (2, RedirectOp::Append) => {
                        if let Err(err) = preflight_output(fs, &path, true).await {
                            close_stdin_redirect(fs, redirects.stdin.take()).await;
                            return Err(err);
                        }
                        redirects.stderr =
                            OutputDestination::File(RedirectFile { path, append: true });
                    }
                    (_, _) => {
                        close_stdin_redirect(fs, redirects.stdin.take()).await;
                        return Err((path, VfsError::new(Errno::EINVAL)));
                    }
                }
            }
        }
    }
    Ok(redirects)
}

async fn preflight_output(fs: &Fs, path: &str, append: bool) -> Result<(), (String, VfsError)> {
    fs.write_file(path, &[], append)
        .await
        .map_err(|err| (path.to_owned(), err))
}

fn apply_fd_redirect(
    redirects: &mut PreparedRedirects,
    redirect: &Redirect,
    target_fd: u32,
) -> Result<(), (String, VfsError)> {
    let fd = redirect.fd.unwrap_or(1);
    if !matches!(fd, 1 | 2) || !matches!(target_fd, 1 | 2) {
        return Err((target_fd.to_string(), VfsError::new(Errno::EINVAL)));
    }
    let mut target = if target_fd == 1 {
        redirects.stdout.clone()
    } else {
        redirects.stderr.clone()
    };
    if let OutputDestination::File(file) = &mut target {
        file.append = true;
        if target_fd == 1 {
            if let OutputDestination::File(stdout) = &mut redirects.stdout {
                stdout.append = true;
            }
        } else if let OutputDestination::File(stderr) = &mut redirects.stderr {
            stderr.append = true;
        }
    }
    if fd == 1 {
        redirects.stdout = target;
    } else {
        redirects.stderr = target;
    }
    Ok(())
}

fn default_redirect_fd(op: RedirectOp) -> u32 {
    match op {
        RedirectOp::Read => 0,
        RedirectOp::Write | RedirectOp::Append => 1,
    }
}

fn redirect_target(
    word: &Word,
    env: &BTreeMap<String, String>,
    last_status: i32,
) -> Result<String, (String, VfsError)> {
    let words = expand_word(word, env, last_status);
    match words.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err((String::new(), VfsError::new(Errno::ENOENT))),
        [first, ..] => Err((first.clone(), VfsError::new(Errno::EINVAL))),
    }
}

fn pipeline_command_cost(pipeline: &Pipeline, session: &Session, last_status: i32) -> usize {
    pipeline
        .commands
        .iter()
        .filter(|command| {
            let AstCommand::Simple(simple) = command;
            !expand_words(&simple.words, &session.env, last_status).is_empty()
        })
        .count()
}

fn is_shell_builtin_name(name: &str) -> bool {
    matches!(name, "cd" | "export" | "unset")
}

fn expand_assignments(
    assignments: &[crate::shell::Assignment],
    env: &BTreeMap<String, String>,
    last_status: i32,
) -> Vec<(String, String)> {
    assignments
        .iter()
        .map(|assignment| {
            (
                assignment.name.clone(),
                expand_assignment_value(&assignment.value, env, last_status),
            )
        })
        .collect()
}

fn expand_words(words: &[Word], env: &BTreeMap<String, String>, last_status: i32) -> Vec<String> {
    words
        .iter()
        .flat_map(|word| expand_word(word, env, last_status))
        .collect()
}

fn expand_word(word: &Word, env: &BTreeMap<String, String>, last_status: i32) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut produced = false;
    for segment in &word.segments {
        match segment {
            Segment::Literal { value, .. } => {
                produced = true;
                fields.last_mut().expect("field exists").push_str(value);
            }
            Segment::Expansion { name, quoted: true } => {
                produced = true;
                fields
                    .last_mut()
                    .expect("field exists")
                    .push_str(&expansion_value(name, env, last_status));
            }
            Segment::Expansion {
                name,
                quoted: false,
            } => {
                let value = expansion_value(name, env, last_status);
                let parts: Vec<_> = value.split_whitespace().collect();
                if parts.is_empty() {
                    if !value.is_empty() && fields.last().is_some_and(|field| !field.is_empty()) {
                        fields.push(String::new());
                    }
                    continue;
                }
                produced = true;
                if value.chars().next().is_some_and(char::is_whitespace)
                    && fields.last().is_some_and(|field| !field.is_empty())
                {
                    fields.push(String::new());
                }
                fields.last_mut().expect("field exists").push_str(parts[0]);
                for part in parts.into_iter().skip(1) {
                    fields.push(part.to_owned());
                }
                if value.chars().last().is_some_and(char::is_whitespace) {
                    fields.push(String::new());
                }
            }
        }
    }
    if !produced {
        return Vec::new();
    }
    while fields.last().is_some_and(String::is_empty) && fields.len() > 1 {
        fields.pop();
    }
    fields
}

fn expand_assignment_value(
    word: &Word,
    env: &BTreeMap<String, String>,
    last_status: i32,
) -> String {
    let mut out = String::new();
    for segment in &word.segments {
        match segment {
            Segment::Literal { value, .. } => out.push_str(value),
            Segment::Expansion { name, .. } => {
                out.push_str(&expansion_value(name, env, last_status))
            }
        }
    }
    out
}

fn expansion_value(name: &str, env: &BTreeMap<String, String>, last_status: i32) -> String {
    if name == "?" {
        last_status.to_string()
    } else {
        env.get(name).cloned().unwrap_or_default()
    }
}

fn assert_not_reserved(name: &str) {
    if matches!(name, "cd" | "export" | "unset") {
        panic!(
            "SandboxBuilder::command cannot register reserved shell builtin '{name}'; cd, export, and unset are interpreted by the shell"
        );
    }
}

#[cfg(feature = "js")]
fn build_syscall_registry(
    entries: Vec<(String, Arc<dyn Syscall>)>,
) -> BTreeMap<String, Arc<dyn Syscall>> {
    let mut syscalls = BTreeMap::new();
    for (name, syscall) in entries {
        if !is_js_syscall_name(&name) {
            panic!(
                "SandboxBuilder::syscall cannot register invalid name '{name}'; names must match [A-Za-z_][A-Za-z0-9_]*"
            );
        }
        if syscalls.insert(name.clone(), syscall).is_some() {
            panic!("SandboxBuilder::syscall cannot register duplicate name '{name}'");
        }
    }
    syscalls
}

#[cfg(feature = "js")]
fn is_js_syscall_name(name: &str) -> bool {
    // JavaScript syscall names use the same identifier shape as shell assignments.
    is_assignment_name(name)
}

fn is_assignment_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}
