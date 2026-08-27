//! Precompiles the embedded QuickJS module so no process pays for Cranelift.
//!
//! The artifact lands in `OUT_DIR` and the crate embeds it. It is pinned to the
//! target's baseline CPU features, so it runs anywhere that architecture does.
//! When the `js` feature is off, or Cranelift cannot target this machine, the
//! crate falls back to compiling at runtime.

fn main() {
    println!("cargo::rerun-if-changed=assets/quickjs.wasm");
    println!("cargo::rerun-if-changed=src/js/engine_config.rs");
    println!("cargo::rustc-check-cfg=cfg(quickjs_precompiled)");
    precompile();
}

#[cfg(feature = "js")]
mod engine {
    include!("src/js/engine_config.rs");
}

#[cfg(feature = "js")]
fn precompile() {
    use engine::quickjs_engine_config;

    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target = std::env::var("TARGET").expect("TARGET");
    let wasm = std::fs::read("assets/quickjs.wasm").expect("read assets/quickjs.wasm");

    let artifact = quickjs_engine_config(Some(&target))
        .and_then(|config| wasmtime::Engine::new(&config))
        .and_then(|engine| engine.precompile_module(&wasm));
    match artifact {
        Ok(artifact) => {
            std::fs::write(out_dir.join("quickjs.cwasm"), artifact).expect("write quickjs.cwasm");
            println!("cargo::rustc-cfg=quickjs_precompiled");
        }
        Err(err) => {
            // A target Cranelift cannot codegen for is not a build failure: the
            // runtime still compiles the module itself.
            println!(
                "cargo::warning=tinysandbox: precompiling quickjs for {target} failed ({err}); the first `js` command will compile it instead"
            );
        }
    }
}

#[cfg(not(feature = "js"))]
fn precompile() {}
