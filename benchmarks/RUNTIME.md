# Runtime boundary measurements

Reproduce the diagnostic harness with:

```sh
cargo run --release -p tinysandbox --no-default-features --locked --example runtime_benchmark
```

Run the resulting `target/release/examples/runtime_benchmark` directly for
repeated measurements. `/usr/bin/time -l` on macOS or `/usr/bin/time -v` on Linux
can report process peak RSS where the host permits reading those statistics.
The harness has no timing thresholds and does not belong in normal CI gates.

Measured on 2026-09-04 on macOS arm64 with
`rustc 1.97.0 (2d8144b78 2026-07-07)`, Cargo's optimized `release` profile,
`--no-default-features`, and a Tokio current-thread runtime:

| Workload | Comparison | Median |
| --- | --- | --- |
| 10,000 current `Fs::stat` calls on a seven-byte in-memory file; 200 warmup calls; seven samples | Memory mount alone | 7.13 ms |
| Same current workload | Memory mount plus an unused local mount | 7.11 ms |
| Isolated 8 MiB of two-byte lines, read in 8 KiB chunks; five samples | Remove the vector prefix after each line | 278.7 ms |
| Same isolated workload, including the same per-line output allocation | Advance a cursor instead | 91.9 ms |
| Isolated 32 one-MiB lines, appended in 8 KiB chunks; five samples | Rescan the whole accumulated prefix after every read | 577.0 ms |
| Same isolated workload | Scan only the newly appended bytes | 9.60 ms |

The memory dispatch comparison exercises the **current implementation** in both
configurations. It shows no material extra dispatch cost from an unused local
mount in this run; it is not a before/after production speedup claim. Permanent
thread-ID assertions in `tests/execution_boundaries.rs` independently verify
that path and handle operations use the selected backend's scheduling class.

The buffer comparisons isolate the previous and replacement algorithms. They
do not measure complete command throughput, regex costs, filesystem I/O, or
pipeline backpressure. Per-line allocation is deliberately present on both
sides of the short-line comparison. Host load and allocator behavior affect
these numbers; there are no portable timing assertions.

The stat benchmark uses `Sandbox::fs()` with default host limits and one fixed
file; it has no command parsing, JS workers, native jq workers, network requests,
or S3 requests. The buffer workloads have the sizes listed above. This run did
not measure allocation counts. `/usr/bin/time -l` completed the benchmark but
could not read `sysctl kern.clockrate` in the execution sandbox, so no peak RSS
measurement is reported.

Native jq admission is separately capped at 16 blocking workers. A worker owns
its admission permit until it exits, including after an execution times out.
Each command's input buffering is bounded by `jq_input_bytes`; it precedes
worker admission so a queued downstream jq can drain its upstream pipe.
Serialized output is delivered in at most
64 KiB chunks through a bounded channel. These bounds do not impose a hard heap
limit or fully preemptive cancellation inside jaq's evaluator. Embedders needing
those guarantees can exclude native jq with `without_command("jq")`; changing
its interpreter or isolation boundary remains a deliberate policy decision.
