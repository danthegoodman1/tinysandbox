//! Wasmtime-hosted QuickJS command.
//!
//! The supported Node `fs` subset intentionally omits `statSync` timestamp,
//! inode, uid, and gid fields until the VFS exposes them. JS execution uses the
//! sandbox wall-clock budget, but timeout handling returns a clean 124 result
//! and discards buffered JS stdout/stderr instead of returning partial output.
//! Module stack traces still reflect QuickJS details: wrapper prefixes leave a
//! line-1 column offset, method frames are named like `at boom`, and visible
//! `<tinysandbox>` glue frames can appear below user frames.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use wasmtime::{
    Caller, Config, Engine, Extern, InstancePre, Linker, Memory, Module, ResourceLimiter, Store,
    Trap,
};

use crate::sandbox::command::{Command, CommandContext, CommandFuture, CommandResult};
use crate::sandbox::fs::{Fs, join_path};
use crate::vfs::{Errno, FileHandle, FileType, Metadata, OpenMode, VfsError};

const QUICKJS_WASM: &[u8] = include_bytes!("../../assets/quickjs.wasm");
const EPOCH_TICK: Duration = Duration::from_millis(5);
const MAX_HOST_READ_BYTES: usize = 16 * 1024 * 1024;
const QUICKJS_HOST_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
const QUICKJS_WASMTIME_STACK_BYTES: usize = 8 * 1024 * 1024;

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
            ..
        } = ctx;

        let invocation = match Invocation::parse(args, &fs).await {
            Ok(invocation) => invocation,
            Err(message) => {
                let _ = stderr.write_all(message.as_bytes()).await;
                return CommandResult::new(1);
            }
        };

        let result = match tokio::task::spawn_blocking(move || {
            run_quickjs_on_host_stack(
                invocation,
                env,
                cwd,
                fs,
                limits.wasm_memory_bytes,
                limits.wall_time,
            )
        })
        .await
        {
            Ok(result) => result,
            Err(err) => JsRunResult {
                exit_code: 1,
                stdout: Vec::new(),
                stderr: format!("js: runtime task failed: {err}\n").into_bytes(),
                peak_wasm_memory_bytes: 0,
            },
        };

        let _ = stdout.write_all(&result.stdout).await;
        let _ = stderr.write_all(&result.stderr).await;
        CommandResult::new(result.exit_code).with_peak_wasm_memory(result.peak_wasm_memory_bytes)
    })
}

fn run_quickjs_on_host_stack(
    invocation: Invocation,
    env: BTreeMap<String, String>,
    cwd: String,
    fs: Fs,
    wasm_memory_bytes: usize,
    wall_time: Duration,
) -> JsRunResult {
    match thread::Builder::new()
        .name("tinysandbox-js-runtime".to_owned())
        .stack_size(QUICKJS_HOST_THREAD_STACK_BYTES)
        .spawn(move || run_quickjs(invocation, env, cwd, fs, wasm_memory_bytes, wall_time))
    {
        Ok(handle) => match handle.join() {
            Ok(result) => result,
            Err(_) => JsRunResult {
                exit_code: 1,
                stdout: Vec::new(),
                stderr: b"js: runtime task panicked\n".to_vec(),
                peak_wasm_memory_bytes: 0,
            },
        },
        Err(err) => JsRunResult {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: format!("js: failed to start runtime thread: {err}\n").into_bytes(),
            peak_wasm_memory_bytes: 0,
        },
    }
}

struct Invocation {
    code: String,
    script_path: String,
    argv: Vec<String>,
}

impl Invocation {
    async fn parse(args: Vec<String>, fs: &Fs) -> Result<Self, String> {
        match args.as_slice() {
            [] => Err("js: usage: js [-e code] script.js [args...]\n".to_owned()),
            [flag, ..] if flag == "-e" => {
                if args.len() < 2 {
                    return Err("js: option requires an argument -- e\n".to_owned());
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
                let data = fs.read_file(script).await.map_err(|err| {
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
}

struct JsRunResult {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    peak_wasm_memory_bytes: usize,
}

fn run_quickjs(
    invocation: Invocation,
    env: BTreeMap<String, String>,
    cwd: String,
    fs: Fs,
    wasm_memory_bytes: usize,
    wall_time: Duration,
) -> JsRunResult {
    match run_quickjs_inner(invocation, env, cwd, fs, wasm_memory_bytes, wall_time) {
        Ok(result) => result,
        Err(err) if is_epoch_timeout(&err) => JsRunResult {
            exit_code: 124,
            stdout: Vec::new(),
            stderr: b"js: command timed out\n".to_vec(),
            peak_wasm_memory_bytes: 0,
        },
        Err(err) => JsRunResult {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: format!("js: {err}\n").into_bytes(),
            peak_wasm_memory_bytes: 0,
        },
    }
}

fn run_quickjs_inner(
    invocation: Invocation,
    env: BTreeMap<String, String>,
    cwd: String,
    fs: Fs,
    wasm_memory_bytes: usize,
    wall_time: Duration,
) -> wasmtime::Result<JsRunResult> {
    let compiled = compiled_runtime()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(wasmtime::Error::new)?;
    let mut store = Store::new(
        &compiled.engine,
        HostState::new(fs, runtime, wasm_memory_bytes),
    );
    store.limiter(|state| &mut state.limiter);
    store.set_epoch_deadline(epoch_ticks(wall_time));
    store.epoch_deadline_trap();

    let instance = compiled.pre.instantiate(&mut store)?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| wasmtime::Error::msg("quickjs wasm did not export memory"))?;
    let initial_memory = memory.data_size(&store);
    store.data_mut().limiter.record_peak(initial_memory);
    if initial_memory > wasm_memory_bytes {
        return Err(wasmtime::Error::msg(
            "tinysandbox wasm memory limit exceeded",
        ));
    }

    let alloc = instance.get_typed_func::<i32, i32>(&mut store, "tinysandbox_alloc")?;
    let free = instance.get_typed_func::<i32, ()>(&mut store, "tinysandbox_free")?;
    let run = instance.get_typed_func::<(i32, i32), i32>(&mut store, "tinysandbox_run")?;

    let config = GuestConfig {
        code: &invocation.code,
        script_path: &invocation.script_path,
        argv: &invocation.argv,
        env: &env,
        cwd: &cwd,
    };
    let input = serde_json::to_vec(&config).map_err(wasmtime::Error::new)?;
    let len = i32::try_from(input.len()).map_err(|_| wasmtime::Error::msg("script too large"))?;
    let ptr = alloc.call(&mut store, len)?;
    memory.write(&mut store, ptr_usize(ptr)?, &input)?;
    let exit_code = match run.call(&mut store, (ptr, len)) {
        Ok(exit_code) => exit_code,
        Err(_) if store.data().limiter.limit_exceeded => {
            return Ok(JsRunResult {
                exit_code: 1,
                stdout: store.data().stdout.clone(),
                stderr: b"js: wasm memory limit exceeded\n".to_vec(),
                peak_wasm_memory_bytes: store.data().limiter.peak_memory_bytes,
            });
        }
        Err(err) => return Err(err),
    };
    free.call(&mut store, ptr)?;

    let state = store.data();
    Ok(JsRunResult {
        exit_code,
        stdout: state.stdout.clone(),
        stderr: state.stderr.clone(),
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
    pre: InstancePre<HostState>,
}

fn compiled_runtime() -> wasmtime::Result<&'static CompiledRuntime> {
    static RUNTIME: OnceLock<wasmtime::Result<CompiledRuntime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            let mut config = Config::new();
            config.epoch_interruption(true);
            config.max_wasm_stack(QUICKJS_WASMTIME_STACK_BYTES);
            let engine = Engine::new(&config)?;
            start_epoch_thread(engine.clone());
            let module = Module::new(&engine, QUICKJS_WASM)?;
            let mut linker = Linker::new(&engine);
            define_tinysandbox_imports(&mut linker)?;
            define_wasi_imports(&mut linker)?;
            let pre = linker.instantiate_pre(&module)?;
            Ok(CompiledRuntime { engine, pre })
        })
        .as_ref()
        .map_err(|err| wasmtime::Error::msg(err.to_string()))
}

fn start_epoch_thread(engine: Engine) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        thread::Builder::new()
            .name("tinysandbox-js-epoch".to_owned())
            .spawn(move || {
                loop {
                    thread::sleep(EPOCH_TICK);
                    engine.increment_epoch();
                }
            })
            .expect("start tinysandbox js epoch thread");
    });
}

struct HostState {
    fs: Fs,
    runtime: tokio::runtime::Runtime,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    response: Vec<u8>,
    fds: BTreeMap<i32, OpenFile>,
    next_fd: i32,
    limiter: WasmLimiter,
    rng: u64,
    started: Instant,
}

impl HostState {
    fn new(fs: Fs, runtime: tokio::runtime::Runtime, memory_limit: usize) -> Self {
        Self {
            fs,
            runtime,
            stdout: Vec::new(),
            stderr: Vec::new(),
            response: Vec::new(),
            fds: BTreeMap::new(),
            next_fd: 3,
            limiter: WasmLimiter::new(memory_limit),
            rng: 0x7468_696e_626f_7821,
            started: Instant::now(),
        }
    }

    fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}

#[derive(Clone)]
struct OpenFile {
    handle: FileHandle,
    position: u64,
    path: String,
}

struct WasmLimiter {
    max_memory_bytes: usize,
    peak_memory_bytes: usize,
    limit_exceeded: bool,
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
        "host_call",
        |mut caller: Caller<'_, HostState>,
         op_ptr: i32,
         op_len: i32,
         json_ptr: i32,
         json_len: i32|
         -> wasmtime::Result<i32> {
            let memory = memory(&mut caller)?;
            let op = read_utf8(&caller, &memory, op_ptr, op_len)?;
            let input = read_utf8(&caller, &memory, json_ptr, json_len)?;
            let args: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
            let response = handle_host_call(caller.data_mut(), &op, args);
            caller.data_mut().response =
                serde_json::to_vec(&response).map_err(wasmtime::Error::new)?;
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
            let data = caller.data().response.clone();
            let n = data.len().min(len);
            memory.write(&mut caller, ptr_usize(ptr)?, &data[..n])?;
            Ok(i32::try_from(n).unwrap_or(i32::MAX))
        },
    )?;
    linker.func_wrap(
        "tinysandbox",
        "write_stdout",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> wasmtime::Result<i32> {
            let memory = memory(&mut caller)?;
            let data = read_bytes(&caller, &memory, ptr, len)?;
            caller.data_mut().stdout.extend_from_slice(&data);
            Ok(len)
        },
    )?;
    linker.func_wrap(
        "tinysandbox",
        "write_stderr",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> wasmtime::Result<i32> {
            let memory = memory(&mut caller)?;
            let data = read_bytes(&caller, &memory, ptr, len)?;
            caller.data_mut().stderr.extend_from_slice(&data);
            Ok(len)
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
        let data = read_bytes(&caller, &memory, ptr, len)?;
        total = total.saturating_add(u32::try_from(data.len()).unwrap_or(u32::MAX));
        if fd == 1 {
            caller.data_mut().stdout.extend_from_slice(&data);
        } else {
            caller.data_mut().stderr.extend_from_slice(&data);
        }
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
    error: Option<NodeError>,
}

impl HostResponse {
    fn value(value: Value) -> Self {
        Self {
            value: Some(value),
            error: None,
        }
    }

    fn error(error: NodeError) -> Self {
        Self {
            value: None,
            error: Some(error),
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
    match handle_host_call_result(state, op, &args) {
        Ok(value) => HostResponse::value(value),
        Err(err) => HostResponse::error(err),
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
                .block_on(state.fs.read_file(&path))
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
                    path,
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
            let len = clamped_read_len(state, &file, read_offset, len);
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
                .block_on(state.fs.read_file(&src))
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

fn clamped_read_len(state: &HostState, file: &OpenFile, offset: u64, requested: usize) -> usize {
    let capped = requested.min(MAX_HOST_READ_BYTES);
    match state.block_on(state.fs.stat(&file.path)) {
        Ok(metadata) if metadata.file_type == FileType::File => {
            let remaining = metadata.len.saturating_sub(offset);
            capped.min(usize::try_from(remaining).unwrap_or(usize::MAX))
        }
        _ => capped,
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
        Errno::EACCES => -13,
        Errno::EEXIST => -17,
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
        Errno::EACCES => "permission denied",
        Errno::EEXIST => "file already exists",
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
    let mut out = vec![0; len];
    memory.read(caller, ptr, &mut out)?;
    Ok(out)
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
