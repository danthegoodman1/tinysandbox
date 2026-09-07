//! Diagnostic end-to-end jq latency. Run with --release, without timing gates.
use std::time::{Duration, Instant};
use tinysandbox::sandbox::Sandbox;

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let sandbox = Sandbox::builder().build();
        let data = format!(
            "[{}]",
            (0..10_000)
                .map(|i| format!("{{\"id\":{i},\"value\":\"example\"}}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        sandbox
            .fs()
            .write_file("data.json", data.as_bytes(), false)
            .await
            .unwrap();
        for (name, command) in [
            ("small object", "jq -nc '{answer: 42}'"),
            ("10k-row map", "jq -c 'map(.id + 1) | length' data.json"),
            ("10k-row stream", "jq -c '.[] | .id' data.json | wc -l"),
        ] {
            let start = Instant::now();
            let first = sandbox.exec(command).await;
            let first_elapsed = start.elapsed();
            assert_eq!(first.exit_code, 0, "{}", first.stderr);
            let mut samples = Vec::<Duration>::new();
            for _ in 0..21 {
                let start = Instant::now();
                let result = sandbox.exec(command).await;
                samples.push(start.elapsed());
                assert_eq!(result.exit_code, 0, "{}", result.stderr);
                assert_eq!(result.stdout, first.stdout);
            }
            samples.sort();
            println!(
                "{name}: first={first_elapsed:?}, median={:?}, peak guest={:?}",
                samples[samples.len() / 2],
                first.metrics.peak_wasm_memory_bytes
            );
        }
    });
}
