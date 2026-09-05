// Shared by build.rs and the jq runtime so precompiled engine settings agree.
pub(crate) fn jq_engine_config(target: Option<&str>) -> wasmtime::Result<wasmtime::Config> {
    let mut config = wasmtime::Config::new();
    config.epoch_interruption(true);
    config.max_wasm_stack(8 * 1024 * 1024);
    if let Some(target) = target {
        config.target(target)?;
    }
    Ok(config)
}
