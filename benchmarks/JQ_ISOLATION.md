# jq isolation measurements

`examples/jq_benchmark.rs` measures complete shell executions, including filter
compilation, input parsing, evaluation, output transport, and teardown. It verifies
output equality between repeated runs and reports the median of 21 warm samples.
These are diagnostics, not CI thresholds or latency guarantees.

Run on macOS arm64 with Rust 1.97.0, release optimization, no optional features:

```sh
cargo run --release --no-default-features --locked --example jq_benchmark
```

The same harness was copied into an exported checkout of `222a7c0` to measure
the native evaluator before this change. The isolated column uses the canonical Linux-built guest
with capped memory, epoch interruption, and deadline-aware output backpressure.

| Workload | Native median | Isolated median | Change |
| --- | ---: | ---: | ---: |
| Small generated object | 0.354 ms | 0.444 ms | 1.25× |
| Map and count 10,000 JSON objects | 10.705 ms | 17.699 ms | 1.65× |
| Stream 10,000 IDs through `wc -l` | 15.448 ms | 22.807 ms | 1.48× |

The first small invocation took 1.358 ms natively and 8.762 ms with isolation,
including initialization of the shared Wasmtime engine and precompiled module.
The guest was precompiled by `build.rs`; these numbers do not include runtime
compilation fallback. Host load and scheduling affect the readings.

Peak guest linear memory was 9,043,968 bytes for the small object and 13,434,880
bytes for either 10,000-row case. This includes the guest's eight-MiB stack;
it is not process RSS. Each admitted worker also has a bounded native stack,
input buffers, and a four-message output channel with 64 KiB chunks. Wasmtime
code is shared across invocations. Guest state is discarded after every command.

The added cost buys a hard guest allocation boundary and interruption inside
computation that produces no output. `tests/jq_isolation.rs` verifies that a
billion-byte intermediate allocation fails within a 32 MiB cap and a subsequent
command succeeds. `jq_runtime` tests independently prove that cancellation exits
an entered evaluator worker, and a live but unpolled output receiver cannot keep
a worker past its deadline. Those deterministic assertions are the CI gates.
