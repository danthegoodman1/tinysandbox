#!/usr/bin/env node

import { readFile } from "node:fs/promises";

const PAGE_BYTES = 64 * 1024;
const path = process.argv[2] ?? new URL("../assets/quickjs.wasm", import.meta.url);
const bytes = await readFile(path);
const module = await WebAssembly.compile(bytes);
const imports = WebAssembly.Module.imports(module);
const importObject = Object.create(null);
let memory;

for (const entry of imports) {
  importObject[entry.module] ??= Object.create(null);
  if (entry.kind === "function") {
    importObject[entry.module][entry.name] = () => 0;
  } else if (entry.kind !== "memory") {
    throw new Error(`cannot inspect imported ${entry.kind} ${entry.module}.${entry.name}`);
  }
}

for (const entry of imports) {
  if (entry.kind === "memory") {
    // JavaScript's reflection API does not expose an imported memory's minimum.
    // Find it without parsing the binary so this inspector remains dependency-free.
    for (let pages = 0; pages <= 256; pages++) {
      try {
        const candidate = new WebAssembly.Memory({ initial: pages });
        importObject[entry.module][entry.name] = candidate;
        await WebAssembly.instantiate(module, importObject);
        memory = candidate;
        break;
      } catch (error) {
        if (!(error instanceof WebAssembly.LinkError)) throw error;
      }
    }
    if (!memory) throw new Error(`could not determine minimum for ${entry.module}.${entry.name}`);
  }
}

const instance = await WebAssembly.instantiate(module, importObject);
const exportedMemory = instance.exports.memory;
if (!(exportedMemory instanceof WebAssembly.Memory) || exportedMemory !== memory) {
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
