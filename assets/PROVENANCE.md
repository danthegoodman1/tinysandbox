# Wasm Artifact Provenance

## jq

`assets/jq.wasm` is built from the standalone `guests/jq` crate using
`scripts/build-jq-wasm.sh`. Its pinned Rust toolchain, locked dependencies,
artifact hash, memory floor, exact imports/exports, and reproduction commands
are documented in [guests/jq/README.md](../guests/jq/README.md). The canonical
builder uses Rust host `x86_64-unknown-linux-gnu`; the pinned Linux/amd64 Docker
recipe avoids platform-dependent Cargo build-unit metadata. CI rebuilds it
on Linux x86_64 and requires byte-for-byte equality with the checked-in artifact,
then runs its ABI and compatibility tests. The guest has no ambient filesystem,
network, process, or environment capability; time comes from one explicit import.

## QuickJS

`assets/quickjs.wasm` is built from source by `scripts/build-quickjs-wasm.sh`.

- Source: quickjs-ng, https://github.com/quickjs-ng/quickjs.git
- Version: `v0.15.1`
- Commit: `fd0a0210b7be00957751871e7e01b8291268fc29`
- Toolchain: WASI SDK `27.0`, `wasm32-wasip1`, downloaded from https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-27
- Host used for the checked-in artifact: Linux x86_64, WASI SDK asset `wasi-sdk-27.0-x86_64-linux.tar.gz`. The same host rebuilt the previous (ABI 13, macOS arm64) artifact byte for byte.
- Build flags: `-Oz -DNDEBUG -D_GNU_SOURCE -DTINYSANDBOX_WASI_STACK_LIMIT -mexec-model=reactor`, linked with `--allow-undefined`, `--import-memory`, `-z stack-size=1048576`, explicit exports `tinysandbox_alloc`, `tinysandbox_free`, `tinysandbox_abi_version`, `tinysandbox_run`, `tinysandbox_resolve`, and the imported memory, then stripped with `llvm-strip`
- Source patch: the build script enables QuickJS's stack-limit branch under WASI so `JS_SetMaxStackSize` raises catchable `RangeError` exceptions before wasmtime stack traps.
- QuickJS stack limit: 786,432 bytes (`768 KiB`), leaving headroom inside the linked 1 MiB C stack
- QuickJS sources linked: `quickjs.c`, `dtoa.c`, `libregexp.c`, `libunicode.c`
- Tinysandbox shim: `src/js/quickjs_shim.c`
- Artifact: 631,445 bytes, SHA-256 `0f22c90b184359272a5022d28abadf0f52371e1a89129ac968cfe9beb6155e12`
- Initial linear memory: 19 WebAssembly pages, 1,245,184 bytes (1.1875 MiB)

Inspect the artifact without installing `wasm-tools` or another package:

```bash
node scripts/inspect-quickjs-wasm.mjs
```

Before the Phase 11 stack reduction, the checked-in artifact was 626,384 bytes
with 67 initial pages (4,390,912 bytes). Its linked C stack was 4 MiB and its
QuickJS stack limit was 3 MiB. Two independent final rebuilds on macOS arm64
produced the Phase 11 SHA-256
`784d9fe7db0db23e4250874920a5661916be3e0c7d007d22ea3ec86934a94e3c`
byte for byte; Phase 12's newer ABI artifact and hash are recorded separately
above and below.

The shim uses QuickJS core only, not `quickjs-libc.c` or the QuickJS `std`/`os`
modules. The guest has no WASI filesystem preopens and reaches the sandbox VFS
only through the `tinysandbox.host_call` import implemented by the Rust runtime.

Phase 12 changed the host ABI without changing the 19-page memory floor. The
artifact imports and re-exports `env.memory`, imports
`tinysandbox.should_interrupt`, and accepts a QuickJS heap limit as the third
argument to `tinysandbox_run`. Rust/Wasmtime and the portable V8 package create
a fresh bounded imported memory per run and consume these same bytes. The
interrupt import is a synchronous monotonic-deadline check in V8; Wasmtime also
retains epoch interruption as defense in depth.

The artifact exports `tinysandbox_abi_version()`, currently `14`. The portable
host checks that marker, the exact import/export contract and exported function
arities, then performs a fixed-minimum/full-wasm32-maximum probe instantiation
before accepting either bytes or a precompiled module. Two independent final
Phase 12 rebuilds produced SHA-256
`486eae877cf3274ce36e74ba30c5e9231d312d08572bed508a92872e57f9923b`
byte for byte.

Phase 13 keeps ABI version 12 and the exact import/export/function-arity and
19-page memory contracts. It adds only a backwards-compatible `vfs` boolean to
the host-supplied JSON run configuration so the shared guest glue can avoid
installing filesystem capabilities for portable runs without a VFS; Rust's
`/bin/js` always sends `vfs: true`. Two independent final Phase 13 rebuilds on
macOS arm64 reproduced that artifact byte for byte.

ABI 14 keeps the import/export/function-arity and 19-page memory contracts and
changes what the final `call_id` argument of `tinysandbox.host_call` means. A
guest call that awaits its answer passes a reserved slot, or `-1` when all 64
are taken; the synchronous `fs` API passes `-2` and must be answered inline.
The portable host can then defer `fs.promises` calls to an asynchronous VFS
without ever deferring an answer a synchronous caller needs. An ABI 13 guest
reserves slots for synchronous calls too, so the host rejects it. The same
artifact ends the guest as soon as it calls `process.exit()`, as Node does,
without draining queued jobs or waiting on outstanding host calls. Two rebuilds
on Linux x86_64 produced the current SHA-256 above byte for byte.
