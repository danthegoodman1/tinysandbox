//! Diagnostic harness (no CI timing thresholds):
//! `cargo run --release -p tinysandbox --no-default-features --example runtime_benchmark`
//!
//! Compares current memory dispatch with/without an unused local mount and
//! isolates old prefix-drain/rescan algorithms versus line-buffer cursors.
//! Buffer numbers describe the isolated algorithm, not command throughput.
//! Run the built binary under `/usr/bin/time -l` (macOS) or `/usr/bin/time -v`
//! (Linux) for process peak RSS; allocator/host noise makes timings diagnostic.

#[cfg(unix)]
fn main() {
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    use tinysandbox::sandbox::Sandbox;
    use tinysandbox::sandbox::fs::Fs;
    use tinysandbox::vfs::LocalVfs;

    fn median(mut samples: Vec<Duration>) -> Duration {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    async fn stats(fs: &Fs, repetitions: usize) -> Duration {
        let start = Instant::now();
        for _ in 0..repetitions {
            black_box(fs.stat("/workspace/file").await.unwrap());
        }
        start.elapsed()
    }

    let scratch = std::env::temp_dir().join(format!(
        "tinysandbox-runtime-benchmark-{}",
        std::process::id()
    ));
    std::fs::create_dir(&scratch).expect("create unique benchmark directory");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let memory = Sandbox::builder().build();
        let mixed = Sandbox::builder()
            .mount("unused", LocalVfs::new(&scratch).unwrap())
            .build();
        memory
            .fs()
            .write_file("file", b"content", false)
            .await
            .unwrap();
        mixed
            .fs()
            .write_file("file", b"content", false)
            .await
            .unwrap();
        let memory_fs = memory.fs();
        let mixed_fs = mixed.fs();
        stats(&memory_fs, 200).await;
        stats(&mixed_fs, 200).await;
        let mut pure_samples = Vec::new();
        let mut mixed_samples = Vec::new();
        for _ in 0..7 {
            pure_samples.push(stats(&memory_fs, 10_000).await);
            mixed_samples.push(stats(&mixed_fs, 10_000).await);
        }
        println!("current 10,000 memory Fs::stat calls, 7 samples after warmup:");
        println!("  memory-only median: {:?}", median(pure_samples));
        println!(
            "  with unused local mount median: {:?}",
            median(mixed_samples)
        );
    });
    std::fs::remove_dir_all(scratch).unwrap();

    let chunk = b"x\n".repeat(4096);
    let mut drain_samples = Vec::new();
    let mut cursor_samples = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let mut drained_bytes = 0;
        for _ in 0..1024 {
            let mut bytes = black_box(chunk.clone());
            while let Some(pos) = bytes.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = bytes.drain(..=pos).collect();
                drained_bytes += black_box(line).len();
            }
        }
        drain_samples.push(start.elapsed());
        let start = Instant::now();
        let mut cursor_bytes = 0;
        for _ in 0..1024 {
            let bytes = black_box(chunk.clone());
            let mut start = 0;
            while let Some(pos) = bytes[start..].iter().position(|byte| *byte == b'\n') {
                let end = start + pos + 1;
                let line = bytes[start..end].to_vec();
                cursor_bytes += black_box(line).len();
                start = end;
            }
        }
        cursor_samples.push(start.elapsed());
        assert_eq!(drained_bytes, 8 * 1024 * 1024);
        assert_eq!(drained_bytes, cursor_bytes);
    }
    println!("isolated 8MiB of 2-byte lines in 8KiB input chunks, 5 samples:");
    println!("  prefix-drain median: {:?}", median(drain_samples));
    println!("  cursor median: {:?}", median(cursor_samples));

    let mut rescan_samples = Vec::new();
    let mut incremental_samples = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..32 {
            let mut bytes = Vec::new();
            for _ in 0..128 {
                black_box(bytes.iter().position(|byte| *byte == b'\n'));
                bytes.extend_from_slice(black_box(&[b'a'; 8192]));
            }
        }
        rescan_samples.push(start.elapsed());
        let start = Instant::now();
        for _ in 0..32 {
            let mut bytes = Vec::new();
            let mut scanned = 0;
            for _ in 0..128 {
                black_box(bytes[scanned..].iter().position(|byte| *byte == b'\n'));
                scanned = bytes.len();
                bytes.extend_from_slice(black_box(&[b'a'; 8192]));
            }
        }
        incremental_samples.push(start.elapsed());
    }
    println!("isolated 32x1MiB lines in 8KiB input chunks, 5 samples:");
    println!("  full-prefix rescan median: {:?}", median(rescan_samples));
    println!(
        "  incremental scan median: {:?}",
        median(incremental_samples)
    );
}

#[cfg(not(unix))]
fn main() {
    eprintln!("the mixed-local-mount benchmark requires a Unix LocalVfs host");
}
