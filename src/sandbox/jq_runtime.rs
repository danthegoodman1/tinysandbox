//! Capability-free jq guest with bounded linear memory and engine interruption.

use std::io;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use wasmtime::{
    Caller, Engine, Linker, Memory, Module, ResourceLimiter, Store, Trap, UpdateDeadline,
};

use super::command::{CommandResult, Limits};
use super::fs::STREAM_CHUNK_BYTES;
use super::jq_protocol::{JqInputSource, JqOptions, JqRequest};

mod engine_config {
    include!("jq_engine_config.rs");
}

const EPOCH_TICK: Duration = Duration::from_millis(5);
pub(crate) const WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

pub(crate) enum JqStreamMessage {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Done(CommandResult),
}

struct CompiledRuntime {
    engine: Engine,
    module: Module,
}

static RUNTIME: OnceLock<wasmtime::Result<CompiledRuntime>> = OnceLock::new();

fn compiled_runtime() -> wasmtime::Result<&'static CompiledRuntime> {
    RUNTIME
        .get_or_init(|| {
            let engine = Engine::new(&engine_config::jq_engine_config(None)?)?;
            let module = load_module(&engine)?;
            let ticker = engine.clone();
            std::thread::Builder::new()
                .name("tinysandbox-jq-epochs".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(EPOCH_TICK);
                        ticker.increment_epoch();
                    }
                })?;
            Ok(CompiledRuntime { engine, module })
        })
        .as_ref()
        .map_err(|err| wasmtime::Error::msg(err.to_string()))
}

fn load_module(engine: &Engine) -> wasmtime::Result<Module> {
    #[cfg(jq_precompiled)]
    #[allow(unsafe_code)]
    // SAFETY: only our build script's artifact from the fixed jq guest is embedded.
    if let Ok(module) = unsafe {
        Module::deserialize(
            engine,
            include_bytes!(concat!(env!("OUT_DIR"), "/jq.cwasm")),
        )
    } {
        return Ok(module);
    }
    Module::new(engine, include_bytes!("../../assets/jq.wasm"))
}

struct State {
    inputs: Vec<Vec<u8>>,
    output: mpsc::Sender<JqStreamMessage>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    memory_limit: usize,
    peak_memory: usize,
    memory_exceeded: bool,
    io_runtime: OnceLock<tokio::runtime::Runtime>,
}

impl State {
    fn checkpoint(&self) -> wasmtime::Result<()> {
        if self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            Err(Trap::Interrupt.into())
        } else {
            Ok(())
        }
    }
}

impl ResourceLimiter for State {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.memory_limit {
            self.memory_exceeded = true;
            return Err(wasmtime::Error::msg("jq memory limit exceeded"));
        }
        self.peak_memory = self.peak_memory.max(desired);
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // The fixed guest's function table is small and never needs to grow.
        Ok(desired <= 16_384)
    }

    fn memories(&self) -> usize {
        1
    }
    fn tables(&self) -> usize {
        1
    }
    fn instances(&self) -> usize {
        1
    }
}

pub(crate) fn run(
    options: JqOptions,
    inputs: Vec<JqInputSource>,
    output: mpsc::Sender<JqStreamMessage>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    limits: Limits,
) {
    let result = run_inner(
        options,
        inputs,
        &output,
        deadline,
        Arc::clone(&cancelled),
        limits,
    );
    let (result, error) = match result {
        Ok(result) => result,
        Err(err) => (CommandResult::new(5), Some(format!("jq: {err}\n"))),
    };
    let io_runtime = OnceLock::new();
    if let Some(error) = error {
        let _ = send(
            &output,
            JqStreamMessage::Stderr(error.into_bytes()),
            deadline,
            &cancelled,
            &io_runtime,
        );
    }
    let _ = send(
        &output,
        JqStreamMessage::Done(result),
        deadline,
        &cancelled,
        &io_runtime,
    );
}

fn run_inner(
    options: JqOptions,
    inputs: Vec<JqInputSource>,
    output: &mpsc::Sender<JqStreamMessage>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    limits: Limits,
) -> wasmtime::Result<(CommandResult, Option<String>)> {
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Ok((CommandResult::new(124), None));
    }
    let runtime = compiled_runtime()?;
    let request = JqRequest {
        options,
        paths: inputs.iter().map(|source| source.path.clone()).collect(),
    };
    let mut config = BoundedConfig {
        bytes: Vec::new(),
        limit: limits.host_input_bytes.max(limits.shell_input_bytes),
    };
    serde_json::to_writer(&mut config, &request)?;
    let inputs = std::iter::once(config.bytes)
        .chain(inputs.into_iter().map(|source| source.data))
        .collect();
    let mut store = Store::new(
        &runtime.engine,
        State {
            inputs,
            output: output.clone(),
            deadline,
            cancelled,
            memory_limit: limits.jq_memory_bytes,
            peak_memory: 0,
            memory_exceeded: false,
            io_runtime: OnceLock::new(),
        },
    );
    store.limiter(|state| state);
    // Recheck the absolute deadline and cancellation every epoch, including
    // loops that never emit values or call imports. No evaluator cooperation.
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|store| {
        store.data().checkpoint()?;
        Ok(UpdateDeadline::Continue(1))
    });
    let mut linker = Linker::new(&runtime.engine);
    define_imports(&mut linker)?;
    let result = (|| {
        store.data().checkpoint()?;
        let instance = linker.instantiate(&mut store, &runtime.module)?;
        let run = instance.get_typed_func::<(), i32>(&mut store, "run")?;
        run.call(&mut store, ())
    })();
    let state = store.data();
    let (exit_code, error) = match result {
        _ if state.checkpoint().is_err() => (124, None),
        Err(_) if state.memory_exceeded => (5, Some("jq: memory limit exceeded\n".into())),
        Ok(code) => (code, None),
        Err(err) if matches!(err.downcast_ref::<Trap>(), Some(Trap::Interrupt)) => (124, None),
        Err(err) => (5, Some(format!("jq: guest execution failed: {err}\n"))),
    };
    Ok((
        CommandResult::new(exit_code).with_peak_wasm_memory(state.peak_memory),
        error,
    ))
}

struct BoundedConfig {
    bytes: Vec<u8>,
    limit: usize,
}

impl io::Write for BoundedConfig {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other(
                "jq configuration exceeds host input limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn memory(caller: &mut Caller<'_, State>) -> wasmtime::Result<Memory> {
    caller
        .get_export("memory")
        .and_then(|item| item.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("jq guest did not export memory"))
}

fn send(
    output: &mpsc::Sender<JqStreamMessage>,
    message: JqStreamMessage,
    deadline: Instant,
    cancelled: &AtomicBool,
    io_runtime: &OnceLock<tokio::runtime::Runtime>,
) -> wasmtime::Result<bool> {
    let message = match output.try_send(message) {
        Ok(()) => return Ok(true),
        Err(mpsc::error::TrySendError::Closed(_)) => return Ok(false),
        Err(mpsc::error::TrySendError::Full(message)) => message,
    };
    // Host backpressure is also bounded by the deadline. A receiver may remain
    // alive without polling, so blocking_send alone could pin a worker forever.
    // This timer has its own driver: it works even if the caller stops polling.
    if io_runtime.get().is_none() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        let _ = io_runtime.set(runtime);
    }
    io_runtime
        .get()
        .expect("initialized above")
        .block_on(async {
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() || cancelled.load(Ordering::Acquire) {
                    return Err(Trap::Interrupt.into());
                }
                match tokio::time::timeout(remaining.min(EPOCH_TICK), output.reserve()).await {
                    Ok(Ok(permit)) => {
                        permit.send(message);
                        return Ok(true);
                    }
                    Ok(Err(_)) => return Ok(false),
                    Err(_) => {}
                }
            }
        })
}

fn transfer_range(ptr: i32, len: i32) -> wasmtime::Result<std::ops::Range<usize>> {
    let start = ptr as u32 as usize;
    let len = usize::try_from(len)?;
    if len > STREAM_CHUNK_BYTES {
        return Err(wasmtime::Error::msg("jq host transfer exceeds chunk limit"));
    }
    let end = start
        .checked_add(len)
        .ok_or_else(|| wasmtime::Error::msg("jq pointer overflow"))?;
    Ok(start..end)
}

fn define_imports(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    linker.func_wrap(
        "tinysandbox_jq",
        "input_len",
        |caller: Caller<'_, State>, index: i32| -> wasmtime::Result<i32> {
            caller.data().checkpoint()?;
            Ok(usize::try_from(index)
                .ok()
                .and_then(|index| caller.data().inputs.get(index))
                .and_then(|input| i32::try_from(input.len()).ok())
                .unwrap_or(-1))
        },
    )?;
    linker.func_wrap(
        "tinysandbox_jq",
        "read_input",
        |mut caller: Caller<'_, State>,
         index: i32,
         offset: i32,
         ptr: i32,
         len: i32|
         -> wasmtime::Result<i32> {
            caller.data().checkpoint()?;
            let range = transfer_range(ptr, len)?;
            let memory = memory(&mut caller)?;
            let (memory, state) = memory.data_and_store_mut(&mut caller);
            let source = usize::try_from(index)
                .ok()
                .and_then(|index| state.inputs.get(index))
                .and_then(|input| {
                    usize::try_from(offset)
                        .ok()
                        .and_then(|offset| input.get(offset..))
                })
                .ok_or_else(|| wasmtime::Error::msg("invalid jq input range"))?;
            let target = memory
                .get_mut(range)
                .ok_or_else(|| wasmtime::Error::msg("invalid jq memory range"))?;
            let n = source.len().min(target.len());
            target[..n].copy_from_slice(&source[..n]);
            Ok(n as i32)
        },
    )?;
    linker.func_wrap(
        "tinysandbox_jq",
        "write_output",
        |mut caller: Caller<'_, State>, kind: i32, ptr: i32, len: i32| -> wasmtime::Result<i32> {
            caller.data().checkpoint()?;
            let range = transfer_range(ptr, len)?;
            let memory = memory(&mut caller)?;
            let data = memory
                .data(&caller)
                .get(range)
                .ok_or_else(|| wasmtime::Error::msg("invalid jq output range"))?
                .to_vec();
            let message = match kind {
                1 => JqStreamMessage::Stdout(data),
                2 => JqStreamMessage::Stderr(data),
                _ => return Err(wasmtime::Error::msg("invalid jq output stream")),
            };
            let state = caller.data();
            Ok(
                if send(
                    &state.output,
                    message,
                    state.deadline,
                    &state.cancelled,
                    &state.io_runtime,
                )? {
                    len
                } else {
                    -1
                },
            )
        },
    )?;
    linker.func_wrap(
        "tinysandbox_jq",
        "now",
        |caller: Caller<'_, State>| -> wasmtime::Result<f64> {
            caller.data().checkpoint()?;
            Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64())
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::jq_protocol::parse_jq_args;
    use super::*;

    #[tokio::test]
    async fn cancellation_interrupts_an_entered_guest_and_releases_its_worker() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (output, mut receiver) = mpsc::channel(4);
        let options = parse_jq_args(vec![
            "-nr".into(),
            "\"ready\", (reduce range(0;1000000000) as $i (0; .))".into(),
        ])
        .unwrap();
        let worker = std::thread::Builder::new()
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                run(
                    options,
                    Vec::new(),
                    output,
                    Instant::now() + Duration::from_secs(60),
                    worker_cancelled,
                    Limits::default(),
                );
            })
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first, JqStreamMessage::Stdout(ref bytes) if bytes == b"ready\n"));
        cancelled.store(true, Ordering::Release);
        let done = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("engine must interrupt the running evaluator, not just return a timeout")
            .unwrap();
        assert!(matches!(done, JqStreamMessage::Done(result) if result.exit_code == 124));
        // Channel closes only when all sender clones and the Store have dropped.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .is_none()
        );
        worker.join().unwrap();
    }

    #[test]
    fn host_transfers_reject_oversized_or_invalid_lengths_before_copying() {
        assert!(transfer_range(0, STREAM_CHUNK_BYTES as i32 + 1).is_err());
        assert!(transfer_range(0, -1).is_err());
        assert_eq!(transfer_range(100, 3).unwrap(), 100..103);
    }

    #[test]
    fn a_live_but_unpolled_output_receiver_cannot_pin_the_worker() {
        compiled_runtime().unwrap();
        let (output, _unpolled_receiver) = mpsc::channel(4);
        let (finished, received) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                let options =
                    parse_jq_args(vec!["-n".into(), "range(0;1000000000)".into()]).unwrap();
                run(
                    options,
                    Vec::new(),
                    output,
                    Instant::now() + Duration::from_millis(100),
                    Arc::new(AtomicBool::new(false)),
                    Limits::default(),
                );
                finished.send(()).unwrap();
            })
            .unwrap();
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("output backpressure must end at the deadline without the caller polling");
        worker.join().unwrap();
    }
}
