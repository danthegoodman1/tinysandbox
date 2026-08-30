#!/usr/bin/env node

import { readFile } from "node:fs/promises";

const PAGE_BYTES = 64 * 1024;
const path = process.argv[2] ?? new URL("../assets/quickjs.wasm", import.meta.url);
const bytes = await readFile(path);
const module = await WebAssembly.compile(bytes);
const imports = WebAssembly.Module.imports(module);
const importObject = Object.create(null);

for (const entry of imports) {
  if (entry.kind !== "function") {
    throw new Error(`cannot inspect imported ${entry.kind} ${entry.module}.${entry.name}`);
  }
  importObject[entry.module] ??= Object.create(null);
  importObject[entry.module][entry.name] = () => 0;
}

const instance = await WebAssembly.instantiate(module, importObject);
const memory = instance.exports.memory;
if (!(memory instanceof WebAssembly.Memory)) {
  throw new Error("quickjs wasm does not export memory");
}

console.log(`artifact_bytes=${bytes.byteLength}`);
console.log(`initial_memory_pages=${memory.buffer.byteLength / PAGE_BYTES}`);
console.log(`initial_memory_bytes=${memory.buffer.byteLength}`);
for (const entry of imports) {
  console.log(`import=${entry.module}.${entry.name}:${entry.kind}`);
}
for (const entry of WebAssembly.Module.exports(module)) {
  console.log(`export=${entry.name}:${entry.kind}`);
}
