//! Precompiling the QuickJS module once and loading it in a later process.
//!
//! The first `js` command in a process asks Cranelift to compile the embedded
//! `quickjs.wasm` into machine code, which costs roughly 400 ms and 29 MiB of
//! resident memory. A build step can pay that instead: `precompile` returns the
//! machine code, and `use_precompiled` loads it in later processes.
//!
//! The artifact is the compiled QuickJS interpreter, nothing else. Host
//! globals are per-command configuration, not part of the module, so one
//! artifact serves every sandbox and every tool surface in the process — the
//! `run` half below binds globals to show that.
//!
//! Run with: cargo run --example js_precompiled
//!
//! With no arguments the example writes an artifact to the system temp
//! directory and re-runs itself to show the load path. The halves also run on
//! their own:
//!
//! ```sh
//! cargo run --example js_precompiled -- build /tmp/quickjs.cwasm
//! cargo run --example js_precompiled -- run /tmp/quickjs.cwasm
//! ```

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use serde_json::json;
use tinysandbox::sandbox::Sandbox;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let default_path = std::env::temp_dir().join("tinysandbox-quickjs.cwasm");
    match args.first().map(String::as_str) {
        Some("build") => build(path_arg(&args, default_path)),
        Some("run") => run(path_arg(&args, default_path)).await,
        None => {
            build(default_path.clone());
            // A fresh process is the point: compilation is cached per process,
            // so the saving shows up in the next one.
            let exe = std::env::current_exe().expect("current exe");
            let status = Command::new(exe)
                .arg("run")
                .arg(&default_path)
                .status()
                .expect("re-run example");
            assert!(status.success());
        }
        Some(other) => panic!("unknown mode '{other}'; expected 'build' or 'run'"),
    }
}

fn build(path: PathBuf) {
    let started = Instant::now();
    let artifact = tinysandbox::js::precompile().expect("precompile quickjs");
    let compile_ms = started.elapsed().as_millis();
    std::fs::write(&path, &artifact).expect("write artifact");
    println!(
        "build: compiled quickjs in {compile_ms} ms, wrote {} bytes to {}",
        artifact.len(),
        path.display()
    );
}

async fn run(path: PathBuf) {
    // A missing, stale, or foreign artifact is not fatal: report it and let the
    // first `js` command compile the module the usual way.
    match std::fs::read(&path) {
        Ok(artifact) => {
            let started = Instant::now();
            match tinysandbox::js::use_precompiled(&artifact) {
                Ok(()) => println!(
                    "run: loaded {} in {} ms",
                    path.display(),
                    started.elapsed().as_millis()
                ),
                Err(err) => println!("run: falling back to compiling quickjs: {err}"),
            }
        }
        Err(err) => println!("run: no artifact at {}: {err}", path.display()),
    }

    // Host globals are unaffected by where the machine code came from: they
    // are installed per command from the sandbox's registry.
    let sandbox = Sandbox::builder()
        .js_global("whoami", |_args| async { Ok(json!("agent-1")) })
        .js_global("tools.answer", |_args| async { Ok(json!(42)) })
        .build();
    let started = Instant::now();
    let result = sandbox
        .exec("js -e 'console.log(whoami(), tools.answer())'")
        .await;
    println!(
        "run: first js exec took {} ms -> {}",
        started.elapsed().as_millis(),
        result.stdout.trim_end()
    );
    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "agent-1 42\n");
}

fn path_arg(args: &[String], fallback: PathBuf) -> PathBuf {
    args.get(1).map(PathBuf::from).unwrap_or(fallback)
}
