//! In-process shell machine over a VFS.
//!
//! ```
//! use thinbox::machine::Machine;
//! use thinbox::vfs::{InMemoryVfs, VfsQuota};
//!
//! # fn main() {
//! # tokio::runtime::Builder::new_current_thread()
//! #     .enable_time()
//! #     .build()
//! #     .unwrap()
//! #     .block_on(async {
//! let machine = Machine::builder().vfs(InMemoryVfs::new(VfsQuota::unlimited())).build();
//!
//! let result = machine.exec("echo hello").await;
//! assert_eq!(result.exit_code, 0);
//! assert_eq!(result.stdout, "hello\n");
//! #     });
//! # }
//! ```

mod builtins;
pub mod command;
pub mod fs;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use command::SharedWriter;
use fs::{Fs, errno_message, normalize_absolute};
use tokio::time;

use crate::shell::{
    self, AndOrList, AndOrOp, Command as AstCommand, Pipeline, Redirect, RedirectOp,
    RedirectTarget, Segment, SimpleCommand, Word,
};
use crate::vfs::{Errno, FileType, InMemoryVfs, Metadata, Vfs, VfsError, VfsStats};

pub use command::{
    BoxAsyncRead, BoxAsyncWrite, Command, CommandContext, CommandFuture, CommandResult, Limits,
};

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub metrics: ExecMetrics,
}

#[derive(Debug, Clone)]
pub struct ExecMetrics {
    pub wall_time: Duration,
    pub commands: Vec<CommandTiming>,
    pub pipe_bytes: Vec<usize>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub peak_wasm_memory_bytes: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct CommandTiming {
    pub name: String,
    pub duration: Duration,
    pub exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineStats {
    pub vfs: Option<VfsStats>,
    pub commands_run: u64,
}

pub struct Machine {
    vfs: Arc<dyn Vfs>,
    commands: Arc<BTreeMap<String, Arc<dyn Command>>>,
    command_names: Arc<BTreeSet<String>>,
    limits: Limits,
    session: Mutex<Session>,
    commands_run: AtomicU64,
}

pub struct MachineBuilder {
    vfs: Arc<dyn Vfs>,
    commands: BTreeMap<String, Arc<dyn Command>>,
    limits: Limits,
    cwd: String,
    env: BTreeMap<String, String>,
}

impl Machine {
    pub fn builder() -> MachineBuilder {
        MachineBuilder::new()
    }

    pub fn vfs(&self) -> Arc<dyn Vfs> {
        Arc::clone(&self.vfs)
    }

    pub fn stats(&self) -> MachineStats {
        MachineStats {
            vfs: self.vfs.stats().and_then(Result::ok),
            commands_run: self.commands_run.load(Ordering::Relaxed),
        }
    }

    /// Executes a shell program against a snapshot of the current session.
    ///
    /// The wall-clock timeout is exec-wide. When it fires, partial stdout,
    /// stderr, metrics, and session mutations are discarded and the result
    /// exits 124. Blocking host calls already running on worker threads are not
    /// cancelled by that timeout, so VFS implementations should keep individual
    /// operations bounded. Concurrent execs each start from the session state
    /// visible at their own start; when they complete, the last stored session
    /// wins.
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
                stderr: "thinbox: command timed out\n".to_owned(),
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
        let mut exec = ExecState::new(session.last_status);
        for list in &program.lists {
            exec.last_status = self.exec_and_or_list(list, &mut session, &mut exec).await;
            if exec.limit_hit {
                break;
            }
        }

        session.last_status = exec.last_status;
        self.store_session(session);
        self.commands_run.fetch_add(
            u64::try_from(exec.command_count).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );

        let (stdout, stdout_truncated) = truncate_output(exec.stdout, self.limits.stdout_bytes);
        let (stderr, stderr_truncated) = truncate_output(exec.stderr, self.limits.stderr_bytes);
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
        let mut input = Vec::new();
        let mut status = 0;
        for (index, command) in pipeline.commands.iter().enumerate() {
            let is_last = index + 1 == pipeline.commands.len();
            let (next_input, command_status) = match command {
                AstCommand::Simple(simple) => {
                    self.exec_simple(simple, session, exec, input, is_last)
                        .await
                }
            };
            status = command_status;
            input = next_input;
            if !is_last {
                exec.pipe_bytes.push(input.len());
            }
            if exec.limit_hit {
                return status;
            }
        }
        status
    }

    async fn exec_simple(
        &self,
        simple: &SimpleCommand,
        session: &mut Session,
        exec: &mut ExecState,
        mut stdin: Vec<u8>,
        is_last: bool,
    ) -> (Vec<u8>, i32) {
        if exec.command_count >= self.limits.max_commands {
            exec.stderr
                .extend_from_slice(b"thinbox: maximum command count exceeded\n");
            exec.limit_hit = true;
            return (Vec::new(), 125);
        }
        exec.command_count += 1;

        let assignment_values =
            expand_assignments(&simple.assignments, &session.env, exec.last_status);
        let words = expand_words(&simple.words, &session.env, exec.last_status);
        if words.is_empty() {
            for (name, value) in assignment_values {
                session.env.insert(name, value);
            }
            return (Vec::new(), 0);
        }

        let command_name = words[0].clone();
        let args = words[1..].to_vec();
        let fs = Fs::new(
            Arc::clone(&self.vfs),
            Arc::clone(&self.command_names),
            session.cwd.clone(),
        );
        let redirects = match prepare_redirects(simple, &fs, &session.env, exec.last_status).await {
            Ok(redirects) => redirects,
            Err((path, err)) => {
                exec.stderr.extend_from_slice(
                    format!("{command_name}: {path}: {}\n", errno_message(err.errno())).as_bytes(),
                );
                return (Vec::new(), 1);
            }
        };
        if let Some(data) = redirects.stdin {
            stdin = data;
        }

        let mut command_env = session.env.clone();
        for (name, value) in assignment_values {
            command_env.insert(name, value);
        }
        command_env.insert("?".to_owned(), exec.last_status.to_string());

        let started = Instant::now();
        let mut special_stdout = Vec::new();
        let mut special_stderr = Vec::new();
        let shell_ctx = ShellBuiltinContext {
            session,
            fs: &fs,
            env: &mut command_env,
            stdout: &mut special_stdout,
            stderr: &mut special_stderr,
        };
        let (mut stdout_bytes, mut stderr_bytes, status) = if let Some(status) = self
            .run_shell_builtin(&command_name, &args, shell_ctx)
            .await
        {
            (special_stdout, special_stderr, status)
        } else if let Some(command) = self.commands.get(&command_name) {
            let stdout = SharedWriter::new();
            let stderr = SharedWriter::new();
            let ctx = CommandContext {
                args,
                env: command_env,
                cwd: session.cwd.clone(),
                stdin: Box::pin(Cursor::new(stdin)),
                stdout: stdout.boxed(),
                stderr: stderr.boxed(),
                fs: fs.clone(),
                limits: self.limits,
                commands: Arc::clone(&self.command_names),
            };
            let result = command.run(ctx).await;
            if let Some(bytes) = result.peak_wasm_memory_bytes {
                exec.peak_wasm_memory_bytes = Some(
                    exec.peak_wasm_memory_bytes
                        .map_or(bytes, |current| current.max(bytes)),
                );
            }
            (stdout.bytes(), stderr.bytes(), result.exit_code)
        } else {
            (
                Vec::new(),
                format!("{command_name}: command not found\n").into_bytes(),
                127,
            )
        };
        let duration = started.elapsed();

        let mut status = status;
        let mut routed_stdout = Vec::new();
        let mut routed_stderr = Vec::new();
        for (destination, data) in [
            (&redirects.stdout, stdout_bytes.as_slice()),
            (&redirects.stderr, stderr_bytes.as_slice()),
        ] {
            match destination {
                OutputDestination::Capture(CaptureFd::Stdout) => {
                    routed_stdout.extend_from_slice(data)
                }
                OutputDestination::Capture(CaptureFd::Stderr) => {
                    routed_stderr.extend_from_slice(data)
                }
                OutputDestination::File(target) => {
                    if let Err(err) = fs.write_file(&target.path, data, true).await {
                        exec.stderr.extend_from_slice(
                            format!(
                                "{command_name}: {}: {}\n",
                                target.path,
                                errno_message(err.errno())
                            )
                            .as_bytes(),
                        );
                        status = 1;
                    }
                }
            }
        }
        stdout_bytes = routed_stdout;
        stderr_bytes = routed_stderr;

        exec.timings.push(CommandTiming {
            name: command_name,
            duration,
            exit_code: status,
        });
        exec.stderr.extend_from_slice(&stderr_bytes);
        if is_last {
            exec.stdout.extend_from_slice(&stdout_bytes);
            (Vec::new(), status)
        } else {
            (stdout_bytes, status)
        }
    }

    async fn run_shell_builtin(
        &self,
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
                        ctx.stderr.extend_from_slice(
                            format!("cd: {target}: Not a directory\n").as_bytes(),
                        );
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
                    // Thinbox tracks one session environment, not Bash's exported
                    // bit, so listing shows every session variable.
                    for (key, value) in &ctx.session.env {
                        ctx.stdout.extend_from_slice(
                            format!("declare -x {key}=\"{value}\"\n").as_bytes(),
                        );
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

impl MachineBuilder {
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
            limits: Limits::default(),
            cwd: "/".to_owned(),
            env,
        }
    }

    pub fn vfs(mut self, vfs: impl Vfs + 'static) -> Self {
        self.vfs = Arc::new(vfs);
        self
    }

    pub fn vfs_arc(mut self, vfs: Arc<dyn Vfs>) -> Self {
        self.vfs = vfs;
        self
    }

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

    pub fn command_obj(mut self, name: impl Into<String>, command: impl Command + 'static) -> Self {
        let name = name.into();
        assert_not_reserved(&name);
        self.commands.insert(name, Arc::new(command));
        self
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = normalize_absolute(cwd.into());
        self.env.insert("PWD".to_owned(), self.cwd.clone());
        self
    }

    pub fn build(self) -> Machine {
        let command_names = Arc::new(self.commands.keys().cloned().collect());
        Machine {
            vfs: self.vfs,
            commands: Arc::new(self.commands),
            command_names,
            limits: self.limits,
            session: Mutex::new(Session {
                cwd: self.cwd,
                env: self.env,
                last_status: 0,
            }),
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

struct ExecState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timings: Vec<CommandTiming>,
    pipe_bytes: Vec<usize>,
    last_status: i32,
    command_count: usize,
    limit_hit: bool,
    peak_wasm_memory_bytes: Option<usize>,
}

impl ExecState {
    fn new(last_status: i32) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            timings: Vec::new(),
            pipe_bytes: Vec::new(),
            last_status,
            command_count: 0,
            limit_hit: false,
            peak_wasm_memory_bytes: None,
        }
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

#[derive(Debug, Clone)]
struct PreparedRedirects {
    stdin: Option<Vec<u8>>,
    stdout: OutputDestination,
    stderr: OutputDestination,
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
}

async fn prepare_redirects(
    simple: &SimpleCommand,
    fs: &Fs,
    env: &BTreeMap<String, String>,
    last_status: i32,
) -> Result<PreparedRedirects, (String, VfsError)> {
    let mut redirects = PreparedRedirects {
        stdin: None,
        stdout: OutputDestination::Capture(CaptureFd::Stdout),
        stderr: OutputDestination::Capture(CaptureFd::Stderr),
    };

    for redirect in &simple.redirects {
        match &redirect.target {
            RedirectTarget::Fd(fd) => apply_fd_redirect(&mut redirects, redirect, *fd)?,
            RedirectTarget::Word(word) => {
                let path = redirect_target(word, env, last_status)?;
                match (
                    redirect.fd.unwrap_or(default_redirect_fd(redirect.op)),
                    redirect.op,
                ) {
                    (0, RedirectOp::Read) => {
                        redirects.stdin =
                            Some(fs.read_file(&path).await.map_err(|err| (path, err))?);
                    }
                    (1, RedirectOp::Write) => {
                        preflight_output(fs, &path, false).await?;
                        redirects.stdout = OutputDestination::File(RedirectFile { path });
                    }
                    (1, RedirectOp::Append) => {
                        preflight_output(fs, &path, true).await?;
                        redirects.stdout = OutputDestination::File(RedirectFile { path });
                    }
                    (2, RedirectOp::Write) => {
                        preflight_output(fs, &path, false).await?;
                        redirects.stderr = OutputDestination::File(RedirectFile { path });
                    }
                    (2, RedirectOp::Append) => {
                        preflight_output(fs, &path, true).await?;
                        redirects.stderr = OutputDestination::File(RedirectFile { path });
                    }
                    (_, _) => return Err((path, VfsError::new(Errno::EINVAL))),
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
    let target = match target_fd {
        1 => redirects.stdout.clone(),
        2 => redirects.stderr.clone(),
        _ => unreachable!("validated target fd"),
    };
    match fd {
        1 => redirects.stdout = target,
        2 => redirects.stderr = target,
        _ => unreachable!("validated source fd"),
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
            "MachineBuilder::command cannot register reserved shell builtin '{name}'; cd, export, and unset are interpreted by the shell"
        );
    }
}

fn is_assignment_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn truncate_output(mut bytes: Vec<u8>, cap: usize) -> (Vec<u8>, bool) {
    if bytes.len() <= cap {
        return (bytes, false);
    }
    let marker = b"\n[thinbox: output truncated]\n";
    if cap <= marker.len() {
        return (marker.to_vec(), true);
    }
    let keep = cap - marker.len();
    let head = keep / 2;
    let tail = keep - head;
    let mut out = Vec::with_capacity(cap);
    out.extend_from_slice(&bytes[..head]);
    out.extend_from_slice(marker);
    out.extend_from_slice(&bytes[bytes.len() - tail..]);
    bytes.clear();
    (out, true)
}
