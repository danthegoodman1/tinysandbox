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
//! let sandbox = Sandbox::builder().mount("workspace", InMemoryVfs::new(VfsQuota::unlimited())).build();
//!
//! let result = sandbox.exec("echo hello").await;
//! assert_eq!(result.exit_code, 0);
//! assert_eq!(result.stdout, "hello\n");
//! #     });
//! # }
//! ```

mod builtins;
pub mod command;
mod control;
pub mod fs;
#[cfg(feature = "js")]
pub mod host;
mod jq;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::io::{self, Cursor};
use std::pin::Pin;
#[cfg(feature = "js")]
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use control::{ExecutionControl, ExecutionGuard};
use fs::{Fs, STREAM_CHUNK_BYTES, errno_message, normalize_absolute};
use tokio::io::{AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::{task, time};

use crate::shell::{
    self, AndOrList, AndOrOp, Command as AstCommand, Pipeline, Redirect, RedirectOp,
    RedirectTarget, Segment, SimpleCommand, Word,
};
use crate::vfs::mount::validate_mount_name;
use crate::vfs::{Errno, FileType, InMemoryVfs, Metadata, MountedVfs, Vfs, VfsError, VfsStats};

pub use command::{
    BoxAsyncRead, BoxAsyncWrite, Command, CommandContext, CommandFuture, CommandResult, Limits,
};
#[cfg(feature = "js")]
pub use host::{
    Fetch, FetchFuture, FetchRequest, FetchResponse, HostError, JsGlobal, JsGlobalError,
    JsGlobalFuture, JsGlobals,
};

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
    host_fs: Fs,
    vfs: Arc<dyn Vfs>,
    commands: Arc<BTreeMap<String, Arc<dyn Command>>>,
    command_names: Arc<BTreeSet<String>>,
    #[cfg(feature = "js")]
    js_globals: RwLock<Arc<BTreeMap<String, Arc<dyn JsGlobal>>>>,
    #[cfg(feature = "js")]
    fetch: Option<Arc<dyn Fetch>>,
    #[cfg(feature = "js")]
    js_prelude: Arc<str>,
    limits: Limits,
    session: Mutex<Session>,
    persist_session: bool,
    commands_run: AtomicU64,
}

/// Builder for [`Sandbox`].
pub struct SandboxBuilder {
    mounts: BTreeMap<String, Arc<dyn Vfs>>,
    commands: BTreeMap<String, Arc<dyn Command>>,
    #[cfg(feature = "js")]
    js_globals: Vec<(String, Arc<dyn JsGlobal>)>,
    #[cfg(feature = "js")]
    fetch: Option<Arc<dyn Fetch>>,
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
        self.host_fs.with_cwd(session.cwd.clone())
    }

    /// Binds a host function at a dotted global path, replacing any global
    /// already registered under that exact name.
    ///
    /// The change is visible to `js` commands whose registry snapshot is taken
    /// after this returns. A command already running keeps the set it started
    /// with, so a script never sees a global appear or vanish mid-run.
    #[cfg(feature = "js")]
    pub fn set_js_global<F, Fut>(
        &self,
        name: impl Into<String>,
        global: F,
    ) -> Result<(), JsGlobalError>
    where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<serde_json::Value, HostError>> + Send + 'static,
    {
        let entries = vec![(name.into(), Arc::new(global) as Arc<dyn JsGlobal>)];
        self.mutate_js_globals(|current| merge_js_globals(current, entries))
    }

    /// Adds a set of host globals to the ones already bound, replacing any that
    /// share an exact name and leaving the rest untouched.
    ///
    /// The merged surface is validated before it lands, so a set that conflicts
    /// with a bound namespace leaves the live globals unchanged. Use this to add
    /// several names as one swap; use [`Sandbox::replace_js_globals`] when the
    /// old surface must go away.
    #[cfg(feature = "js")]
    pub fn extend_js_globals(&self, globals: JsGlobals) -> Result<(), JsGlobalError> {
        self.mutate_js_globals(|current| merge_js_globals(current, globals.entries))
    }

    /// Removes a host global, reporting whether it was registered.
    ///
    /// Commands already running keep calling it; the removal applies to
    /// snapshots taken afterwards. A handler mid-flight is not cancelled.
    #[cfg(feature = "js")]
    pub fn remove_js_global(&self, name: &str) -> bool {
        let mut guard = self
            .js_globals
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if !guard.contains_key(name) {
            return false;
        }
        let mut globals = guard.as_ref().clone();
        globals.remove(name);
        *guard = Arc::new(globals);
        true
    }

    /// Replaces every host global with the given set in one swap.
    ///
    /// Globals registered on the builder go away too: the live surface becomes
    /// exactly this set. It is validated as a whole first, so a rejected set
    /// leaves the sandbox untouched rather than landing halfway. This is the
    /// call for a per-turn tool surface, where anything not granted again should
    /// stop being callable.
    #[cfg(feature = "js")]
    pub fn replace_js_globals(&self, globals: JsGlobals) -> Result<(), JsGlobalError> {
        self.mutate_js_globals(|_| merge_js_globals(BTreeMap::new(), globals.entries))
    }

    /// Applies a validated change to the registry while holding the write lock,
    /// so concurrent mutations cannot lose an update.
    #[cfg(feature = "js")]
    fn mutate_js_globals<F>(&self, change: F) -> Result<(), JsGlobalError>
    where
        F: FnOnce(
            BTreeMap<String, Arc<dyn JsGlobal>>,
        ) -> Result<BTreeMap<String, Arc<dyn JsGlobal>>, JsGlobalError>,
    {
        let mut guard = self
            .js_globals
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let next = change(guard.as_ref().clone())?;
        *guard = Arc::new(next);
        Ok(())
    }

    /// Returns the names currently bound as host globals, in sorted order.
    #[cfg(feature = "js")]
    pub fn js_global_names(&self) -> Vec<String> {
        self.js_globals_snapshot().keys().cloned().collect()
    }

    #[cfg(feature = "js")]
    fn js_globals_snapshot(&self) -> Arc<BTreeMap<String, Arc<dyn JsGlobal>>> {
        Arc::clone(
            &self
                .js_globals
                .read()
                .unwrap_or_else(PoisonError::into_inner),
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
    /// preempted by that timeout. Their admission slots remain held until they
    /// finish; new filesystem calls are rejected and remaining handles are
    /// aborted. Slow cleanup can outlive the result. VFS implementations and
    /// trusted callbacks must keep individual operations bounded.
    ///
    /// By default, each exec starts from the sandbox's base session: the
    /// builder-configured cwd and environment (defaults: `/workspace`,
    /// `PWD=/workspace`). Shell mutations
    /// such as `cd`, `export`, assignments, and `$?` updates are visible within
    /// that exec and discarded afterward, so concurrent default execs have no
    /// session last-writer-wins hazard. Filesystem mutations always persist.
    ///
    /// With [`SandboxBuilder::persist_session`] set to `true`, each exec snapshots the
    /// stored session at start and stores the mutated session at completion; if
    /// multiple execs overlap, the last completed exec wins for session state.
    pub async fn exec(&self, input: &str) -> ExecResult {
        let started = Instant::now();
        let control = ExecutionControl::new(self.limits);
        let _guard = ExecutionGuard(Arc::clone(&control));
        let future = self.exec_inner(input, Arc::clone(&control));
        match time::timeout(control.remaining(), future).await {
            Ok(mut result) if !control.is_cancelled() => {
                result.metrics.wall_time = started.elapsed();
                result
            }
            _ => {
                control.cancel();
                ExecResult {
                    stdout: String::new(),
                    stderr: "tinysandbox: command timed out\n".to_owned(),
                    exit_code: 124,
                    metrics: ExecMetrics {
                        wall_time: started.elapsed(),
                        ..ExecMetrics::empty()
                    },
                }
            }
        }
    }

    async fn exec_inner(&self, input: &str, control: Arc<ExecutionControl>) -> ExecResult {
        if input.len() > self.limits.shell_input_bytes {
            return ExecResult {
                stdout: String::new(),
                stderr: "tinysandbox: shell input limit exceeded\n".into(),
                exit_code: 125,
                metrics: ExecMetrics::empty(),
            };
        }
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
        let mut exec = ExecState::new(session.last_status, self.limits, control);
        for list in &program.lists {
            task::yield_now().await;
            if exec.control.is_cancelled() {
                break;
            }
            exec.last_status = self.exec_and_or_list(list, &mut session, &mut exec).await;
            if exec.limit_hit {
                break;
            }
        }

        session.last_status = exec.last_status;
        if self.persist_session && !exec.control.is_cancelled() {
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
        exec.last_status = status;
        for item in &list.rest {
            let should_run = match item.op {
                AndOrOp::And => status == 0,
                AndOrOp::Or => status != 0,
            };
            if should_run {
                status = self.exec_pipeline(&item.pipeline, session, exec).await;
                exec.last_status = status;
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
        task::yield_now().await;
        if exec.control.is_cancelled() {
            return 124;
        }
        let remaining = self.limits.max_commands.saturating_sub(exec.command_count);
        let command_cost = pipeline.commands.len();
        if command_cost > remaining {
            exec.write_stderr(b"tinysandbox: maximum command count exceeded\n");
            exec.limit_hit = true;
            return 125;
        }

        // Admit the entire pipeline before allocating expanded argv, opening
        // redirects, or creating pipes. Include field-vector storage as well as
        // payload bytes so whitespace expansion cannot bypass the budget.
        let mut expansion_remaining = self
            .limits
            .shell_input_bytes
            .max(self.limits.host_input_bytes);
        for command in &pipeline.commands {
            task::yield_now().await;
            if exec.control.is_cancelled() {
                return 124;
            }
            let AstCommand::Simple(simple) = command;
            if let Some(cost) =
                expansion_cost(simple, &session.env, exec.last_status, expansion_remaining)
            {
                expansion_remaining -= cost;
            } else {
                exec.write_stderr(b"tinysandbox: shell expansion limit exceeded\n");
                exec.limit_hit = true;
                return 125;
            }
        }

        if pipeline.commands.len() == 1 {
            let AstCommand::Simple(simple) = &pipeline.commands[0];
            return self.exec_single_simple(simple, session, exec).await;
        }

        let mut stages = Vec::new();
        for command in &pipeline.commands {
            task::yield_now().await;
            if exec.control.is_cancelled() {
                return 124;
            }
            let AstCommand::Simple(simple) = command;
            stages.push(self.prepare_stage(simple, session, exec).await);
        }
        exec.command_count += stages.len();
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
        exec.command_count += 1;

        // Shell assignments in a null command persist even if its redirect fails.
        if words.is_empty() {
            for (name, value) in &assignment_values {
                session.env.insert(name.clone(), value.clone());
            }
        }
        let command_name = words
            .first()
            .cloned()
            .unwrap_or_else(|| "tinysandbox".into());
        let args = words.get(1..).unwrap_or_default().to_vec();
        let fs = Fs::scoped(
            Arc::clone(&self.vfs),
            Arc::clone(&self.command_names),
            session.cwd.clone(),
            Some(Arc::clone(&exec.control)),
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

        if words.is_empty() {
            close_stdin_redirect(&fs, redirects.stdin.take()).await;
            if let Err((path, err)) = finish_redirects(&redirects).await {
                exec.write_stderr(
                    format!("tinysandbox: {path}: {}\n", errno_message(err.errno())).as_bytes(),
                );
                return 1;
            }
            for (name, value) in assignment_values {
                session.env.insert(name, value);
            }
            return 0;
        }

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
        if let Some(mut status) = if self.commands.contains_key(&command_name) {
            run_shell_builtin_stage(&command_name, &args, shell_ctx).await
        } else {
            None
        } {
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
                js_globals: self.js_globals_snapshot(),
                #[cfg(feature = "js")]
                js_fetch: self.fetch.clone(),
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
        let fs = Fs::scoped(
            Arc::clone(&self.vfs),
            Arc::clone(&self.command_names),
            session.cwd.clone(),
            Some(Arc::clone(&exec.control)),
        );
        let mut env = session.env.clone();
        for (name, value) in assignment_values {
            env.insert(name, value);
        }
        env.insert("?".to_owned(), exec.last_status.to_string());
        let redirect_env = if words.is_empty() { &env } else { &session.env };
        let redirects = match prepare_redirects(simple, &fs, redirect_env, exec.last_status).await {
            Ok(redirects) => redirects,
            Err((path, err)) => {
                return PreparedStage {
                    name: name.clone(),
                    args: Vec::new(),
                    env,
                    cwd: session.cwd.clone(),
                    fs,
                    command: None,
                    shell_builtin: false,
                    redirects: PreparedRedirects::default(),
                    limits: self.limits,
                    commands: Arc::clone(&self.command_names),
                    #[cfg(feature = "js")]
                    js_globals: self.js_globals_snapshot(),
                    #[cfg(feature = "js")]
                    js_fetch: self.fetch.clone(),
                    #[cfg(feature = "js")]
                    js_prelude: Arc::clone(&self.js_prelude),
                    counts_command: !words.is_empty(),
                    kind: StageKind::Failed {
                        message: format!("{name}: {path}: {}\n", errno_message(err.errno())),
                    },
                };
            }
        };

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
            shell_builtin: self.commands.contains_key(&name) && is_shell_builtin_name(&name),
            redirects,
            limits: self.limits,
            commands: Arc::clone(&self.command_names),
            #[cfg(feature = "js")]
            js_globals: self.js_globals_snapshot(),
            #[cfg(feature = "js")]
            js_fetch: self.fetch.clone(),
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
                    inner: Some(Arc::new(Mutex::new(writer))),
                    wake: Arc::new(PipeWake {
                        waiters: Mutex::new(BTreeMap::new()),
                        next_id: AtomicUsize::new(1),
                    }),
                    id: 0,
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
        env.insert("PWD".to_owned(), "/workspace".to_owned());
        let mut mounts = BTreeMap::new();
        mounts.insert(
            "workspace".to_owned(),
            Arc::new(InMemoryVfs::default()) as Arc<dyn Vfs>,
        );
        Self {
            mounts,
            commands,
            #[cfg(feature = "js")]
            js_globals: Vec::new(),
            #[cfg(feature = "js")]
            fetch: None,
            #[cfg(feature = "js")]
            js_prelude: None,
            limits: Limits::default(),
            cwd: "/workspace".to_owned(),
            env,
            persist_session: false,
        }
    }

    /// Adds or replaces a top-level mount with a concrete VFS implementation.
    pub fn mount(mut self, name: impl Into<String>, vfs: impl Vfs + 'static) -> Self {
        let name = name.into();
        assert!(
            validate_mount_name(&name).is_ok(),
            "SandboxBuilder::mount requires a non-reserved single path component"
        );
        self.mounts.insert(name, Arc::new(vfs));
        self
    }

    /// Adds or replaces a top-level mount with a shared VFS trait object.
    pub fn mount_arc(mut self, name: impl Into<String>, vfs: Arc<dyn Vfs>) -> Self {
        let name = name.into();
        assert!(
            validate_mount_name(&name).is_ok(),
            "SandboxBuilder::mount_arc requires a non-reserved single path component"
        );
        self.mounts.insert(name, vfs);
        self
    }

    /// Removes all configured mounts.
    pub fn clear_mounts(mut self) -> Self {
        self.mounts.clear();
        self.cwd = "/".to_owned();
        self.env.insert("PWD".to_owned(), self.cwd.clone());
        self
    }

    /// Removes a command, including a default builtin, from lookup and `/bin`.
    /// For example, use `without_command("jq")` when native evaluator work is
    /// outside the host's resource policy.
    pub fn without_command(mut self, name: &str) -> Self {
        self.commands.remove(name);
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

    /// Binds a host function into the sandboxed JavaScript global scope.
    ///
    /// The name is a dotted path: `search` becomes `globalThis.search`, and
    /// `tools.search` becomes `globalThis.tools.search` with the `tools`
    /// namespace created for you. Names may not shadow a global the runtime
    /// installs itself.
    #[cfg(feature = "js")]
    pub fn js_global<F, Fut>(mut self, name: impl Into<String>, global: F) -> Self
    where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<serde_json::Value, HostError>> + Send + 'static,
    {
        self.js_globals.push((name.into(), Arc::new(global)));
        self
    }

    /// Registers the host transport backing sandboxed JavaScript `fetch`.
    #[cfg(feature = "js")]
    pub fn fetch<F, Fut>(mut self, fetch: F) -> Self
    where
        F: Fn(FetchRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<FetchResponse, HostError>> + Send + 'static,
    {
        self.fetch = Some(Arc::new(fetch));
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
        let js_globals = Arc::new(build_js_global_registry(self.js_globals));
        #[cfg(feature = "js")]
        let js_prelude = Arc::<str>::from(self.js_prelude.unwrap_or_default());
        let command_names = Arc::new(self.commands.keys().cloned().collect());
        let vfs: Arc<dyn Vfs> = Arc::new(
            MountedVfs::new(self.mounts).expect("SandboxBuilder validates mount names eagerly"),
        );
        let host_fs = Fs::new(
            Arc::clone(&vfs),
            Arc::clone(&command_names),
            self.cwd.clone(),
        );
        Sandbox {
            host_fs,
            vfs,
            commands: Arc::new(self.commands),
            command_names,
            #[cfg(feature = "js")]
            js_globals: RwLock::new(js_globals),
            #[cfg(feature = "js")]
            fetch: self.fetch,
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
    control: Arc<ExecutionControl>,
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
    fn new(last_status: i32, limits: Limits, control: Arc<ExecutionControl>) -> Self {
        Self {
            control,
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
    tail: VecDeque<u8>,
    truncated: bool,
}

impl CappedOutput {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            total: 0,
            pre_truncation: Vec::new(),
            head: Vec::new(),
            tail: VecDeque::new(),
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
            self.tail.extend(&data[data.len() - limit..]);
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
        self.tail.extend(data);
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
        out.extend(self.tail.iter());
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
    js_globals: Arc<BTreeMap<String, Arc<dyn JsGlobal>>>,
    #[cfg(feature = "js")]
    js_fetch: Option<Arc<dyn Fetch>>,
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

struct SharedCountingPipeWriter {
    inner: Option<Arc<Mutex<DuplexStream>>>,
    wake: Arc<PipeWake>,
    id: usize,
    bytes: Arc<AtomicUsize>,
    broken: Arc<AtomicBool>,
}

// DuplexStream has one write waker. Duplicated descriptors may be polled by
// different tasks, so its readiness must wake every waiting descriptor.
struct PipeWake {
    waiters: Mutex<BTreeMap<usize, Waker>>,
    next_id: AtomicUsize,
}
impl Wake for PipeWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        let waiters =
            std::mem::take(&mut *self.waiters.lock().unwrap_or_else(PoisonError::into_inner));
        for waker in waiters.into_values() {
            waker.wake();
        }
    }
}
impl Clone for SharedCountingPipeWriter {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            bytes: Arc::clone(&self.bytes),
            broken: Arc::clone(&self.broken),
            wake: Arc::clone(&self.wake),
            id: self.wake.next_id.fetch_add(1, Ordering::Relaxed),
        }
    }
}
impl Drop for SharedCountingPipeWriter {
    fn drop(&mut self) {
        self.wake
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl AsyncWrite for SharedCountingPipeWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let Some(pipe) = &self.inner else {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
        };
        self.wake
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.id, cx.waker().clone());
        let waker = Waker::from(Arc::clone(&self.wake));
        let mut shared_cx = Context::from_waker(&waker);
        let mut inner = pipe.lock().unwrap_or_else(PoisonError::into_inner);
        let result = Pin::new(&mut *inner).poll_write(&mut shared_cx, buf);
        if result.is_ready() {
            self.wake
                .waiters
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.id);
        }
        match result {
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

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Duplex writes are accepted directly into the bounded pipe buffer.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.wake
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&this.id);
        this.inner = None;
        Poll::Ready(Ok(()))
    }
}

struct FileSink(Arc<RedirectFile>);

// Duplication shares one description and offset. Accept at most one owned
// chunk per destination; pending lock acquisition owns no caller bytes, so a
// cancelled write can safely be followed by a write of a different buffer.
type RedirectLock =
    Pin<Box<dyn Future<Output = tokio::sync::OwnedMutexGuard<RedirectState>> + Send>>;
struct RedirectWriter {
    target: Arc<RedirectFile>,
    pending: Option<RedirectLock>,
}
impl RedirectWriter {
    fn poll_lock(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<tokio::sync::OwnedMutexGuard<RedirectState>> {
        if self.pending.is_none() {
            self.pending = Some(Box::pin(Arc::clone(&self.target.state).lock_owned()));
        }
        let result = self
            .pending
            .as_mut()
            .expect("lock installed")
            .as_mut()
            .poll(cx);
        if result.is_ready() {
            self.pending = None;
        }
        result
    }
}
impl AsyncWrite for RedirectWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let mut state = match this.poll_lock(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(state) => state,
        };
        if let Some(err) = state.error {
            return Poll::Ready(Err(io::Error::other(err)));
        }
        let Some(handle) = state.handle else {
            return Poll::Ready(Err(io::Error::other(VfsError::new(Errno::EBADF))));
        };
        let n = buf.len().min(STREAM_CHUNK_BYTES);
        if n == 0 {
            return Poll::Ready(Ok(0));
        }
        let data = buf[..n].to_vec();
        let fs = this.target.fs.clone();
        // Retain an error if the task panics or is discarded during runtime
        // shutdown. Completion clears it only after every byte is written.
        state.error = Some(VfsError::new(Errno::EIO));
        task::spawn(async move {
            let mut written = 0;
            while written < data.len() {
                match fs
                    .write_at(handle, state.offset, data[written..].to_vec())
                    .await
                {
                    Ok(0) => {
                        state.error = Some(VfsError::new(Errno::ENOSPC));
                        return;
                    }
                    Ok(bytes) => {
                        written += bytes;
                        state.offset = state.offset.saturating_add(bytes as u64);
                    }
                    Err(err) => {
                        state.error = Some(err);
                        return;
                    }
                }
                if let Err(err) = fs.checkpoint().await {
                    state.error = Some(err);
                    return;
                }
            }
            state.error = None;
        });
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().poll_lock(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(state) => {
                Poll::Ready(state.error.map_or(Ok(()), |err| Err(io::Error::other(err))))
            }
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
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
    _fs: &Fs,
    target: &Arc<RedirectFile>,
) -> Result<(BoxAsyncWrite, FileSink), (String, VfsError)> {
    Ok((
        Box::pin(RedirectWriter {
            target: Arc::clone(target),
            pending: None,
        }),
        FileSink(Arc::clone(target)),
    ))
}

async fn await_file_sink(sink: FileSink) -> Result<(), (String, VfsError)> {
    sink.0.finish().await
}

async fn drain_file_sinks(sinks: Vec<FileSink>) {
    for sink in sinks {
        let _ = await_file_sink(sink).await;
    }
}

async fn run_stage_task(
    index: usize,
    stage: PreparedStage,
    stdin: BoxAsyncRead,
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
        drop(stdin);
        drop(stdout);
        drop(stderr);
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
            js_globals: stage.js_globals,
            #[cfg(feature = "js")]
            js_fetch: stage.js_fetch,
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

#[derive(Clone)]
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

#[derive(Clone)]
enum OutputDestination {
    Capture(CaptureFd),
    File(Arc<RedirectFile>),
}

#[derive(Debug, Clone, Copy)]
enum CaptureFd {
    Stdout,
    Stderr,
}

struct RedirectFile {
    path: String,
    fs: Fs,
    state: Arc<tokio::sync::Mutex<RedirectState>>,
}
struct RedirectState {
    handle: Option<crate::vfs::FileHandle>,
    offset: u64,
    error: Option<VfsError>,
}
impl RedirectFile {
    async fn finish(&self) -> Result<(), (String, VfsError)> {
        let mut state = self.state.lock().await;
        if let Some(handle) = state.handle.take() {
            let result = if state.error.is_some() || self.fs.is_cancelled() {
                self.fs.abort(handle).await
            } else {
                self.fs.close(handle).await
            };
            if state.error.is_none() {
                state.error = result.err();
            }
        }
        state
            .error
            .map_or(Ok(()), |err| Err((self.path.clone(), err)))
    }
}

async fn finish_redirects(redirects: &PreparedRedirects) -> Result<(), (String, VfsError)> {
    let mut result = Ok(());
    for destination in [&redirects.stdout, &redirects.stderr] {
        if let OutputDestination::File(file) = destination {
            let close = file.finish().await;
            if result.is_ok() {
                result = close;
            }
        }
    }
    result
}

async fn prepare_redirects(
    simple: &SimpleCommand,
    fs: &Fs,
    env: &BTreeMap<String, String>,
    last_status: i32,
) -> Result<PreparedRedirects, (String, VfsError)> {
    let mut redirects = PreparedRedirects::default();
    let mut opened: Vec<Arc<RedirectFile>> = Vec::new();
    let result = async {
        for redirect in &simple.redirects {
            fs.checkpoint()
                .await
                .map_err(|err| ("redirect".into(), err))?;
            match &redirect.target {
                RedirectTarget::Fd(fd) => apply_fd_redirect(&mut redirects, redirect, *fd)?,
                RedirectTarget::Word(word) => {
                    let path = redirect_target(word, env, last_status)?;
                    match (
                        redirect.fd.unwrap_or(default_redirect_fd(redirect.op)),
                        redirect.op,
                    ) {
                        (0, RedirectOp::Read) => {
                            let handle = fs
                                .open(&path, crate::vfs::OpenMode::read_only())
                                .await
                                .map_err(|err| (path.clone(), err))?;
                            if let Some(previous) = redirects.stdin.replace(InputRedirect {
                                path: path.clone(),
                                handle,
                            }) {
                                fs.close(previous.handle)
                                    .await
                                    .map_err(|err| (previous.path, err))?;
                            }
                        }
                        (fd @ (1 | 2), op @ (RedirectOp::Write | RedirectOp::Append)) => {
                            let mode = if op == RedirectOp::Append {
                                crate::vfs::OpenMode::write_only().create().append()
                            } else {
                                crate::vfs::OpenMode::write_only().create().truncate()
                            };
                            let handle = fs
                                .open(&path, mode)
                                .await
                                .map_err(|err| (path.clone(), err))?;
                            let target = Arc::new(RedirectFile {
                                path,
                                fs: fs.clone(),
                                state: Arc::new(tokio::sync::Mutex::new(RedirectState {
                                    handle: Some(handle),
                                    offset: 0,
                                    error: None,
                                })),
                            });
                            opened.push(Arc::clone(&target));
                            if fd == 1 {
                                redirects.stdout = OutputDestination::File(target);
                            } else {
                                redirects.stderr = OutputDestination::File(target);
                            }
                        }
                        _ => return Err((path, VfsError::new(Errno::EINVAL))),
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    // Earlier successful redirects still create/truncate their files, including
    // redirects superseded later in the same command or followed by an error.
    let mut result = result;
    for file in opened {
        if result.is_err() || Arc::strong_count(&file) == 1 {
            let close = file.finish().await;
            if result.is_ok() {
                result = close;
            }
        }
    }
    if let Err(err) = result {
        close_stdin_redirect(fs, redirects.stdin.take()).await;
        return Err(err);
    }
    Ok(redirects)
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
    let target = if target_fd == 1 {
        redirects.stdout.clone()
    } else {
        redirects.stderr.clone()
    };
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

fn is_shell_builtin_name(name: &str) -> bool {
    matches!(name, "cd" | "export" | "unset")
}

fn expansion_cost(
    simple: &SimpleCommand,
    env: &BTreeMap<String, String>,
    last_status: i32,
    limit: usize,
) -> Option<usize> {
    let mut cost = 0usize;
    let mut charge = |n: usize| {
        cost = cost.saturating_add(n);
        cost <= limit
    };
    // The current environment is cloned for command execution. Assignments
    // contribute their new values below, bounding retained session growth too.
    for (name, value) in env {
        if !charge(name.len().saturating_add(value.len())) {
            return None;
        }
    }
    let mut assigned = BTreeMap::new();
    for assignment in &simple.assignments {
        if !charge(
            assignment
                .name
                .len()
                .saturating_add(2 * std::mem::size_of::<String>()),
        ) {
            return None;
        }
        let mut bytes = 0usize;
        for segment in &assignment.value.segments {
            let value = match segment {
                Segment::Literal { value, .. } => std::borrow::Cow::Borrowed(value.as_str()),
                Segment::Expansion { name, .. } => expansion_value(name, env, last_status),
            };
            bytes = bytes.saturating_add(value.len());
            if bytes > limit {
                return None;
            }
        }
        assigned.insert(
            assignment.name.as_str(),
            (&assignment.value, bytes, None::<usize>),
        );
    }
    for (word, split, redirect) in simple
        .assignments
        .iter()
        .map(|a| (&a.value, false, false))
        .chain(simple.words.iter().map(|w| (w, true, false)))
        .chain(simple.redirects.iter().filter_map(|r| match &r.target {
            RedirectTarget::Word(w) => Some((w, true, true)),
            _ => None,
        }))
    {
        if !charge(std::mem::size_of::<String>()) {
            return None;
        }
        for segment in &word.segments {
            let (value, fields) = match segment {
                Segment::Literal { value, .. } => {
                    (std::borrow::Cow::Borrowed(value.as_str()), false)
                }
                Segment::Expansion { name, quoted } => {
                    (expansion_value(name, env, last_status), split && !quoted)
                }
            };
            // Null commands apply assignments before redirect expansion.
            // Cache estimates by variable and admit bytes before scanning for
            // fields. Repeated references cannot cause quadratic rescanning of
            // assignment syntax or materialize an amplified redirect string.
            let assigned_value = if redirect && let Segment::Expansion { name, .. } = segment {
                assigned.get_mut(name.as_str())
            } else {
                None
            };
            let bytes = assigned_value
                .as_ref()
                .map_or(value.len(), |(_, n, _)| value.len().max(*n));
            if !charge(bytes) {
                return None;
            }
            if fields {
                let mut field_count = value.split_whitespace().count().saturating_add(2);
                if let Some((word, _, cached_fields)) = assigned_value {
                    let count = cached_fields.get_or_insert_with(|| {
                        let mut count = 1usize;
                        for segment in &word.segments {
                            let value = match segment {
                                Segment::Literal { value, .. } => {
                                    std::borrow::Cow::Borrowed(value.as_str())
                                }
                                Segment::Expansion { name, .. } => {
                                    expansion_value(name, env, last_status)
                                }
                            };
                            count = count
                                .saturating_add(value.split_whitespace().count().saturating_add(2));
                        }
                        count
                    });
                    field_count = field_count.max(*count);
                }
                if !charge(
                    field_count.saturating_mul(
                        std::mem::size_of::<String>() + std::mem::size_of::<&str>(),
                    ),
                ) {
                    return None;
                }
            }
        }
    }
    Some(cost)
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

fn expansion_value<'a>(
    name: &str,
    env: &'a BTreeMap<String, String>,
    last_status: i32,
) -> std::borrow::Cow<'a, str> {
    if name == "?" {
        std::borrow::Cow::Owned(last_status.to_string())
    } else {
        std::borrow::Cow::Borrowed(env.get(name).map(String::as_str).unwrap_or_default())
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
const RESERVED_JS_GLOBALS: &[&str] = &[
    "Buffer",
    "Headers",
    "Response",
    "__dirname",
    "__filename",
    "console",
    "exports",
    "fetch",
    "globalThis",
    "module",
    "process",
    "require",
];

#[cfg(feature = "js")]
fn build_js_global_registry(
    entries: Vec<(String, Arc<dyn JsGlobal>)>,
) -> BTreeMap<String, Arc<dyn JsGlobal>> {
    merge_js_globals(BTreeMap::new(), entries)
        .unwrap_or_else(|err| panic!("SandboxBuilder::js_global {err}"))
}

/// Merges entries onto an existing surface, validating the result so a rejected
/// change never lands halfway.
///
/// A name already bound in `merged` is replaced; a name repeated inside
/// `entries` is an error, which is what makes a duplicate in a builder or a
/// replacement set fail instead of silently winning.
#[cfg(feature = "js")]
fn merge_js_globals(
    mut merged: BTreeMap<String, Arc<dyn JsGlobal>>,
    entries: Vec<(String, Arc<dyn JsGlobal>)>,
) -> Result<BTreeMap<String, Arc<dyn JsGlobal>>, JsGlobalError> {
    let mut added: BTreeSet<String> = BTreeSet::new();
    for (name, global) in entries {
        check_js_global_name(&name)?;
        if !added.insert(name.clone()) {
            return Err(JsGlobalError::new(format!(
                "cannot register duplicate name '{name}'"
            )));
        }
        merged.remove(&name);
        if let Some(other) = merged.keys().find(|other| paths_conflict(other, &name)) {
            return Err(JsGlobalError::new(format!(
                "cannot register '{name}'; it conflicts with '{other}'"
            )));
        }
        merged.insert(name, global);
    }
    Ok(merged)
}

#[cfg(feature = "js")]
fn check_js_global_name(name: &str) -> Result<(), JsGlobalError> {
    if !is_js_global_name(name) {
        return Err(JsGlobalError::new(format!(
            "cannot register invalid name '{name}'; names are dot-separated paths of [A-Za-z_][A-Za-z0-9_]* segments"
        )));
    }
    let root = name.split('.').next().unwrap_or(name);
    if RESERVED_JS_GLOBALS.contains(&root) {
        return Err(JsGlobalError::new(format!(
            "cannot register reserved name '{name}'; '{root}' is provided by the JavaScript runtime"
        )));
    }
    Ok(())
}

/// Reports whether one global path is the other or a namespace inside it, so
/// `tools` and `tools.search` cannot both be registered.
#[cfg(feature = "js")]
fn paths_conflict(left: &str, right: &str) -> bool {
    fn is_inside(namespace: &str, path: &str) -> bool {
        path.len() > namespace.len()
            && path.as_bytes()[namespace.len()] == b'.'
            && path.starts_with(namespace)
    }
    left == right || is_inside(left, right) || is_inside(right, left)
}

#[cfg(feature = "js")]
fn is_js_global_name(name: &str) -> bool {
    // Each segment uses the same identifier shape as shell assignments.
    !name.is_empty() && name.split('.').all(is_assignment_name)
}

fn is_assignment_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}
