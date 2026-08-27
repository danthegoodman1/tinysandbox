// Included by both `src/js/mod.rs` and `build.rs`, so the build-time
// precompile and the runtime agree on engine settings. An artifact built with
// different settings is rejected at load time, which costs a silent fallback to
// compiling rather than an error, so the two must not drift.

const QUICKJS_WASMTIME_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Builds the Wasmtime configuration for the QuickJS runtime.
///
/// `target` pins code generation to a triple with that ISA's baseline CPU
/// features, which is what the build script wants: the artifact then runs on
/// any CPU of that architecture, not just one as new as the build machine.
/// Passing `None` lets Wasmtime detect the host.
pub(crate) fn quickjs_engine_config(target: Option<&str>) -> wasmtime::Result<wasmtime::Config> {
    let mut config = wasmtime::Config::new();
    config.epoch_interruption(true);
    config.max_wasm_stack(QUICKJS_WASMTIME_STACK_BYTES);
    if let Some(target) = target {
        config.target(target)?;
    }
    Ok(config)
}
