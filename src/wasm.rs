//! Shared compiler configuration and epoch clock for isolated guest stores.

use std::sync::OnceLock;
use std::time::Duration;
use wasmtime::Engine;

include!("wasm_config.rs");

pub(crate) const EPOCH_TICK: Duration = Duration::from_millis(5);

pub(crate) fn engine() -> wasmtime::Result<&'static Engine> {
    static ENGINE: OnceLock<wasmtime::Result<Engine>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let engine = Engine::new(&engine_config(None)?)?;
            let ticker = engine.clone();
            std::thread::Builder::new()
                .name("tinysandbox-wasm-epochs".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(EPOCH_TICK);
                        ticker.increment_epoch();
                    }
                })?;
            Ok(engine)
        })
        .as_ref()
        .map_err(|err| wasmtime::Error::msg(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use wasmtime::{Instance, Memory, MemoryType, Module, Store, Trap};

    #[test]
    fn shared_engine_keeps_store_memory_and_interruption_independent() {
        let engine = engine().unwrap();
        assert!(Engine::same(engine, super::engine().unwrap()));
        // An empty exported function checks epochs at entry without risking a
        // hung test if the shared ticker stops.
        let module = Module::new(
            engine,
            [
                0, 97, 115, 109, 1, 0, 0, 0, 1, 4, 1, 96, 0, 0, 3, 2, 1, 0, 7, 7, 1, 3, 114, 117,
                110, 0, 0, 10, 4, 1, 2, 0, 11,
            ],
        )
        .unwrap();
        let mut expiring = Store::new(engine, ());
        expiring.set_epoch_deadline(1);
        expiring.epoch_deadline_trap();
        let mut live = Store::new(engine, ());
        live.set_epoch_deadline(u64::MAX / 2);
        live.epoch_deadline_trap();
        let first_memory = Memory::new(&mut expiring, MemoryType::new(1, Some(1))).unwrap();
        let second_memory = Memory::new(&mut live, MemoryType::new(1, Some(1))).unwrap();
        first_memory.write(&mut expiring, 0, b"private").unwrap();
        assert_eq!(&second_memory.data(&live)[..7], &[0; 7]);
        let first = Instance::new(&mut expiring, &module, &[])
            .unwrap()
            .get_typed_func::<(), ()>(&mut expiring, "run")
            .unwrap();
        let second = Instance::new(&mut live, &module, &[])
            .unwrap()
            .get_typed_func::<(), ()>(&mut live, "run")
            .unwrap();
        let started = Instant::now();
        loop {
            std::thread::sleep(EPOCH_TICK);
            if let Err(error) = first.call(&mut expiring, ()) {
                assert!(matches!(
                    error.downcast_ref::<Trap>(),
                    Some(Trap::Interrupt)
                ));
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "epoch clock stopped"
            );
        }
        second.call(&mut live, ()).unwrap();
        assert_eq!(&first_memory.data(&expiring)[..7], b"private");
    }
}
