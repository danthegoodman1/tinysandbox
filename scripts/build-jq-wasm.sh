#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JQ_BUILD_DIR="${TINYSANDBOX_JQ_BUILD_DIR:-${ROOT}/target/jq-guest}"
JQ_TOOLCHAIN="${TINYSANDBOX_JQ_RUST_TOOLCHAIN:-1.97.0}"
JQ_CARGO_ROOT="${CARGO_HOME:-${HOME}/.cargo}"
# Rust must have this target installed. No WASI SDK or platform compiler is used.
# Putting the 8 MiB stack first ensures stack exhaustion hits the inaccessible
# low address range instead of overwriting static data or the heap.
RUSTFLAGS="-C link-arg=-zstack-size=8388608 -C link-arg=--stack-first -C target-feature=-reference-types -C debuginfo=0 --remap-path-prefix=${ROOT}=/tinysandbox --remap-path-prefix=${JQ_CARGO_ROOT}=/cargo" \
    cargo "+${JQ_TOOLCHAIN}" build --manifest-path "${ROOT}/guests/jq/Cargo.toml" \
    --target wasm32-unknown-unknown --target-dir "${JQ_BUILD_DIR}" \
    --release --locked "$@"
cp "${JQ_BUILD_DIR}/wasm32-unknown-unknown/release/tinysandbox_jq_guest.wasm" "${ROOT}/assets/jq.wasm"
