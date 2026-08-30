# `@tinysandbox/js-runtime`

A small, stateless QuickJS runtime for standard WebAssembly hosts. It has no
runtime dependencies and uses the same checked-in QuickJS-ng artifact and guest
glue as tinysandbox's Rust/Wasmtime `js` command.

The package never reads a file or calls `fetch` itself. Supply the wasm bytes or
a compiled `WebAssembly.Module` explicitly, compile an engine once, and call
`runCode()` as needed. Every call creates a new bounded linear memory, wasm
instance, QuickJS runtime, and context; no guest state survives the call.

```js
import { readFile } from "node:fs/promises";
import { createEngine } from "@tinysandbox/js-runtime";

const bytes = await readFile(new URL(
  "./node_modules/@tinysandbox/js-runtime/quickjs.wasm",
  import.meta.url,
));
const engine = await createEngine(bytes);

const result = engine.runCode("console.log(tools.search({ query: 'hello' }))", {
  globals: {
    "tools.search": ({ query }) => ({ query, hits: 1 }),
  },
  timeoutMs: 1000,
  wasmMemoryBytes: 16 * 1024 * 1024,
  quickjsHeapBytes: 8 * 1024 * 1024,
});
```

Browser callers can fetch the exported `quickjs.wasm` asset themselves. Hosts
such as Convex that compile `.wasm` imports use the exported subpath directly:

```ts
import { createEngine } from "@tinysandbox/js-runtime";
import quickjsModule from "@tinysandbox/js-runtime/quickjs.wasm";

const value = await doConvexWork();
const engine = await createEngine(quickjsModule);
const result = engine.runCode("console.log(context.value(null))", {
  globals: { "context.value": () => value },
});
```

`runCode(code, options)` returns `exitCode`, UTF-8 `stdout` and `stderr`, and
initial/peak wasm memory bytes. Its defaults are a 64 MiB wasm maximum, 32 MiB
QuickJS heap, 30 second monotonic deadline, and 1 MiB each for source, serialized
host responses, stdout, and stderr. A `wasmMemoryBytes` value below the artifact
minimum of 1,245,184 bytes is rejected before instantiation. Non-page-aligned
values are rounded down for the actual WebAssembly maximum.

Global names use dot-separated JavaScript identifier segments. Each value must
be a synchronous one-argument function. Guest arguments cross the boundary
with JavaScript's normal `JSON.stringify` semantics (including its omission and
coercion rules); host return values must already be strict JSON values and are
validated without coercion. Promises, invalid returns, invalid names, namespace
conflicts, and runtime-global shadowing fail deterministically. Complete awaited
host work before calling `runCode()`; wasm execution is synchronous and blocks
the V8 event loop until it finishes.

This stateless package does not provide a VFS, file execution, filesystem or
network access, timers, guest ESM, TypeScript transpilation, Asyncify/JSPI, or
persistent isolates. The guest compatibility glue still defines `fetch` and
`require("fs")`, but without a host capability their operations fail with
`ENOSYS`; neither reaches ambient V8 `fetch` or filesystem APIs.
