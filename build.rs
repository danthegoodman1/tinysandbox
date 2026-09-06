//! Precompiles embedded jq and QuickJS modules so processes avoid Cranelift.
//!
//! The artifact lands in `OUT_DIR` and the crate embeds it. It is pinned to the
//! target's baseline CPU features, so it runs anywhere that architecture does.
//! jq is always isolated; QuickJS is included with the `js` feature. A target
//! unsupported by the build-time compiler falls back to runtime compilation.

fn main() {
    println!("cargo::rerun-if-changed=assets/quickjs.wasm");
    println!("cargo::rerun-if-changed=src/wasm_config.rs");
    println!("cargo::rustc-check-cfg=cfg(quickjs_precompiled)");
    println!("cargo::rerun-if-changed=assets/jq.wasm");
    println!("cargo::rustc-check-cfg=cfg(jq_precompiled)");
    precompile("jq");
    #[cfg(feature = "js")]
    precompile("quickjs");
}

mod engine {
    include!("src/wasm_config.rs");
}

fn precompile(name: &str) {
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target = std::env::var("TARGET").expect("TARGET");
    let wasm = std::fs::read(format!("assets/{name}.wasm")).expect("read embedded guest");

    let artifact = engine::engine_config(Some(&target))
        .and_then(|config| wasmtime::Engine::new(&config))
        .and_then(|engine| engine.precompile_module(&wasm));
    match artifact {
        Ok(artifact) => {
            std::fs::write(out_dir.join(format!("{name}.cwasm")), artifact)
                .expect("write guest artifact");
            println!("cargo::rustc-cfg={name}_precompiled");
        }
        Err(err) => {
            // A target Cranelift cannot codegen for is not a build failure: the
            // runtime still compiles the module itself.
            println!(
                "cargo::warning=tinysandbox: precompiling {name} for {target} failed ({err}); the first `{name}` command will compile it instead"
            );
        }
    }
}
