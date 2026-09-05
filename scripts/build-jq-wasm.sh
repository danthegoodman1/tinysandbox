#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JQ_BUILD_DIR="${TINYSANDBOX_JQ_BUILD_DIR:-${ROOT}/target/jq-guest}"
JQ_TOOLCHAIN="${TINYSANDBOX_JQ_RUST_TOOLCHAIN:-1.97.0}"
JQ_CARGO_ROOT="${CARGO_HOME:-${HOME}/.cargo}"
JQ_RUST_HOST="$(rustc "+${JQ_TOOLCHAIN}" -vV | sed -n 's/^host: //p')"
# Cargo includes native build-script/proc-macro identities in target crate
# metadata. Remapping source paths alone cannot make different Rust hosts
# generate byte-identical Wasm. Keep the artifact's build host explicit.
if [[ "${JQ_RUST_HOST}" != "x86_64-unknown-linux-gnu" ]]; then
    if [[ "${TINYSANDBOX_JQ_ALLOW_NONCANONICAL:-0}" != "1" ]]; then
        printf '%s\n' \
            "jq.wasm requires the canonical Rust host x86_64-unknown-linux-gnu; found ${JQ_RUST_HOST}." \
            "Use the Linux/amd64 Docker command in guests/jq/README.md to reproduce the checked-in artifact." \
            "For a development-only build, set TINYSANDBOX_JQ_ALLOW_NONCANONICAL=1; its bytes will differ from CI." >&2
        exit 1
    fi
    printf '%s\n' "Building development-only jq.wasm for Rust host ${JQ_RUST_HOST}; its bytes will differ from CI." >&2
fi
# Rust must have this target installed. No WASI SDK or platform compiler is used.
# Putting the 8 MiB stack first ensures stack exhaustion hits the inaccessible
# low address range instead of overwriting static data or the heap.
RUSTFLAGS="-C link-arg=-zstack-size=8388608 -C link-arg=--stack-first -C target-feature=-reference-types -C debuginfo=0 --remap-path-prefix=${ROOT}=/tinysandbox --remap-path-prefix=${JQ_CARGO_ROOT}=/cargo" \
    cargo "+${JQ_TOOLCHAIN}" build --manifest-path "${ROOT}/guests/jq/Cargo.toml" \
    --target wasm32-unknown-unknown --target-dir "${JQ_BUILD_DIR}" \
    --release --locked "$@"
cp "${JQ_BUILD_DIR}/wasm32-unknown-unknown/release/tinysandbox_jq_guest.wasm" "${ROOT}/assets/jq.wasm"
