//! Multi-file JavaScript with require, the fs API, limits, and metrics.
//!
//! Run with: cargo run --example js_scripts

use std::time::Duration;

use tinysandbox::sandbox::{Limits, Sandbox};

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder()
        .persist_session(true)
        .limits(Limits {
            wasm_memory_bytes: 32 * 1024 * 1024,
            ..Limits::default()
        })
        .build();

    // Persistent cwd lets the example build a small multi-file program across
    // separate exec calls, like one long shell session.
    sandbox
        .exec(concat!(
            "mkdir -p /app && cd /app && ",
            r#"echo 'exports.stats = (text) => {
  const words = text.split(/\s+/).filter(Boolean)
  return { words: words.length, unique: new Set(words).size }
}' > helper.js"#,
        ))
        .await;
    sandbox
        .exec(
            r#"echo 'const fs = require("fs")
const { stats } = require("./helper.js")
const text = fs.readFileSync(process.argv[2], "utf8")
const result = stats(text)
console.log(JSON.stringify(result))
fs.writeFileSync("/app/stats.json", JSON.stringify(result))' > main.js"#,
        )
        .await;

    sandbox
        .exec("echo 'the quick brown fox jumps over the lazy dog' > input.txt")
        .await;

    // Scripts run under Wasmtime with the sandbox's memory/CPU limits and see
    // only the VFS. require resolves like Node (cache, cycles, index.js...).
    let result = sandbox.exec("js main.js input.txt").await;
    print!("stdout: {}", result.stdout);
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "{\"words\":9,\"unique\":8}\n");

    // Results land in the VFS like any other file.
    let result = sandbox.exec("cat /app/stats.json | wc -c").await;
    print!("stats.json bytes: {}", result.stdout);

    // Peak wasm memory for the run is reported in the metrics.
    let result = sandbox.exec("js -e 'console.log(6 * 7)'").await;
    println!(
        "js -e output: {}peak wasm memory: {:?} bytes",
        result.stdout, result.metrics.peak_wasm_memory_bytes
    );

    // Runaway scripts hit the wall-clock deadline and exit 124, like GNU
    // timeout would report. A tighter sandbox makes the demo quick (the wasm
    // module itself is compiled once per process and cached, so this sandbox
    // doesn't pay that cost again).
    let impatient = Sandbox::builder()
        .limits(Limits {
            wall_time: Duration::from_secs(2),
            ..Limits::default()
        })
        .build();
    let result = impatient.exec("js -e 'while (true) {}'").await;
    println!("runaway script exit code: {}", result.exit_code);
    assert_eq!(result.exit_code, 124);
}
