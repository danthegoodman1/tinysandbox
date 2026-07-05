# QuickJS Wasm Artifact Provenance

`assets/quickjs.wasm` is built from source by `scripts/build-quickjs-wasm.sh`.

- Source: quickjs-ng, https://github.com/quickjs-ng/quickjs.git
- Version: `v0.15.1`
- Commit: `fd0a0210b7be00957751871e7e01b8291268fc29`
- Toolchain: WASI SDK `27.0`, `wasm32-wasip1`, downloaded from https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-27
- Host used for the checked-in artifact: macOS arm64, WASI SDK asset `wasi-sdk-27.0-arm64-macos.tar.gz`
- Build flags: `-Oz -DNDEBUG -D_GNU_SOURCE -DTHINBOX_WASI_STACK_LIMIT -mexec-model=reactor`, linked with `--allow-undefined`, `-z stack-size=4194304`, explicit exports `thinbox_alloc`, `thinbox_free`, `thinbox_run`, and `memory`, then stripped with `llvm-strip`
- Source patch: the build script enables QuickJS's stack-limit branch under WASI so `JS_SetMaxStackSize` raises catchable `RangeError` exceptions before wasmtime stack traps.
- QuickJS sources linked: `quickjs.c`, `dtoa.c`, `libregexp.c`, `libunicode.c`
- Thinbox shim: `src/js/quickjs_shim.c`
- Artifact: 609,334 bytes, SHA-256 `9c7e820fdf34078bd13d4eaebf65c54e257ab40c7f5edc15e9f6f7f698ef9ff0`

The shim uses QuickJS core only, not `quickjs-libc.c` or the QuickJS `std`/`os`
modules. The guest has no WASI filesystem preopens and reaches the sandbox VFS
only through the `thinbox.host_call` import implemented by the Rust runtime.
