//! Where the JavaScript runtime's machine code comes from.
//!
//! Running wasm means turning it into machine code, and Cranelift takes about
//! 400 ms and 29 MiB to do it. The build script does that work for this crate's
//! target and embeds the result, so no process compiles anything: the first
//! `js` command loads the artifact instead.
//!
//! Run with: cargo run --example js_precompiled

use std::time::Instant;

use serde_json::json;
use tinysandbox::js::RuntimeSource;
use tinysandbox::sandbox::Sandbox;

#[tokio::main]
async fn main() {
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
        "first js exec: {} ms from {} machine code -> {}",
        started.elapsed().as_millis(),
        match source {
            RuntimeSource::Precompiled => "precompiled",
            RuntimeSource::Compiled => "just-in-time compiled",
        },
        result.stdout.trim_end()
    );
    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(source, RuntimeSource::Precompiled);

    // Producing an artifact by hand is for targeting another machine, or
    // sharing one across processes that build separately.
    let started = Instant::now();
    let artifact = tinysandbox::js::precompile().expect("precompile quickjs");
    let path = std::env::temp_dir().join("tinysandbox-quickjs.cwasm");
    std::fs::write(&path, &artifact).expect("write artifact");
    println!(
        "precompiled quickjs in {} ms, wrote {} bytes to {}",
        started.elapsed().as_millis(),
        artifact.len(),
        path.display()
    );

    // Installing one replaces the embedded runtime, and only works before the
    // first `js` command, which already ran above. A stale or foreign artifact
    // is refused the same way, leaving the embedded runtime in place.
    match tinysandbox::js::use_precompiled(&artifact) {
        Ok(()) => println!("installed the artifact"),
        Err(err) => println!("kept the embedded runtime: {err}"),
    }
}
