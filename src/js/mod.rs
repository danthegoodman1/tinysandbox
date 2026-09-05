//! Wasmtime-hosted QuickJS command.
//!
//! The supported Node `fs` subset intentionally omits `statSync` timestamp,
//! inode, uid, and gid fields until the VFS exposes them. Scripts still execute
//! as CommonJS wrappers, so top-level `await` is a syntax error; already-settled
//! promise chains and `await` inside async functions are drained before exit.
//! The guest also receives `fetch`, `Headers`, and `Response` globals backed by
//! an embedder-provided fetch handler. This is a WHATWG subset: there is no
//! `Request` class, streams, `AbortController`, redirects, or full header
//! mutator/iterator surface; header names are normalized without validation;
//! direct `Response` construction synthesizes a default reason phrase for
//! `statusText`; and tinysandbox accepts a non-standard `Response` `url` init.
//! JS execution uses the sandbox wall-clock budget, but timeout handling returns
//! a clean 124 result. Output streams directly through the command pipes, so
//! partial output remains available when execution stops. Module stack traces
//! still reflect QuickJS details: wrapper prefixes leave a line-1 column offset, method frames are named like `at boom`,
//! and visible `<tinysandbox>` glue frames can appear below user frames.
//!
//! `quickjs.wasm` is machine code by the time a script runs. The build script
//! compiles it for this crate's target and the module embeds the result, so the
//! first `js` command loads it rather than running Cranelift. [`precompile`]
//! and [`use_precompiled`] produce and install an artifact by hand, for a
//! different machine or a differently built process, and
//! [`runtime_source`] reports which path a process ended up on.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Semaphore, mpsc, oneshot};
use wasmtime::{
    Caller, Engine, Extern, Linker, Memory, MemoryType, Module, ResourceLimiter, Store, Trap,
};

use crate::sandbox::HostContext;
use crate::sandbox::command::{Command, CommandContext, CommandFuture, CommandResult};
use crate::sandbox::fs::{Fs, join_path};
use crate::sandbox::host::{Fetch, FetchRequest, FetchResponse, HostError, JsGlobal};
use crate::vfs::{Errno, FileHandle, FileType, Metadata, OpenMode, VfsError};

include!("engine_config.rs");

const QUICKJS_WASM: &[u8] = include_bytes!("../../assets/quickjs.wasm");
/// Machine code for [`QUICKJS_WASM`], produced by `build.rs` for this target.
#[cfg(quickjs_precompiled)]
const QUICKJS_CWASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/quickjs.cwasm"));
const EPOCH_TICK: Duration = Duration::from_millis(5);
const WASM_PAGE_BYTES: usize = 64 * 1024;
const QUICKJS_INITIAL_MEMORY_PAGES: u32 = 19;
const MAX_HOST_READ_BYTES: usize = 16 * 1024 * 1024;
const QUICKJS_HOST_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
const OUTPUT_CHUNK_BYTES: usize = 16 * 1024;
const MAX_CONCURRENT_JS: usize = 16;

/// Registers the `js` command in a sandbox command registry.
pub fn register(commands: &mut BTreeMap<String, Arc<dyn Command>>) {
    commands.insert("js".to_owned(), Arc::new(js_command));
}

fn js_command(ctx: CommandContext) -> CommandFuture {
    Box::pin(async move {
        let CommandContext {
            args,
            env,
            cwd,
            mut stdout,
            mut stderr,
            fs,
            limits,
            js_globals,
            js_fetch,
            js_prelude,
            ..
        } = ctx;

        let started = Instant::now();
        let source_bytes = limits.host_input_bytes.min(limits.wasm_memory_bytes);
        // Admission is bounded independently of Tokio's blocking pool. The
        // worker owns the permit until it has actually exited, even when the
        // command future is cancelled while a host callback is still running.
        static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let workers = WORKERS.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_JS)));
        let remaining = fs
            .remaining_wall_time()
            .unwrap_or_else(|| limits.wall_time.saturating_sub(started.elapsed()));
        let permit = match tokio::time::timeout(remaining, workers.clone().acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            _ => return CommandResult::new(124),
        };
        let invocation = match Invocation::parse(args, &fs, source_bytes).await {
            Ok(invocation) => invocation,
            Err(message) => {
                if fs.is_cancelled() {
                    return CommandResult::new(124);
                }
                let _ = stderr.write_all(message.as_bytes()).await;
                return CommandResult::new(1);
            }
        };
        let host_runtime = tokio::runtime::Handle::current();

        let (output, mut chunks) = mpsc::channel::<OutputChunk>(1);
        let config = JsRunConfig {
            invocation,
            env,
            cwd,
            fs,
            js_globals,
            js_fetch,
            js_prelude,
            limits: JsRuntimeLimits {
                wasm_memory_bytes: limits.wasm_memory_bytes,
                fetch_response_bytes: limits.fetch_response_bytes,
                source_bytes,
                // A serialized host response larger than the guest's entire
                // linear-memory allowance can never cross successfully.
                host_response_bytes: limits.wasm_memory_bytes,
                host_input_bytes: limits.host_input_bytes,
                max_open_files: limits.max_open_files,
                wall_time: limits.wall_time,
            },
            host_runtime,
            output,
            started,
        };

        let (finished, result) = oneshot::channel();
        if let Err(err) = thread::Builder::new()
            .name("tinysandbox-js-runtime".to_owned())
            .stack_size(QUICKJS_HOST_THREAD_STACK_BYTES)
            .spawn(move || {
                let _permit = permit;
                let _ = finished.send(run_quickjs(config));
            })
        {
            let _ = stderr
                .write_all(format!("js: failed to start runtime thread: {err}\n").as_bytes())
                .await;
            return CommandResult::failure();
        }

        // Ack only after the downstream write completes. This preserves bytes
        // and ordering, applies backpressure, and lets early pipe closure stop
        // the producer. Dropping this future also drops the pending ack.
        while let Some(chunk) = chunks.recv().await {
            let destination = if chunk.stdout {
                &mut stdout
            } else {
                &mut stderr
            };
            let written = destination.write_all(&chunk.bytes).await;
            let _ = chunk.ack.send(written);
        }
        let result = result.await.unwrap_or_else(|_| JsRunResult {
            exit_code: 1,
            stderr: b"js: runtime thread panicked\n".to_vec(),
            peak_wasm_memory_bytes: 0,
        });
        let _ = stderr.write_all(&result.stderr).await;
        CommandResult::new(result.exit_code).with_peak_wasm_memory(result.peak_wasm_memory_bytes)
    })
}

struct JsRunConfig {
    invocation: Invocation,
    env: BTreeMap<String, String>,
    cwd: String,
    fs: Fs,
    js_globals: Arc<BTreeMap<String, Arc<dyn JsGlobal>>>,
    js_fetch: Option<Arc<dyn Fetch>>,
    js_prelude: Arc<str>,
    limits: JsRuntimeLimits,
    host_runtime: tokio::runtime::Handle,
    output: mpsc::Sender<OutputChunk>,
    started: Instant,
}

#[derive(Clone, Copy)]
struct JsRuntimeLimits {
    wasm_memory_bytes: usize,
    fetch_response_bytes: usize,
    source_bytes: usize,
    host_response_bytes: usize,
    host_input_bytes: usize,
    max_open_files: usize,
    wall_time: Duration,
}

struct OutputChunk {
    stdout: bool,
    bytes: Vec<u8>,
    ack: oneshot::Sender<std::io::Result<()>>,
}

struct Invocation {
    code: String,
    script_path: String,
    argv: Vec<String>,
}

impl Invocation {
    async fn parse(args: Vec<String>, fs: &Fs, source_bytes: usize) -> Result<Self, String> {
        match args.as_slice() {
            [] => Err("js: usage: js [-e code] script.js [args...]\n".to_owned()),
            [flag, ..] if flag == "-e" => {
                if args.len() < 2 {
                    return Err("js: option requires an argument -- e\n".to_owned());
                }
                if args[1].len() > source_bytes {
                    return Err(format!(
                        "js: script source exceeded limit of {source_bytes} bytes\n"
                    ));
                }
                let code = args[1].clone();
                let mut argv = vec!["js".to_owned(), "-e".to_owned()];
                argv.extend(args[2..].iter().cloned());
                Ok(Self {
                    code,
                    script_path: "[eval]".to_owned(),
                    argv,
                })
            }
            [flag, ..] if flag.starts_with('-') => Err(format!("js: unsupported option {flag}\n")),
            [script, rest @ ..] => {
                let data = fs
                    .read_file_bounded(script, source_bytes)
                    .await
                    .map_err(|err| {
                        format!("js: {script}: {}\n", node_errno_message(err.errno()))
                    })?;
                let code = String::from_utf8(data)
                    .map_err(|_| format!("js: {script}: script is not valid UTF-8\n"))?;
                let mut argv = vec!["js".to_owned(), script.clone()];
                argv.extend(rest.iter().cloned());
                Ok(Self {
                    code,
                    script_path: fs.resolve(script),
                    argv,
                })
            }
        }
    }
}

#[derive(Serialize)]
struct GuestConfig<'a> {
    code: &'a str,
    #[serde(rename = "scriptPath")]
    script_path: &'a str,
    argv: &'a [String],
    env: &'a BTreeMap<String, String>,
    cwd: &'a str,
    globals: &'a [String],
    prelude: &'a str,
    vfs: bool,
}

struct JsRunResult {
    exit_code: i32,
    stderr: Vec<u8>,
    peak_wasm_memory_bytes: usize,
}

fn run_quickjs(config: JsRunConfig) -> JsRunResult {
    match run_quickjs_inner(config) {
        Ok(result) => result,
        Err(err) if is_epoch_timeout(&err) => JsRunResult {
            exit_code: 124,
            stderr: b"js: command timed out\n".to_vec(),
            peak_wasm_memory_bytes: 0,
        },
        Err(err)
            if err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|err| err.kind() == std::io::ErrorKind::BrokenPipe) =>
        {
            JsRunResult {
                exit_code: 141,
                stderr: Vec::new(),
                peak_wasm_memory_bytes: 0,
            }
        }
        Err(err) => JsRunResult {
            exit_code: 1,
            stderr: format!("js: {err}\n").into_bytes(),
            peak_wasm_memory_bytes: 0,
        },
    }
}

fn run_quickjs_inner(config: JsRunConfig) -> wasmtime::Result<JsRunResult> {
    let JsRunConfig {
        invocation,
        env,
        cwd,
        fs,
        js_globals,
        js_fetch,
        js_prelude,
        limits,
        host_runtime,
        output,
        started,
    } = config;
    if fs.is_cancelled()
        || fs
            .remaining_wall_time()
            .unwrap_or_else(|| limits.wall_time.saturating_sub(started.elapsed()))
            .is_zero()
    {
        return Err(Trap::Interrupt.into());
    }
    let compiled = compiled_runtime()?;
    if invocation.code.len() > limits.source_bytes {
        return Err(wasmtime::Error::msg(format!(
            "script source exceeded limit of {} bytes",
            limits.source_bytes
        )));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(wasmtime::Error::new)?;
    let mut store = Store::new(
        &compiled.engine,
        HostState::new(HostStateConfig {
            fs,
            runtime,
            host_runtime,
            globals: js_globals,
            fetch: js_fetch,
            limits,
            output,
            started,
        }),
    );
    store.limiter(|state| &mut state.limiter);
    let remaining = store.data().remaining_wall_time();
    if remaining.is_zero() || store.data().fs.is_cancelled() {
        return Err(Trap::Interrupt.into());
    }
    store.set_epoch_deadline(epoch_ticks(remaining));
    store.epoch_deadline_trap();

    let max_pages = (limits.wasm_memory_bytes / WASM_PAGE_BYTES).min(65_536);
    if max_pages < QUICKJS_INITIAL_MEMORY_PAGES as usize {
        return Err(wasmtime::Error::msg(
            "tinysandbox wasm memory limit exceeded",
        ));
    }
    let memory = Memory::new(
        &mut store,
        MemoryType::new(QUICKJS_INITIAL_MEMORY_PAGES, Some(max_pages as u32)),
    )?;
    let mut linker = Linker::new(&compiled.engine);
    define_tinysandbox_imports(&mut linker)?;
    define_wasi_imports(&mut linker)?;
    linker.define(&mut store, "env", "memory", memory)?;
    let instance = linker.instantiate(&mut store, &compiled.module)?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| wasmtime::Error::msg("quickjs wasm did not export memory"))?;
    let initial_memory = memory.data_size(&store);
    store.data_mut().limiter.record_peak(initial_memory);
    if initial_memory > limits.wasm_memory_bytes {
        return Err(wasmtime::Error::msg(
            "tinysandbox wasm memory limit exceeded",
        ));
    }

    let alloc = instance.get_typed_func::<i32, i32>(&mut store, "tinysandbox_alloc")?;
    let free = instance.get_typed_func::<i32, ()>(&mut store, "tinysandbox_free")?;
    let run = instance.get_typed_func::<(i32, i32, i32), i32>(&mut store, "tinysandbox_run")?;

    let global_names = store.data().globals.keys().cloned().collect::<Vec<_>>();
    let config = GuestConfig {
        code: &invocation.code,
        script_path: &invocation.script_path,
        argv: &invocation.argv,
        env: &env,
        cwd: &cwd,
        globals: &global_names,
        prelude: &js_prelude,
        vfs: true,
    };
    let input = bounded_json(&config, limits.wasm_memory_bytes).map_err(wasmtime::Error::new)?;
    let len = i32::try_from(input.len()).map_err(|_| wasmtime::Error::msg("script too large"))?;
    let ptr = alloc.call(&mut store, len)?;
    memory.write(&mut store, ptr_usize(ptr)?, &input)?;
    let heap_limit = i32::try_from(limits.wasm_memory_bytes).unwrap_or(i32::MAX);
    let exit_code = match run.call(&mut store, (ptr, len, heap_limit)) {
        Ok(exit_code) => exit_code,
        Err(_) if store.data().limiter.limit_exceeded => {
            return Ok(JsRunResult {
                exit_code: 1,
                stderr: b"js: wasm memory limit exceeded\n".to_vec(),
                peak_wasm_memory_bytes: store.data().limiter.peak_memory_bytes,
            });
        }
        Err(err) => {
            if store.data().timed_out
                || store.data().fs.is_cancelled()
                || store.data().remaining_wall_time().is_zero()
            {
                return Err(Trap::Interrupt.into());
            }
            return Err(err);
        }
    };
    if store.data().timed_out
        || store.data().fs.is_cancelled()
        || store.data().remaining_wall_time().is_zero()
    {
        return Err(Trap::Interrupt.into());
    }
    free.call(&mut store, ptr)?;

    store.data_mut().finish_files(exit_code == 0)?;
    let state = store.data();
    Ok(JsRunResult {
        exit_code,
        stderr: Vec::new(),
        peak_wasm_memory_bytes: state.limiter.peak_memory_bytes,
    })
}

fn is_epoch_timeout(err: &wasmtime::Error) -> bool {
    matches!(err.downcast_ref::<Trap>(), Some(Trap::Interrupt))
}

fn epoch_ticks(wall_time: Duration) -> u64 {
    let tick_ms = EPOCH_TICK.as_millis().max(1);
    let ticks = wall_time.as_millis().div_ceil(tick_ms);
    u64::try_from(ticks.max(1)).unwrap_or(u64::MAX)
}

struct CompiledRuntime {
    engine: Engine,
    module: Module,
    source: RuntimeSource,
}

static RUNTIME: OnceLock<wasmtime::Result<CompiledRuntime>> = OnceLock::new();
static RUNTIME_INIT: Mutex<()> = Mutex::new(());

fn compiled_runtime() -> wasmtime::Result<&'static CompiledRuntime> {
    if let Some(runtime) = RUNTIME.get() {
        return runtime
            .as_ref()
            .map_err(|err| wasmtime::Error::msg(err.to_string()));
    }
    let _init = RUNTIME_INIT
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    RUNTIME
        .get_or_init(|| {
            let engine = Engine::new(&quickjs_engine_config(None)?)?;
            let (module, source) = load_module(&engine)?;
            link_runtime(engine, module, source)
        })
        .as_ref()
        .map_err(|err| wasmtime::Error::msg(err.to_string()))
}

/// Where the machine code backing the JavaScript runtime came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSource {
    /// Loaded from the artifact the build script precompiled, or from one
    /// installed with [`use_precompiled`].
    Precompiled,
    /// Compiled in this process, the slow path the build normally avoids.
    Compiled,
}

/// Reports where this process's JavaScript runtime came from, initializing it
/// if no `js` command has run yet.
///
/// [`RuntimeSource::Compiled`] means the process paid for compilation: the
/// build script produced no artifact, or the one it produced was rejected by
/// this engine.
pub fn runtime_source() -> Result<RuntimeSource, PrecompileError> {
    compiled_runtime()
        .map(|runtime| runtime.source)
        .map_err(|err| PrecompileError::new(err.to_string()))
}

fn engine() -> Result<Engine, PrecompileError> {
    quickjs_engine_config(None)
        .and_then(|config| Engine::new(&config))
        .map_err(|err| PrecompileError::new(format!("wasmtime engine: {err}")))
}

/// Loads the module, preferring the build script's artifact.
///
/// A rejected artifact is not an error: an engine or CPU the build did not
/// target falls back to compiling, which is slower but correct.
fn load_module(engine: &Engine) -> wasmtime::Result<(Module, RuntimeSource)> {
    #[cfg(quickjs_precompiled)]
    // SAFETY: build.rs generated these embedded bytes from our fixed guest.
    #[allow(unsafe_code)]
    if let Ok(module) = unsafe { deserialize_module(engine, QUICKJS_CWASM) } {
        return Ok((module, RuntimeSource::Precompiled));
    }
    Module::new(engine, QUICKJS_WASM).map(|module| (module, RuntimeSource::Compiled))
}

#[allow(unsafe_code)]
unsafe fn deserialize_module(engine: &Engine, artifact: &[u8]) -> wasmtime::Result<Module> {
    // SAFETY: the caller guarantees authentic, unmodified Wasmtime output.
    // Header and CPU checks alone cannot establish that safety requirement.
    unsafe { Module::deserialize(engine, artifact) }
}

fn link_runtime(
    engine: Engine,
    module: Module,
    source: RuntimeSource,
) -> wasmtime::Result<CompiledRuntime> {
    start_epoch_thread(engine.clone())?;
    Ok(CompiledRuntime {
        engine,
        module,
        source,
    })
}

/// Error from the precompiled-runtime APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrecompileError {
    message: String,
}

impl PrecompileError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PrecompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PrecompileError {}

/// Compiles the embedded QuickJS module and returns the machine-code artifact.
///
/// The build script already does this for the crate's own target, so a normal
/// build never runs Cranelift at runtime. Use this to produce an artifact
/// yourself: to target a different machine, or to share one across processes
/// that build separately.
///
/// The engine here matches the one the runtime uses, so the artifact loads
/// through [`use_precompiled`] on any machine of the same architecture.
///
/// The artifact is tied to this Wasmtime version and to the CPU features of the
/// machine it targets; Wasmtime rejects a mismatched artifact rather than
/// running it, so treat it as a build output, never as a portable asset.
pub fn precompile() -> Result<Vec<u8>, PrecompileError> {
    engine()?
        .precompile_module(QUICKJS_WASM)
        .map_err(|err| PrecompileError::new(format!("precompile quickjs module: {err}")))
}

/// Installs an artifact from [`precompile`] as this process's JavaScript
/// runtime, in place of the one the build script embedded.
///
/// Call it before the first `js` command: the runtime is initialized once per
/// process, so a later call fails and leaves the existing runtime in place. A
/// stale or foreign artifact fails here too, which is the signal to fall back to
/// the normal path and let the next `js` command compile the module.
///
/// # Safety
///
/// `artifact` must be the unmodified output of [`precompile`] from a trusted
/// build. Wasmtime executes these bytes as native machine code: compatibility
/// checks do not validate their safety. Never accept artifacts from untrusted
/// input or storage that an attacker can modify.
///
/// ```compile_fail
/// // A raw artifact cannot be installed through safe Rust.
/// tinysandbox::js::use_precompiled(&[]).unwrap();
/// ```
#[allow(unsafe_code)]
pub unsafe fn use_precompiled(artifact: &[u8]) -> Result<(), PrecompileError> {
    let _init = RUNTIME_INIT
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if RUNTIME.get().is_some() {
        return Err(PrecompileError::new(
            "the JavaScript runtime is already initialized in this process",
        ));
    }
    let engine = engine()?;
    // SAFETY: this public unsafe function forwards the caller's trust contract.
    let module = unsafe { deserialize_module(&engine, artifact) }
        .map_err(|err| PrecompileError::new(format!("deserialize quickjs module: {err}")))?;
    let runtime = link_runtime(engine, module, RuntimeSource::Precompiled)
        .map_err(|err| PrecompileError::new(format!("link quickjs runtime: {err}")))?;
    RUNTIME.set(Ok(runtime)).map_err(|_| {
        PrecompileError::new("the JavaScript runtime is already initialized in this process")
    })
}

fn start_epoch_thread(engine: Engine) -> wasmtime::Result<()> {
    // Called only while holding RUNTIME_INIT, after a module is ready to become
    // the process runtime. A losing installation can never capture the ticker.
    thread::Builder::new()
        .name("tinysandbox-js-epoch".to_owned())
        .spawn(move || {
            loop {
                thread::sleep(EPOCH_TICK);
                engine.increment_epoch();
            }
        })
        .map_err(wasmtime::Error::new)?;
    Ok(())
}

struct HostState {
    fs: Fs,
    runtime: tokio::runtime::Runtime,
    host_runtime: tokio::runtime::Handle,
    globals: Arc<BTreeMap<String, Arc<dyn JsGlobal>>>,
    fetch: Option<Arc<dyn Fetch>>,
    output: mpsc::Sender<OutputChunk>,
    response: Vec<u8>,
    fds: BTreeMap<i32, OpenFile>,
    next_fd: i32,
    limiter: WasmLimiter,
    fetch_response_bytes: usize,
    host_response_bytes: usize,
    host_input_bytes: usize,
    max_open_files: usize,
    rng: u64,
    started: Instant,
    wall_time: Duration,
    timed_out: bool,
}

struct HostStateConfig {
    fs: Fs,
    runtime: tokio::runtime::Runtime,
    host_runtime: tokio::runtime::Handle,
    globals: Arc<BTreeMap<String, Arc<dyn JsGlobal>>>,
    fetch: Option<Arc<dyn Fetch>>,
    limits: JsRuntimeLimits,
    output: mpsc::Sender<OutputChunk>,
    started: Instant,
}

impl HostState {
    fn new(config: HostStateConfig) -> Self {
        let HostStateConfig {
            fs,
            runtime,
            host_runtime,
            globals,
            fetch,
            limits,
            output,
            started,
        } = config;
        Self {
            fs,
            runtime,
            host_runtime,
            globals,
            fetch,
            output,
            response: Vec::new(),
            fds: BTreeMap::new(),
            next_fd: 3,
            limiter: WasmLimiter::new(limits.wasm_memory_bytes),
            fetch_response_bytes: limits.fetch_response_bytes,
            host_response_bytes: limits.host_response_bytes,
            host_input_bytes: limits.host_input_bytes,
            max_open_files: limits.max_open_files,
            rng: 0x7468_696e_626f_7821,
            started,
            wall_time: limits.wall_time,
            timed_out: false,
        }
    }

    fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    fn remaining_wall_time(&self) -> Duration {
        self.fs
            .remaining_wall_time()
            .unwrap_or_else(|| self.wall_time.saturating_sub(self.started.elapsed()))
    }

    fn host_read_bytes(&self) -> usize {
        // Reserve room for JSON framing and base64 expansion before reading.
        self.host_input_bytes
            .min(self.host_response_bytes.saturating_sub(128) / 4 * 3)
    }

    fn finish_files(&mut self, commit: bool) -> wasmtime::Result<()> {
        let mut failure = None;
        for (_, file) in std::mem::take(&mut self.fds) {
            let result = if commit && !self.fs.is_cancelled() {
                self.block_on(self.fs.close(file.handle))
            } else {
                self.block_on(self.fs.abort(file.handle))
            };
            if let Err(err) = result {
                failure.get_or_insert(err);
            }
        }
        failure.map_or(Ok(()), |err| Err(wasmtime::Error::new(err)))
    }
}

impl Drop for HostState {
    fn drop(&mut self) {
        // Covers traps, initialization errors, timeout, and unwinding. Staged
        // outputs from an unsuccessful execution must not be committed here.
        let _ = self.finish_files(false);
    }
}

struct HostCallTimeout;

fn block_on_host_timeout<F, T>(
    state: &HostState,
    call: impl FnOnce(HostContext) -> F,
) -> Result<T, HostCallTimeout>
where
    F: Future<Output = T>,
{
    // Preserve time for the guest to catch/render host errors before the outer
    // command expires. This shorter deadline is visible to the callback too.
    let headroom = (EPOCH_TICK * 10).min(state.wall_time / 4);
    let remaining = state.remaining_wall_time().saturating_sub(headroom);
    let deadline = Instant::now()
        .checked_add(remaining)
        .unwrap_or_else(Instant::now);
    let context = state.fs.host_context().child(deadline);
    struct CancelOnDrop(HostContext);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _guard = CancelOnDrop(context.clone());
    state.host_runtime.block_on(async {
        if context.is_cancelled() {
            return Err(HostCallTimeout);
        }
        let future = call(context.clone());
        let mut future = std::pin::pin!(future);
        let mut cancelled = std::pin::pin!(context.cancelled());
        std::future::poll_fn(|cx| {
            use std::task::Poll;
            if context.is_cancelled() {
                return Poll::Ready(Err(HostCallTimeout));
            }
            if let Poll::Ready(value) = future.as_mut().poll(cx) {
                return Poll::Ready(if context.is_cancelled() {
                    Err(HostCallTimeout)
                } else {
                    Ok(value)
                });
            }
            if cancelled.as_mut().poll(cx).is_ready() {
                Poll::Ready(Err(HostCallTimeout))
            } else {
                Poll::Pending
            }
        })
        .await
    })
}

#[derive(Clone)]
struct OpenFile {
    handle: FileHandle,
    position: u64,
}

struct WasmLimiter {
    max_memory_bytes: usize,
    peak_memory_bytes: usize,
    limit_exceeded: bool,
}

fn capture_output(
    caller: &mut Caller<'_, HostState>,
    memory: &Memory,
    ptr: i32,
    len: i32,
    stdout: bool,
) -> wasmtime::Result<i32> {
    let len = usize_len(len)?;
    let ptr = ptr_usize(ptr)?;
    let end = ptr
        .checked_add(len)
        .ok_or_else(|| wasmtime::Error::msg("invalid output range"))?;
    if end > memory.data_size(&*caller) {
        return Err(wasmtime::Error::msg("output range exceeds guest memory"));
    }
    for offset in (ptr..end).step_by(OUTPUT_CHUNK_BYTES) {
        if caller.data().fs.is_cancelled() || caller.data().remaining_wall_time().is_zero() {
            return Err(Trap::Interrupt.into());
        }
        let bytes = memory.data(&*caller)[offset..end.min(offset + OUTPUT_CHUNK_BYTES)].to_vec();
        let (ack, written) = oneshot::channel();
        let state = caller.data();
        let result = state.host_runtime.block_on(async {
            tokio::time::timeout(state.remaining_wall_time(), async {
                state
                    .output
                    .send(OutputChunk { stdout, bytes, ack })
                    .await
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
                written
                    .await
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?
            })
            .await
        });
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(wasmtime::Error::new(err)),
            Err(_) => return Err(Trap::Interrupt.into()),
        }
    }
    Ok(i32::try_from(len).unwrap_or(i32::MAX))
}

/// Stops serialization before allocation exceeds the response budget.
fn bounded_json(value: &impl Serialize, cap: usize) -> serde_json::Result<Vec<u8>> {
    struct Writer {
        bytes: Vec<u8>,
        cap: usize,
    }
    impl std::io::Write for Writer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if data.len() > self.cap.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("host response exceeded limit"));
            }
            let needed = self.bytes.len() + data.len();
            if needed > self.bytes.capacity() {
                let capacity = needed
                    .max(self.bytes.capacity().saturating_mul(2))
                    .min(self.cap);
                self.bytes.reserve_exact(capacity - self.bytes.len());
            }
            self.bytes.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer {
        bytes: Vec::new(),
        cap,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}

impl WasmLimiter {
    fn new(max_memory_bytes: usize) -> Self {
        Self {
            max_memory_bytes,
            peak_memory_bytes: 0,
            limit_exceeded: false,
        }
    }

    fn record_peak(&mut self, bytes: usize) {
        self.peak_memory_bytes = self.peak_memory_bytes.max(bytes);
    }
}

impl ResourceLimiter for WasmLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.max_memory_bytes {
            self.limit_exceeded = true;
            Err(wasmtime::Error::msg(
                "tinysandbox wasm memory limit exceeded",
            ))
        } else {
            self.record_peak(desired);
            Ok(true)
        }
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(maximum.is_none_or(|max| desired <= max))
    }

    fn memories(&self) -> usize {
        1
    }
}

fn define_tinysandbox_imports(linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
    linker.func_wrap(
        "tinysandbox",
        "should_interrupt",
        |mut caller: Caller<'_, HostState>| -> i32 {
            if caller.data().fs.is_cancelled() || caller.data().remaining_wall_time().is_zero() {
                caller.data_mut().timed_out = true;
                1
            } else {
                0
            }
        },
    )?;
    linker.func_wrap(
        "tinysandbox",
        "host_call",
        |mut caller: Caller<'_, HostState>,
         op_ptr: i32,
         op_len: i32,
         json_ptr: i32,
         json_len: i32|
         -> wasmtime::Result<i32> {
            if caller.data().fs.is_cancelled() || caller.data().remaining_wall_time().is_zero() {
                return Err(Trap::Interrupt.into());
            }
            let memory = memory(&mut caller)?;
            let op = read_utf8(&caller, &memory, op_ptr, op_len)?;
            let input = read_utf8(&caller, &memory, json_ptr, json_len)?;
            let response = match serde_json::from_str(&input) {
                Ok(args) => handle_host_call(caller.data_mut(), &op, args),
                Err(err) => HostResponse::error(HostCallError::invalid_json(err)),
            };
            let cap = caller.data().host_response_bytes;
            let bytes = bounded_json(&response, cap).unwrap_or_else(|_| {
                bounded_json(
                    &HostResponse::error(HostCallError {
                        code: "E2BIG",
                        message: format!("host response exceeded limit of {cap} bytes"),
                    }),
                    cap,
                )
                .unwrap_or_default()
            });
            caller.data_mut().response = bytes;
            Ok(0)
        },
    )?;
    linker.func_wrap(
        "tinysandbox",
        "host_response_len",
        |caller: Caller<'_, HostState>| -> i32 {
            i32::try_from(caller.data().response.len()).unwrap_or(i32::MAX)
        },
    )?;
    linker.func_wrap(
        "tinysandbox",
        "host_response_read",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> wasmtime::Result<i32> {
            let memory = memory(&mut caller)?;
            let len = usize_len(len)?;
            let ptr = ptr_usize(ptr)?;
            let data = std::mem::take(&mut caller.data_mut().response);
            let n = data.len().min(len);
            let result = memory.write(&mut caller, ptr, &data[..n]);
            caller.data_mut().response = data;
            result?;
            Ok(i32::try_from(n).unwrap_or(i32::MAX))
        },
    )?;
    linker.func_wrap(
        "tinysandbox",
        "write_stdout",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> wasmtime::Result<i32> {
            let memory = memory(&mut caller)?;
            capture_output(&mut caller, &memory, ptr, len, true)
        },
    )?;
    linker.func_wrap(
        "tinysandbox",
        "write_stderr",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> wasmtime::Result<i32> {
            let memory = memory(&mut caller)?;
            capture_output(&mut caller, &memory, ptr, len, false)
        },
    )?;
    Ok(())
}

fn define_wasi_imports(linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
    linker.func_wrap("wasi_snapshot_preview1", "fd_write", wasi_fd_write)?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_read",
        |_caller: Caller<'_, HostState>,
         _fd: i32,
         _iovs: i32,
         _iovs_len: i32,
         _nread: i32|
         -> i32 { WASI_ERRNO_SUCCESS },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_close",
        |_caller: Caller<'_, HostState>, _fd: i32| -> i32 { WASI_ERRNO_SUCCESS },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_seek",
        |_caller: Caller<'_, HostState>,
         _fd: i32,
         _offset: i64,
         _whence: i32,
         _new_offset: i32|
         -> i32 { WASI_ERRNO_BADF },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_fdstat_get",
        |_caller: Caller<'_, HostState>, _fd: i32, _stat: i32| -> i32 { WASI_ERRNO_BADF },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_prestat_get",
        |_caller: Caller<'_, HostState>, _fd: i32, _buf: i32| -> i32 { WASI_ERRNO_BADF },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_prestat_dir_name",
        |_caller: Caller<'_, HostState>, _fd: i32, _path: i32, _path_len: i32| -> i32 {
            WASI_ERRNO_BADF
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "args_sizes_get",
        |mut caller: Caller<'_, HostState>,
         argc: i32,
         argv_buf_size: i32|
         -> wasmtime::Result<i32> {
            let memory = memory(&mut caller)?;
            write_u32(&mut caller, &memory, argc, 0)?;
            write_u32(&mut caller, &memory, argv_buf_size, 0)?;
            Ok(WASI_ERRNO_SUCCESS)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "args_get",
        |_caller: Caller<'_, HostState>, _argv: i32, _argv_buf: i32| -> i32 { WASI_ERRNO_SUCCESS },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "environ_sizes_get",
        |mut caller: Caller<'_, HostState>, count: i32, buf_size: i32| -> wasmtime::Result<i32> {
            let memory = memory(&mut caller)?;
            write_u32(&mut caller, &memory, count, 0)?;
            write_u32(&mut caller, &memory, buf_size, 0)?;
            Ok(WASI_ERRNO_SUCCESS)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "environ_get",
        |_caller: Caller<'_, HostState>, _environ: i32, _environ_buf: i32| -> i32 {
            WASI_ERRNO_SUCCESS
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_time_get",
        wasi_clock_time_get,
    )?;
    linker.func_wrap("wasi_snapshot_preview1", "random_get", wasi_random_get)?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "proc_exit",
        |_caller: Caller<'_, HostState>, code: i32| -> wasmtime::Result<()> {
            Err(wasmtime::Error::msg(format!("wasi proc_exit({code})")))
        },
    )?;
    Ok(())
}

const WASI_ERRNO_SUCCESS: i32 = 0;
const WASI_ERRNO_BADF: i32 = 8;
const WASI_ERRNO_INVAL: i32 = 28;

fn wasi_fd_write(
    mut caller: Caller<'_, HostState>,
    fd: i32,
    iovs: i32,
    iovs_len: i32,
    nwritten: i32,
) -> wasmtime::Result<i32> {
    let memory = memory(&mut caller)?;
    if !matches!(fd, 1 | 2) {
        return Ok(WASI_ERRNO_BADF);
    }
    let mut total = 0_u32;
    for index in 0..usize_len(iovs_len)? {
        let base = ptr_usize(iovs)? + index * 8;
        let ptr = read_u32(&caller, &memory, base)? as i32;
        let len = read_u32(&caller, &memory, base + 4)? as i32;
        let captured = capture_output(&mut caller, &memory, ptr, len, fd == 1)?;
        total = total.saturating_add(captured as u32);
    }
    write_u32(&mut caller, &memory, nwritten, total)?;
    Ok(WASI_ERRNO_SUCCESS)
}

fn wasi_clock_time_get(
    mut caller: Caller<'_, HostState>,
    clock_id: i32,
    _precision: i64,
    result: i32,
) -> wasmtime::Result<i32> {
    let nanos = match clock_id {
        0 => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        1 => caller.data().started.elapsed().as_nanos(),
        _ => return Ok(WASI_ERRNO_INVAL),
    };
    let memory = memory(&mut caller)?;
    memory.write(
        &mut caller,
        ptr_usize(result)?,
        &(nanos as u64).to_le_bytes(),
    )?;
    Ok(WASI_ERRNO_SUCCESS)
}

fn wasi_random_get(mut caller: Caller<'_, HostState>, ptr: i32, len: i32) -> wasmtime::Result<i32> {
    let mut out = vec![0_u8; usize_len(len)?];
    for byte in &mut out {
        let mut x = caller.data().rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        caller.data_mut().rng = x;
        *byte = x as u8;
    }
    let memory = memory(&mut caller)?;
    memory.write(&mut caller, ptr_usize(ptr)?, &out)?;
    Ok(WASI_ERRNO_SUCCESS)
}

#[derive(Serialize)]
struct HostResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

impl HostResponse {
    fn value(value: Value) -> Self {
        Self {
            value: Some(value),
            error: None,
        }
    }

    fn error(error: impl Serialize) -> Self {
        Self {
            value: None,
            error: Some(serde_json::to_value(error).expect("host errors serialize")),
        }
    }
}

#[derive(Serialize)]
struct HostCallError {
    code: &'static str,
    message: String,
}

impl HostCallError {
    fn invalid_json(error: serde_json::Error) -> Self {
        Self {
            code: "EINVAL",
            message: format!("invalid host call JSON: {error}"),
        }
    }
}

#[derive(Serialize)]
struct HostErrorPayload {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

impl From<HostError> for HostErrorPayload {
    fn from(error: HostError) -> Self {
        Self {
            message: error.message,
            code: error.code,
        }
    }
}

#[derive(Serialize)]
struct NodeError {
    code: &'static str,
    errno: i32,
    message: &'static str,
    syscall: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

fn handle_host_call(state: &mut HostState, op: &str, args: Value) -> HostResponse {
    if op == "global" {
        match handle_js_global(state, &args) {
            Ok(value) => HostResponse::value(value),
            Err(err) => HostResponse::error(HostErrorPayload::from(err)),
        }
    } else if op == "fetch" {
        match handle_fetch(state, &args) {
            Ok(value) => HostResponse::value(value),
            Err(err) => HostResponse::error(HostErrorPayload::from(err)),
        }
    } else {
        match handle_host_call_result(state, op, &args) {
            Ok(value) => HostResponse::value(value),
            Err(err) => HostResponse::error(err),
        }
    }
}

#[derive(Deserialize)]
struct FetchHostRequest {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

#[derive(Serialize)]
struct FetchHostResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

fn handle_fetch(state: &mut HostState, args: &Value) -> Result<Value, HostError> {
    let fetch = state.fetch.clone().ok_or_else(|| {
        HostError::new("network is not available in this sandbox: no fetch handler registered")
    })?;
    let payload: FetchHostRequest = serde_json::from_value(args.clone())
        .map_err(|err| HostError::new(format!("invalid fetch request: {err}")))?;
    let body = payload
        .body
        .as_deref()
        .map(|body| {
            BASE64_STANDARD
                .decode(body)
                .map_err(|_| HostError::new("invalid fetch request body encoding"))
        })
        .transpose()?;
    let request = FetchRequest {
        url: payload.url,
        method: payload.method,
        headers: payload.headers,
        body,
    };
    let response =
        match block_on_host_timeout(state, |context| fetch.fetch_with_context(request, context)) {
            Ok(result) => result?,
            Err(_) => return Err(HostError::new("fetch timed out")),
        };
    if response.body.len() > state.fetch_response_bytes {
        return Err(HostError::new(format!(
            "fetch response body exceeded limit of {} bytes",
            state.fetch_response_bytes
        )));
    }
    if response.body.len() > state.host_response_bytes.saturating_sub(128) / 4 * 3 {
        return Err(HostError::new(format!(
            "host response exceeded limit of {} bytes",
            state.host_response_bytes
        ))
        .with_code("E2BIG"));
    }
    fetch_response_json(response).map_err(|err| HostError::new(err.to_string()))
}

fn fetch_response_json(response: FetchResponse) -> serde_json::Result<Value> {
    serde_json::to_value(FetchHostResponse {
        status: response.status,
        headers: response.headers,
        body: base64_encode(&response.body),
    })
}

fn handle_js_global(state: &mut HostState, args: &Value) -> Result<Value, HostError> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::new("global host call requires a string name"))?;
    let global = state
        .globals
        .get(name)
        .cloned()
        .ok_or_else(|| HostError::new(format!("unknown global '{name}'")))?;
    let payload = args.get("args").cloned().unwrap_or(Value::Null);
    match block_on_host_timeout(state, |context| global.call_with_context(payload, context)) {
        Ok(result) => result,
        Err(_) => Err(HostError::new(format!("global '{name}' timed out"))),
    }
}

fn handle_host_call_result(
    state: &mut HostState,
    op: &str,
    args: &Value,
) -> Result<Value, NodeError> {
    match op {
        "readFile" => {
            let path = string_arg(args, "path")?;
            let data = state
                .block_on(state.fs.read_file_bounded(&path, state.host_read_bytes()))
                .map_err(|err| node_error(err, "open", Some(path.clone())))?;
            Ok(json!(base64_encode(&data)))
        }
        "writeFile" => {
            let path = string_arg(args, "path")?;
            let data = bytes_arg(args, "data")?;
            state
                .block_on(state.fs.write_file(&path, &data, false))
                .map_err(|err| node_error(err, "open", Some(path)))?;
            Ok(Value::Null)
        }
        "appendFile" => {
            let path = string_arg(args, "path")?;
            let data = bytes_arg(args, "data")?;
            state
                .block_on(state.fs.write_file(&path, &data, true))
                .map_err(|err| node_error(err, "open", Some(path)))?;
            Ok(Value::Null)
        }
        "mkdir" => {
            let path = string_arg(args, "path")?;
            if bool_arg(args, "recursive") {
                mkdir_recursive(state, &path)?;
            } else {
                state
                    .block_on(state.fs.mkdir(&path))
                    .map_err(|err| node_error(err, "mkdir", Some(path)))?;
            }
            Ok(Value::Null)
        }
        "readdir" => {
            let path = string_arg(args, "path")?;
            let entries = state
                .block_on(state.fs.readdir(&path))
                .map_err(|err| node_error(err, "scandir", Some(path)))?;
            if bool_arg(args, "withFileTypes") {
                Ok(json!(
                    entries
                        .into_iter()
                        .map(|entry| json!({
                            "name": entry.name,
                            "isFile": entry.metadata.file_type == FileType::File,
                            "isDirectory": entry.metadata.file_type == FileType::Directory,
                        }))
                        .collect::<Vec<_>>()
                ))
            } else {
                Ok(json!(
                    entries
                        .into_iter()
                        .map(|entry| entry.name)
                        .collect::<Vec<_>>()
                ))
            }
        }
        "stat" => {
            let path = string_arg(args, "path")?;
            let metadata = state
                .block_on(state.fs.stat(&path))
                .map_err(|err| node_error(err, "stat", Some(path)))?;
            Ok(metadata_json(metadata))
        }
        "rename" => {
            let from = string_arg(args, "from")?;
            let to = string_arg(args, "to")?;
            state
                .block_on(state.fs.rename(&from, &to))
                .map_err(|err| node_error(err, "rename", Some(from)))?;
            Ok(Value::Null)
        }
        "rm" => {
            let path = string_arg(args, "path")?;
            match remove_path(state, &path, bool_arg(args, "recursive")) {
                Ok(()) => Ok(Value::Null),
                Err(err) if err.code == "ENOENT" && bool_arg(args, "force") => Ok(Value::Null),
                Err(err) => Err(err),
            }
        }
        "unlink" => {
            let path = string_arg(args, "path")?;
            state
                .block_on(state.fs.unlink(&path))
                .map_err(|err| node_error(err, "unlink", Some(path)))?;
            Ok(Value::Null)
        }
        "rmdir" => {
            let path = string_arg(args, "path")?;
            if bool_arg(args, "recursive") {
                remove_path(state, &path, true)?;
            } else {
                state
                    .block_on(state.fs.rmdir(&path))
                    .map_err(|err| node_error(err, "rmdir", Some(path)))?;
            }
            Ok(Value::Null)
        }
        "exists" => {
            let path = string_arg(args, "path")?;
            Ok(json!(state.block_on(state.fs.stat(&path)).is_ok()))
        }
        "open" => {
            if state.fds.len() >= state.max_open_files || state.next_fd == i32::MAX {
                return Err(node_error(VfsError::new(Errno::ENOSPC), "open", None));
            }
            let path = string_arg(args, "path")?;
            let flags = string_arg(args, "flags")?;
            let mode =
                open_mode(&flags).map_err(|err| node_error(err, "open", Some(path.clone())))?;
            let handle = state
                .block_on(state.fs.open(&path, mode))
                .map_err(|err| node_error(err, "open", Some(path.clone())))?;
            let fd = state.next_fd;
            state.next_fd += 1;
            state.fds.insert(
                fd,
                OpenFile {
                    handle,
                    position: 0,
                },
            );
            Ok(json!(fd))
        }
        "read" => {
            let fd = i32_arg(args, "fd")?;
            let offset = u64_arg(args, "position")?;
            let len = usize_arg(args, "length")?;
            let file = state
                .fds
                .get(&fd)
                .cloned()
                .ok_or_else(|| node_error(VfsError::new(Errno::EBADF), "read", None))?;
            let read_offset = offset.unwrap_or(file.position);
            // File identity belongs to the open handle, regardless of what
            // currently occupies its original path. Short reads are valid.
            let len = len.min(MAX_HOST_READ_BYTES).min(state.host_read_bytes());
            let (mut data, n) = state
                .block_on(state.fs.read_at(file.handle, read_offset, vec![0; len]))
                .map_err(|err| node_error(err, "read", None))?;
            data.truncate(n);
            if offset.is_none() {
                state.fds.get_mut(&fd).expect("fd was validated").position =
                    file.position.saturating_add(n as u64);
            }
            Ok(json!({ "bytesRead": n, "data": base64_encode(&data) }))
        }
        "write" => {
            let fd = i32_arg(args, "fd")?;
            let data = bytes_arg(args, "data")?;
            let offset = u64_arg(args, "position")?;
            let file = state
                .fds
                .get(&fd)
                .cloned()
                .ok_or_else(|| node_error(VfsError::new(Errno::EBADF), "write", None))?;
            let write_offset = offset.unwrap_or(file.position);
            let n = state
                .block_on(state.fs.write_at(file.handle, write_offset, data))
                .map_err(|err| node_error(err, "write", None))?;
            if offset.is_none() {
                state.fds.get_mut(&fd).expect("fd was validated").position =
                    file.position.saturating_add(n as u64);
            }
            Ok(json!(n))
        }
        "ftruncate" => {
            let fd = i32_arg(args, "fd")?;
            let len = u64_arg(args, "len")?.unwrap_or(0);
            let file = state
                .fds
                .get(&fd)
                .cloned()
                .ok_or_else(|| node_error(VfsError::new(Errno::EBADF), "ftruncate", None))?;
            state
                .block_on(state.fs.truncate(file.handle, len))
                .map_err(|err| node_error(err, "ftruncate", None))?;
            Ok(Value::Null)
        }
        "close" => {
            let fd = i32_arg(args, "fd")?;
            let file = state
                .fds
                .remove(&fd)
                .ok_or_else(|| node_error(VfsError::new(Errno::EBADF), "close", None))?;
            state
                .block_on(state.fs.close(file.handle))
                .map_err(|err| node_error(err, "close", None))?;
            Ok(Value::Null)
        }
        "copyFile" => {
            let src = string_arg(args, "src")?;
            let dest = string_arg(args, "dest")?;
            let data = state
                .block_on(state.fs.read_file_bounded(&src, state.host_input_bytes))
                .map_err(|err| node_error(err, "copyfile", Some(src)))?;
            state
                .block_on(state.fs.write_file(&dest, &data, false))
                .map_err(|err| node_error(err, "copyfile", Some(dest)))?;
            Ok(Value::Null)
        }
        _ => Err(node_error(
            VfsError::new(Errno::EINVAL),
            "tinysandbox",
            None,
        )),
    }
}

fn mkdir_recursive(state: &HostState, path: &str) -> Result<(), NodeError> {
    let resolved = state.fs.resolve(path);
    if resolved == "/" {
        return Ok(());
    }
    let mut current = String::new();
    for part in resolved.trim_start_matches('/').split('/') {
        current.push('/');
        current.push_str(part);
        match state.block_on(state.fs.mkdir(&current)) {
            Ok(()) => {}
            Err(err) if err.errno() == Errno::EEXIST => match state
                .block_on(state.fs.stat(&current))
            {
                Ok(metadata) if metadata.file_type == FileType::Directory => {}
                Ok(_) => return Err(node_error(err, "mkdir", Some(path.to_owned()))),
                Err(stat_err) => return Err(node_error(stat_err, "mkdir", Some(path.to_owned()))),
            },
            Err(err) => return Err(node_error(err, "mkdir", Some(path.to_owned()))),
        }
    }
    Ok(())
}

fn remove_path(state: &HostState, path: &str, recursive: bool) -> Result<(), NodeError> {
    let metadata = state
        .block_on(state.fs.stat(path))
        .map_err(|err| node_error(err, "rm", Some(path.to_owned())))?;
    if metadata.file_type == FileType::File {
        return state
            .block_on(state.fs.unlink(path))
            .map_err(|err| node_error(err, "unlink", Some(path.to_owned())));
    }
    if !recursive {
        return Err(node_error(
            VfsError::new(Errno::EISDIR),
            "rm",
            Some(path.to_owned()),
        ));
    }
    let entries = state
        .block_on(state.fs.readdir(path))
        .map_err(|err| node_error(err, "scandir", Some(path.to_owned())))?;
    for entry in entries {
        remove_path(state, &join_path(path, &entry.name), true)?;
    }
    state
        .block_on(state.fs.rmdir(path))
        .map_err(|err| node_error(err, "rmdir", Some(path.to_owned())))
}

fn metadata_json(metadata: Metadata) -> Value {
    json!({
        "size": metadata.len,
        "isFile": metadata.file_type == FileType::File,
        "isDirectory": metadata.file_type == FileType::Directory,
    })
}

fn open_mode(flags: &str) -> Result<OpenMode, VfsError> {
    match flags {
        "r" => Ok(OpenMode::read_only()),
        "r+" => Ok(OpenMode::read_write()),
        "w" => Ok(OpenMode::write_only().create().truncate()),
        "wx" | "xw" => Ok(OpenMode::write_only().create_new().truncate()),
        "w+" => Ok(OpenMode::read_write().create().truncate()),
        "wx+" | "w+x" | "xw+" | "x+w" => Ok(OpenMode::read_write().create_new().truncate()),
        "a" => Ok(OpenMode::write_only().create().append()),
        "ax" | "xa" => Ok(OpenMode::write_only().create_new().append()),
        "a+" => Ok(OpenMode::read_write().create().append()),
        "ax+" | "a+x" | "xa+" | "x+a" => Ok(OpenMode::read_write().create_new().append()),
        _ => Err(VfsError::new(Errno::EINVAL)),
    }
}

fn string_arg(args: &Value, name: &str) -> Result<String, NodeError> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| node_error(VfsError::new(Errno::EINVAL), "tinysandbox", None))
}

fn bool_arg(args: &Value, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn i32_arg(args: &Value, name: &str) -> Result<i32, NodeError> {
    let value = args
        .get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| node_error(VfsError::new(Errno::EINVAL), "tinysandbox", None))?;
    i32::try_from(value).map_err(|_| node_error(VfsError::new(Errno::EINVAL), "tinysandbox", None))
}

fn usize_arg(args: &Value, name: &str) -> Result<usize, NodeError> {
    let value = args
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| node_error(VfsError::new(Errno::EINVAL), "tinysandbox", None))?;
    usize::try_from(value)
        .map_err(|_| node_error(VfsError::new(Errno::EINVAL), "tinysandbox", None))
}

fn u64_arg(args: &Value, name: &str) -> Result<Option<u64>, NodeError> {
    match args.get(name) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| node_error(VfsError::new(Errno::EINVAL), "tinysandbox", None)),
    }
}

fn bytes_arg(args: &Value, name: &str) -> Result<Vec<u8>, NodeError> {
    let data = args
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| node_error(VfsError::new(Errno::EINVAL), "tinysandbox", None))?;
    BASE64_STANDARD
        .decode(data)
        .map_err(|_| node_error(VfsError::new(Errno::EINVAL), "tinysandbox", None))
}

fn base64_encode(data: &[u8]) -> String {
    BASE64_STANDARD.encode(data)
}

fn node_error(err: VfsError, syscall: &'static str, path: Option<String>) -> NodeError {
    let errno = err.errno();
    NodeError {
        code: errno.name(),
        errno: libuv_errno(errno),
        message: node_errno_message(errno),
        syscall,
        path,
    }
}

fn libuv_errno(errno: Errno) -> i32 {
    // Verified with Node v24.15.0 via process.binding('uv').UV_*.
    match errno {
        Errno::EBADF => -9,
        Errno::EBUSY => -16,
        Errno::EXDEV => -18,
        Errno::EACCES => -13,
        Errno::EEXIST => -17,
        Errno::EFBIG => -27,
        Errno::EIO => -5,
        Errno::EINVAL => -22,
        Errno::EISDIR => -21,
        Errno::ENOENT => -2,
        Errno::ENOSPC => -28,
        Errno::ENOTDIR => -20,
        Errno::ENOTEMPTY => -66,
    }
}

fn node_errno_message(errno: Errno) -> &'static str {
    match errno {
        Errno::EBADF => "bad file descriptor",
        Errno::EBUSY => "resource busy or locked",
        Errno::EXDEV => "cross-device link not permitted",
        Errno::EACCES => "permission denied",
        Errno::EEXIST => "file already exists",
        Errno::EFBIG => "file too large",
        Errno::EIO => "i/o error",
        Errno::EINVAL => "invalid argument",
        Errno::EISDIR => "illegal operation on a directory",
        Errno::ENOENT => "no such file or directory",
        Errno::ENOSPC => "no space left on device",
        Errno::ENOTDIR => "not a directory",
        Errno::ENOTEMPTY => "directory not empty",
    }
}

fn memory<T>(caller: &mut Caller<'_, T>) -> wasmtime::Result<Memory> {
    match caller.get_export("memory") {
        Some(Extern::Memory(memory)) => Ok(memory),
        _ => Err(wasmtime::Error::msg("guest memory export missing")),
    }
}

fn read_utf8<T>(
    caller: &Caller<'_, T>,
    memory: &Memory,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<String> {
    String::from_utf8(read_bytes(caller, memory, ptr, len)?).map_err(wasmtime::Error::new)
}

fn read_bytes<T>(
    caller: &Caller<'_, T>,
    memory: &Memory,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<Vec<u8>> {
    let ptr = ptr_usize(ptr)?;
    let len = usize_len(len)?;
    let end = ptr
        .checked_add(len)
        .ok_or_else(|| wasmtime::Error::msg("invalid guest range"))?;
    memory
        .data(caller)
        .get(ptr..end)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| wasmtime::Error::msg("range exceeds guest memory"))
}

fn read_u32<T>(caller: &Caller<'_, T>, memory: &Memory, ptr: usize) -> wasmtime::Result<u32> {
    let mut bytes = [0_u8; 4];
    memory.read(caller, ptr, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_u32<T>(
    caller: &mut Caller<'_, T>,
    memory: &Memory,
    ptr: i32,
    value: u32,
) -> wasmtime::Result<()> {
    memory.write(caller, ptr_usize(ptr)?, &value.to_le_bytes())?;
    Ok(())
}

fn ptr_usize(ptr: i32) -> wasmtime::Result<usize> {
    usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("negative guest pointer"))
}

fn usize_len(len: i32) -> wasmtime::Result<usize> {
    usize::try_from(len).map_err(|_| wasmtime::Error::msg("negative guest length"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_runtime_initialization_keeps_the_winning_engine_ticking() {
        let artifact = Arc::new(precompile().expect("trusted artifact"));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|index| {
                let artifact = artifact.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    if index % 2 == 0 {
                        // SAFETY: the shared immutable bytes came from precompile.
                        #[allow(unsafe_code)]
                        let _ = unsafe { use_precompiled(&artifact) };
                    } else {
                        compiled_runtime().expect("initialize runtime");
                    }
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("initialization did not panic");
        }
        let runtime = compiled_runtime().expect("winning runtime");
        // A tiny module with one empty exported function checks epoch handling
        // at function entry. It cannot hang if the ticker is broken, and does
        // not rely on QuickJS's separate should_interrupt host callback.
        let module = Module::new(
            &runtime.engine,
            [
                0, 97, 115, 109, 1, 0, 0, 0, 1, 4, 1, 96, 0, 0, 3, 2, 1, 0, 7, 7, 1, 3, 114, 117,
                110, 0, 0, 10, 4, 1, 2, 0, 11,
            ],
        )
        .expect("valid empty wasm function");
        let mut store = Store::new(&runtime.engine, ());
        store.set_epoch_deadline(1);
        store.epoch_deadline_trap();
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("instantiate");
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .expect("export");
        let started = Instant::now();
        loop {
            thread::sleep(EPOCH_TICK);
            if let Err(error) = run.call(&mut store, ()) {
                assert!(is_epoch_timeout(&error), "{error}");
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "winning engine has no ticker"
            );
        }
    }

    #[test]
    fn bounded_serialization_stops_before_writing_an_oversized_value() {
        let value = json!({ "data": "x".repeat(512) });
        assert!(bounded_json(&value, 128).is_err());
        let exact = serde_json::to_vec(&value).unwrap();
        assert_eq!(bounded_json(&value, exact.len()).unwrap(), exact);
    }
}
