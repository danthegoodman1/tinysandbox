# QuickJS Wasm Artifact Provenance

`assets/quickjs.wasm` is built from source by `scripts/build-quickjs-wasm.sh`.

- Source: quickjs-ng, https://github.com/quickjs-ng/quickjs.git
- Version: `v0.15.1`
- Commit: `fd0a0210b7be00957751871e7e01b8291268fc29`
- Toolchain: WASI SDK `27.0`, `wasm32-wasip1`, downloaded from https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-27
- Host used for the checked-in artifact: Linux x86_64, WASI SDK asset `wasi-sdk-27.0-x86_64-linux.tar.gz`. The build reproduces across hosts: rebuilding the previous artifact on Linux x86_64 matched the macOS arm64 build byte for byte.
- Build flags: `-Oz -DNDEBUG -D_GNU_SOURCE -DTINYSANDBOX_WASI_STACK_LIMIT -mexec-model=reactor`, linked with `--allow-undefined`, `-z stack-size=4194304`, explicit exports `tinysandbox_alloc`, `tinysandbox_free`, `tinysandbox_run`, and `memory`, then stripped with `llvm-strip`
- Source patch: the build script enables QuickJS's stack-limit branch under WASI so `JS_SetMaxStackSize` raises catchable `RangeError` exceptions before wasmtime stack traps.
- QuickJS sources linked: `quickjs.c`, `dtoa.c`, `libregexp.c`, `libunicode.c`
- Tinysandbox shim: `src/js/quickjs_shim.c`
- Artifact: 626,384 bytes, SHA-256 `5815948e20420d7e7c6d82da9ad4130f82bb2f56f185c62789bd2065d6cced5d`

The shim uses QuickJS core only, not `quickjs-libc.c` or the QuickJS `std`/`os`
modules. The guest has no WASI filesystem preopens and reaches the sandbox VFS
only through the `tinysandbox.host_call` import implemented by the Rust runtime.
