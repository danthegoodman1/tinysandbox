import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { createEngine, QUICKJS_INITIAL_MEMORY_BYTES } from "../dist/index.js";

const bytes = await readFile(new URL("../quickjs.wasm", import.meta.url));
const corpus = JSON.parse(await readFile(new URL("../../tests/fixtures/js_portable_corpus.json", import.meta.url), "utf8"));

function replaceSequence(input, before, after) {
  assert.equal(before.length, after.length);
  const output = Uint8Array.from(input);
  outer: for (let index = 0; index <= output.length - before.length; index++) {
    for (let offset = 0; offset < before.length; offset++) if (output[index + offset] !== before[offset]) continue outer;
    output.set(after, index);
    return output;
  }
  throw new Error("wasm patch sequence not found");
}

test("loads bytes and a precompiled module", async () => {
  const fromBytes = await createEngine(bytes);
  const fromModule = await createEngine(await WebAssembly.compile(bytes));
  assert.equal(fromBytes.runCode("console.log('bytes')").stdout, "bytes\n");
  assert.equal(fromModule.runCode("console.log('module')").stdout, "module\n");
});

test("rejects incompatible memory and interrupt ABI at engine creation", async () => {
  const memoryImport = [3, 101, 110, 118, 6, 109, 101, 109, 111, 114, 121, 2, 0, 19];
  const patchedMemory = replaceSequence(bytes, memoryImport, [...memoryImport.slice(0, -1), 20]);
  await assert.rejects(() => createEngine(patchedMemory), /memory contract/);

  const interruptName = [...new TextEncoder().encode("should_interrupt")];
  const patchedInterrupt = replaceSequence(bytes, interruptName, [...new TextEncoder().encode("absent_interrupt")]);
  await assert.rejects(() => createEngine(patchedInterrupt), /incompatible QuickJS wasm imports/);
});

test("creates fresh physical state for every run", async () => {
  const engine = await createEngine(bytes);
  assert.equal(engine.runCode("globalThis.leak = 42; console.log('set')").exitCode, 0);
  const isolated = engine.runCode("console.log(typeof leak)");
  assert.equal(isolated.stdout, "undefined\n");
  assert.equal(isolated.initialWasmMemoryBytes, QUICKJS_INITIAL_MEMORY_BYTES);
  assert.ok(isolated.peakWasmMemoryBytes >= QUICKJS_INITIAL_MEMORY_BYTES);
});

test("runs the shared Rust/V8 corpus", async () => {
  const engine = await createEngine(bytes);
  for (const item of corpus) {
    const result = engine.runCode(item.code, { argv: item.argv, env: item.env, cwd: item.cwd });
    assert.equal(result.exitCode, item.exitCode, item.name);
    assert.equal(result.stdout, item.stdout, item.name);
    assert.ok(result.stderr.startsWith(item.stderrPrefix), `${item.name}: ${result.stderr}`);
  }
});

test("supports synchronous dotted globals and JSON-safe values", async () => {
  const engine = await createEngine(bytes);
  const result = engine.runCode("console.log(JSON.stringify(tools.echo({ text: 'λ', list: [1, true, null] })))", {
    globals: { "tools.echo": (argument) => ({ argument, answer: 42 }) },
  });
  assert.equal(result.exitCode, 0);
  assert.deepEqual(JSON.parse(result.stdout), { argument: { text: "λ", list: [1, true, null] }, answer: 42 });

  const caught = engine.runCode("try { tools.fail(null) } catch (error) { console.log(error.message, error.code) }", {
    globals: { "tools.fail": () => Object.assign(Promise.resolve(), { code: "ASYNC" }) },
  });
  assert.match(caught.stdout, /host globals must be synchronous/);
  assert.throws(() => engine.runCode("", { globals: { "tools": () => null, "tools.search": () => null } }), /conflicts/);
  assert.throws(() => engine.runCode("", { globals: { "console.log": () => null } }), /reserved/);
  assert.throws(() => engine.runCode("", { globals: { "bad-name": () => null } }), /invalid name/);
});

test("rejects every non-JSON host-global shape deterministically", async () => {
  const engine = await createEngine(bytes);
  const cycle = {};
  cycle.self = cycle;
  const invalid = [
    undefined,
    () => null,
    Symbol("value"),
    Number.NaN,
    Number.POSITIVE_INFINITY,
    new Date(),
    { nested: { bad: undefined } },
    { big: 1n },
    cycle,
  ];
  for (const value of invalid) {
    const result = engine.runCode("try { invalid(null) } catch (error) { console.log(error.name, error.message) }", { globals: { invalid: () => value } });
    assert.equal(result.exitCode, 0);
    assert.match(result.stdout, /JSON|finite|plain objects|cycles/);
  }
});

test("enforces exact source and output byte boundaries before copying", async () => {
  const engine = await createEngine(bytes);
  assert.equal(engine.runCode("", { sourceBytes: 0 }).exitCode, 0);
  assert.equal(engine.runCode("λ", { sourceBytes: 2 }).exitCode, 1);
  assert.throws(() => engine.runCode("λ", { sourceBytes: 1 }), /source exceeded/);
  assert.equal(engine.runCode("console.log('abc')", { stdoutBytes: 4 }).stdout, "abc\n");
  const stdout = engine.runCode("console.log('abc')", { stdoutBytes: 3 });
  assert.equal(stdout.exitCode, 1);
  assert.equal(stdout.stderr, "js: stdout exceeded limit of 3 bytes\n");
  assert.equal(engine.runCode("console.error('abc')", { stderrBytes: 4 }).stderr, "abc\n");
  const stderr = engine.runCode("console.error('abc')", { stderrBytes: 3 });
  assert.equal(stderr.exitCode, 1);
  assert.equal(stderr.stderr, "js: stderr exceeded limit of 3 bytes\n");
});

test("bounds host responses before copying into wasm", async () => {
  const engine = await createEngine(bytes);
  const exact = engine.runCode("console.log(exact(null).length)", { globals: { exact: () => "x".repeat(100) }, hostResponseBytes: 112 });
  assert.equal(exact.stdout, "100\n");
  const oneOver = engine.runCode("try { exact(null) } catch (error) { console.log(error.code) }", { globals: { exact: () => "x".repeat(100) }, hostResponseBytes: 111 });
  assert.equal(oneOver.stdout, "E2BIG\n");
  const caught = engine.runCode("try { huge(null) } catch (error) { console.log(error.code, error.message) }", {
    globals: { huge: () => "x".repeat(100) },
    hostResponseBytes: 100,
  });
  assert.equal(caught.exitCode, 0);
  assert.equal(caught.stdout, "E2BIG host response exceeded limit of 100 bytes\n");
});

test("rejects a wasm cap below the artifact minimum before instantiation", async () => {
  const engine = await createEngine(bytes);
  assert.throws(() => engine.runCode("", { wasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES - 1 }), /must be at least/);
  assert.equal(engine.runCode("console.log('large')", { wasmMemoryBytes: Number.MAX_SAFE_INTEGER }).stdout, "large\n");
});

test("enforces wasm maximum, QuickJS heap, and monotonic deadline", async () => {
  const engine = await createEngine(bytes);
  const wasmOom = engine.runCode("const x=[]; while(true) x.push(new ArrayBuffer(256*1024))", {
    wasmMemoryBytes: 2 * 1024 * 1024,
    quickjsHeapBytes: 32 * 1024 * 1024,
    timeoutMs: 2_000,
  });
  assert.equal(wasmOom.exitCode, 1);
  assert.match(wasmOom.stderr, /memory limit exceeded|out of memory/i);
  assert.ok(wasmOom.peakWasmMemoryBytes <= 2 * 1024 * 1024);

  const heapOom = engine.runCode("const x=[]; while(true) x.push(new ArrayBuffer(64*1024))", {
    quickjsHeapBytes: 512 * 1024,
    timeoutMs: 2_000,
  });
  assert.equal(heapOom.exitCode, 1);
  assert.match(heapOom.stderr, /out of memory|failed to create context/i);

  const started = performance.now();
  const timeout = engine.runCode("while (true) {}", { timeoutMs: 20 });
  assert.equal(timeout.exitCode, 124);
  assert.equal(timeout.stdout, "");
  assert.equal(timeout.stderr, "js: command timed out\n");
  assert.ok(performance.now() - started < 1_000);
});
