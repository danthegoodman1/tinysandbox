# Isolated jq guest

This standalone crate compiles the jq filter engine into `assets/jq.wasm`.
Tinysandbox's Rust host creates a fresh Wasmtime instance for each command,
limits its linear memory, and interrupts its instructions using the execution's
absolute deadline and cancellation state. Filter compilation, JSON parsing,
variable decoding, evaluation, and serialization all execute inside that
instance. The guest links no WASI filesystem, process, environment, or network
capabilities.

The artifact uses jaq-core 3.1.0, jaq-json 2.0.1, and jaq-std 3.0.1. Exact
transitive versions are recorded in this crate's `Cargo.lock`, independently
of the host workspace. The evaluator and its existing tests moved from
`src/sandbox/jq.rs`; supported filter syntax and CLI status rules are retained.

## Build and verify

```sh
rustup toolchain install 1.97.0 --profile minimal --target wasm32-unknown-unknown
scripts/build-jq-wasm.sh
cargo +1.97.0 test --manifest-path guests/jq/Cargo.toml --locked
node --test guests/jq/abi.test.mjs
```

The build script pins Rust 1.97.0, uses `--locked`, enables release optimization,
LTO and one codegen unit, aborts on guest panic, strips symbols, and remaps
checkout/Cargo source paths. It reserves an eight-MiB stack with `--stack-first`
so exhausting the linear-memory stack traps before overwriting the heap or
static data. `TINYSANDBOX_JQ_BUILD_DIR` selects a separate output directory for
an independent rebuild. `TINYSANDBOX_JQ_RUST_TOOLCHAIN` is an explicit toolchain
override for development; changing it may change the artifact.

The checked-in artifact was built on macOS arm64 with
`rustc 1.97.0 (2d8144b78 2026-07-07)`:

- Size: 1,856,818 bytes.
- SHA-256: `a08cbfca784d3ea7e69c2f369485e934706302f160cb72bd5f9284a3174f2720`.
- Initial memory: 135 Wasm pages, or 8,847,360 bytes, including the eight-MiB stack.
- Separate clean build directories produced identical bytes on this host.

The host's configured memory limit includes that memory floor, the guest heap,
parser/compiler state, and guest input/output buffers. Wasmtime compiled code,
host input buffers, bounded bridge messages, and native worker stacks are
separate host resources. Instantiating this artifact directly in another engine
does not itself provide a memory ceiling or deadline; the embedding host must
enforce those limits.

## Byte ABI

The guest exports only `memory` and `run() -> i32`. `run` returns the jq CLI exit
status. Its imports belong to module `tinysandbox_jq`:

| Import | Contract |
| --- | --- |
| `input_len(index: i32) -> i32` | Length of one host input; negative indicates failure. |
| `read_input(index: i32, offset: i32, ptr: i32, len: i32) -> i32` | Copies up to `len` bytes into guest memory; returns the byte count or a negative failure. Each request is at most 64 KiB. |
| `write_output(kind: i32, ptr: i32, len: i32) -> i32` | Copies stdout (`kind=1`) or stderr (`kind=2`) from guest memory; returns the accepted byte count or a negative failure. Each request is at most 64 KiB. |
| `now() -> f64` | Explicit host clock capability returning UNIX seconds. |

Input zero is UTF-8 JSON for the shared `JqRequest` in
`src/sandbox/jq_protocol.rs`. Inputs one onward are raw source bytes, ordered by
`request.paths`. `--argjson` values cross the boundary as strings and are parsed
in the guest; malformed values retain exit code 2. Output uses bounded chunks
and flushes after each completed value, preserving downstream early exit.

## Environment and time

`env` returns an empty object. Host environment variables never cross this
capability boundary, and sandbox environment forwarding is not currently part
of the jq protocol. `now` uses the explicit clock import. `localtime` and
`strflocaltime` use UTC, as do `gmtime` and `strftime`, so formatting does not
depend on an ambient machine timezone. Other date conversion functions retain
their existing jaq behavior. The real-Wasm Node tests verify these choices as
well as imports/exports, CLI status, multiple inputs, and chunk sizes.
