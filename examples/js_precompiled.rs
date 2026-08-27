//! Where the JavaScript runtime's machine code comes from.
//!
//! Running wasm means turning it into machine code, and Cranelift takes about
//! 400 ms and 29 MiB to do it. The build script does that work for this crate's
//! target and embeds the result, so no process compiles anything: the first
//! `js` command loads the artifact instead.
//!
//! Run with: cargo run --example js_precompiled
//!
//! The `precompile` and `use_precompiled` APIs shown below are for producing an
//! artifact yourself — to target another machine, or to share one across
//! processes that build separately. The halves also run on their own:
//!
//! ```sh
//! cargo run --example js_precompiled -- build /tmp/quickjs.cwasm
//! cargo run --example js_precompiled -- run /tmp/quickjs.cwasm
//! ```

use std::path::PathBuf;
use std::time::Instant;

use serde_json::json;
use tinysandbox::js::RuntimeSource;
use tinysandbox::sandbox::Sandbox;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let default_path = std::env::temp_dir().join("tinysandbox-quickjs.cwasm");
    match args.first().map(String::as_str) {
        Some("build") => build(path_arg(&args, default_path)),
        Some("run") => run(Some(path_arg(&args, default_path))).await,
        None => run(None).await,
        Some(other) => panic!("unknown mode '{other}'; expected 'build' or 'run'"),
    }
}

fn build(path: PathBuf) {
    // This is what build.rs already did for the crate's own target.
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

async fn run(path: Option<PathBuf>) {
    // Installing an artifact is optional. Skipping it leaves the one the build
    // script embedded, which is already machine code.
    if let Some(path) = path {
        match std::fs::read(&path) {
            Ok(artifact) => {
                let started = Instant::now();
                match tinysandbox::js::use_precompiled(&artifact) {
                    Ok(()) => println!(
                        "run: installed {} in {} ms",
                        path.display(),
                        started.elapsed().as_millis()
                    ),
                    // A stale or foreign artifact is not fatal: the embedded one
                    // still serves, and a compile is the last resort.
                    Err(err) => println!("run: keeping the embedded runtime: {err}"),
                }
            }
            Err(err) => println!("run: no artifact at {}: {err}", path.display()),
        }
    }

    // Host globals are unaffected by where the machine code came from: they are
    // installed per command from the sandbox's registry, so one artifact serves
    // every sandbox and every tool surface in the process.
    let sandbox = Sandbox::builder()
        .js_global("whoami", |_args| async { Ok(json!("agent-1")) })
        .js_global("tools.answer", |_args| async { Ok(json!(42)) })
        .build();
    let started = Instant::now();
    let result = sandbox
        .exec("js -e 'console.log(whoami(), tools.answer())'")
        .await;
    let source = tinysandbox::js::runtime_source().expect("runtime source");
    println!(
        "run: first js exec took {} ms from {} machine code -> {}",
        started.elapsed().as_millis(),
        match source {
            RuntimeSource::Precompiled => "precompiled",
            RuntimeSource::Compiled => "just-in-time compiled",
        },
        result.stdout.trim_end()
    );
    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "agent-1 42\n");
    assert_eq!(source, RuntimeSource::Precompiled);
}

fn path_arg(args: &[String], fallback: PathBuf) -> PathBuf {
    args.get(1).map(PathBuf::from).unwrap_or(fallback)
}
