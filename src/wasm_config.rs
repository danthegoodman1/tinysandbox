// Included by build.rs and the runtime so embedded native artifacts use the
// same engine settings. A target triple selects baseline CPU features for
// portable build output; None lets the runtime detect the host.
pub(crate) fn engine_config(target: Option<&str>) -> wasmtime::Result<wasmtime::Config> {
    let mut config = wasmtime::Config::new();
    config.epoch_interruption(true);
    config.max_wasm_stack(8 * 1024 * 1024);
    if let Some(target) = target {
        config.target(target)?;
    }
    Ok(config)
}
