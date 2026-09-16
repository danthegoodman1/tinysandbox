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
  assert.equal((await fromBytes.runCode("console.log('bytes')")).stdout, "bytes\n");
  assert.equal((await fromModule.runCode("console.log('module')")).stdout, "module\n");
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
  assert.equal((await engine.runCode("globalThis.leak = 42; console.log('set')")).exitCode, 0);
  const isolated = await engine.runCode("console.log(typeof leak)");
  assert.equal(isolated.stdout, "undefined\n");
  assert.equal(isolated.initialWasmMemoryBytes, QUICKJS_INITIAL_MEMORY_BYTES);
  assert.ok(isolated.peakWasmMemoryBytes >= QUICKJS_INITIAL_MEMORY_BYTES);
});

test("runs the shared Rust/V8 corpus", async () => {
  const engine = await createEngine(bytes);
  for (const item of corpus) {
    const result = await engine.runCode(item.code, { argv: item.argv, env: item.env, cwd: item.cwd });
    assert.equal(result.exitCode, item.exitCode, item.name);
    assert.equal(result.stdout, item.stdout, item.name);
    assert.ok(result.stderr.startsWith(item.stderrPrefix), `${item.name}: ${result.stderr}`);
  }
});

test("supports dotted globals, JSON-safe values, and awaited async globals", async () => {
  const engine = await createEngine(bytes);
  const result = await engine.runCode("(async () => console.log(JSON.stringify(await tools.echo({ text: 'λ', list: [1, true, null] }))))()", {
    globals: { "tools.echo": (argument) => ({ argument, answer: 42 }) },
  });
  assert.equal(result.exitCode, 0);
  assert.deepEqual(JSON.parse(result.stdout), { argument: { text: "λ", list: [1, true, null] }, answer: 42 });

  // A global that returns a promise suspends the guest and resumes it with the
  // settled value; a synchronous one still returns without suspending.
  const awaited = await engine.runCode(
    "(async () => { console.log(JSON.stringify(await tools.slow({ n: 2 }))) })()",
    { globals: { "tools.slow": async ({ n }) => { await new Promise((r) => setTimeout(r, 10)); return n * 21; } } },
  );
  assert.equal(awaited.exitCode, 0, awaited.stderr);
  assert.equal(awaited.stdout, "42\n");
  await assert.rejects(() => engine.runCode("", { globals: { "tools": () => null, "tools.search": () => null } }), /conflicts/);
  await assert.rejects(() => engine.runCode("", { globals: { "console.log": () => null } }), /reserved/);
  await assert.rejects(() => engine.runCode("", { globals: { "bad-name": () => null } }), /invalid name/);
});

test("host globals can poll the shared monotonic deadline and abort signal", async () => {
  const engine = await createEngine(bytes);
  let observed = false;
  const result = await engine.runCode("(async () => await work(null))()", {
    timeoutMs: 50,
    globals: { work: (argument, context) => {
      assert.equal(argument, null);
      assert.ok(context.signal instanceof AbortSignal);
      assert.ok(context.remainingTimeMs() > 0 && context.remainingTimeMs() <= 50);
      assert.ok(Math.abs(context.deadlineMs - Date.now() - context.remainingTimeMs()) < 100);
      context.signal.addEventListener("abort", () => { observed = true; }, { once: true });
      // A second clock independently bounds this regression if context polling breaks.
      const watchdog = performance.now() + 500;
      while (!context.isCancelled() && performance.now() < watchdog) {}
      assert.equal(context.isCancelled(), true);
      assert.equal(context.signal.aborted, true);
      return null;
    } },
  });
  assert.equal(result.exitCode, 124);
  assert.equal(observed, true);
  assert.equal((await engine.runCode("(async () => console.log(await legacy(7)))()", { globals: { legacy: (value) => value } })).stdout, "7\n");
});

test("external aborts propagate synchronously to host context and stop later operations", async () => {
  const engine = await createEngine(bytes);
  const controller = new AbortController();
  let laterCalls = 0;
  const options = {
    signal: controller.signal,
    globals: {
      stop: (_argument, context) => {
        controller.abort(new Error("stop this run"));
        assert.equal(context.signal.aborted, true);
        assert.equal(context.isCancelled(), true);
        return null;
      },
      later: () => { laterCalls += 1; return null; },
    },
  };
  assert.equal((await engine.runCode("stop(null); later(null)", options)).exitCode, 124);
  assert.equal(laterCalls, 0);
  assert.equal((await engine.runCode("later(null)", options)).exitCode, 124, "already-aborted signal rejects entry");
  assert.equal(laterCalls, 0);
  assert.equal((await engine.runCode("console.log('healthy')")).stdout, "healthy\n");
});

test("successful and failed callbacks release their signals before the next invocation", async () => {
  const engine = await createEngine(bytes);
  let previous;
  let aborts = 0;
  const result = await engine.runCode("(async () => { await inspect(false); try { await inspect(true) } catch {} await inspect(false) })()", {
    globals: { inspect: (fail, context) => {
      if (previous) assert.equal(previous.signal.aborted, true);
      assert.equal(context.signal.aborted, false);
      context.signal.addEventListener("abort", () => { aborts += 1; }, { once: true });
      previous = context;
      if (fail) throw new Error("expected");
      return null;
    } },
  });
  assert.equal(result.exitCode, 0, result.stderr);
  assert.equal(previous.signal.aborted, true, "retained signal aborts without polling");
  assert.equal(aborts, 3);
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
    const result = await engine.runCode("(async () => { try { await invalid(null) } catch (error) { console.log(error.name, error.message) } })()", { globals: { invalid: () => value } });
    assert.equal(result.exitCode, 0);
    assert.match(result.stdout, /JSON|finite|plain objects|cycles/);
  }
});

test("enforces exact source and output byte boundaries before copying", async () => {
  const engine = await createEngine(bytes);
  assert.equal((await engine.runCode("", { sourceBytes: 0 })).exitCode, 0);
  assert.equal((await engine.runCode("λ", { sourceBytes: 2 })).exitCode, 1);
  await assert.rejects(() => engine.runCode("λ", { sourceBytes: 1 }), /source exceeded/);
  assert.equal((await engine.runCode("console.log('abc')", { stdoutBytes: 4 })).stdout, "abc\n");
  const stdout = await engine.runCode("console.log('abc')", { stdoutBytes: 3 });
  assert.equal(stdout.exitCode, 1);
  assert.equal(stdout.stderr, "js: stdout exceeded limit of 3 bytes\n");
  assert.equal((await engine.runCode("console.error('abc')", { stderrBytes: 4 })).stderr, "abc\n");
  const stderr = await engine.runCode("console.error('abc')", { stderrBytes: 3 });
  assert.equal(stderr.exitCode, 1);
  assert.equal(stderr.stderr, "js: stderr exceeded limit of 3 bytes\n");
});

test("bounds host responses before copying into wasm", async () => {
  const engine = await createEngine(bytes);
  const exact = await engine.runCode("(async () => console.log((await exact(null)).length))()", { globals: { exact: () => "x".repeat(100) }, hostResponseBytes: 112 });
  assert.equal(exact.stdout, "100\n");
  const oneOver = await engine.runCode("(async () => { try { await exact(null) } catch (error) { console.log(error.code) } })()", { globals: { exact: () => "x".repeat(100) }, hostResponseBytes: 111 });
  assert.equal(oneOver.stdout, "E2BIG\n");
  const caught = await engine.runCode("(async () => { try { await huge(null) } catch (error) { console.log(error.code, error.message) } })()", {
    globals: { huge: () => "x".repeat(100) },
    hostResponseBytes: 100,
  });
  assert.equal(caught.exitCode, 0);
  assert.equal(caught.stdout, "E2BIG host response exceeded limit of 100 bytes\n");
});

test("rejects a wasm cap below the artifact minimum before instantiation", async () => {
  const engine = await createEngine(bytes);
  await assert.rejects(() => engine.runCode("", { wasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES - 1 }), /must be at least/);
  assert.equal((await engine.runCode("console.log('large')", { wasmMemoryBytes: Number.MAX_SAFE_INTEGER })).stdout, "large\n");
});

test("enforces wasm maximum, QuickJS heap, and monotonic deadline", async () => {
  const engine = await createEngine(bytes);
  const wasmOom = await engine.runCode("const x=[]; while(true) x.push(new ArrayBuffer(256*1024))", {
    wasmMemoryBytes: 2 * 1024 * 1024,
    quickjsHeapBytes: 32 * 1024 * 1024,
    timeoutMs: 2_000,
  });
  assert.equal(wasmOom.exitCode, 1);
  assert.match(wasmOom.stderr, /memory limit exceeded|out of memory/i);
  assert.ok(wasmOom.peakWasmMemoryBytes <= 2 * 1024 * 1024);

  const heapOom = await engine.runCode("const x=[]; while(true) x.push(new ArrayBuffer(64*1024))", {
    quickjsHeapBytes: 512 * 1024,
    timeoutMs: 2_000,
  });
  assert.equal(heapOom.exitCode, 1);
  assert.match(heapOom.stderr, /out of memory|failed to create context/i);

  const started = performance.now();
  const timeout = await engine.runCode("while (true) {}", { timeoutMs: 20 });
  assert.equal(timeout.exitCode, 124);
  assert.equal(timeout.stdout, "");
  assert.equal(timeout.stderr, "js: command timed out\n");
  assert.ok(performance.now() - started < 1_000);
});


test("bounded JSON retains exact UTF-8 and escape semantics", async () => {
  const engine = await createEngine(bytes);
  for (const value of ["a".repeat(100), "λ🙂".repeat(30), "\u0000\b\t\n\f\r\\\"".repeat(20), "\ud800".repeat(20), { "λ🙂": [false, 1.5, null, "hello"] }]) {
    const cap = new TextEncoder().encode(JSON.stringify({ value })).byteLength;
    const exact = await engine.runCode("(async () => console.log(JSON.stringify(await value())))()", { globals: { value: () => value }, hostResponseBytes: cap });
    assert.equal(exact.exitCode, 0, exact.stderr);
    assert.equal(exact.stdout, `${JSON.stringify(value)}\n`);
    const over = await engine.runCode("(async () => { try { await value() } catch (e) { console.log(e.code) } })()", { globals: { value: () => value }, hostResponseBytes: cap - 1 });
    if (cap >= 100) assert.equal(over.stdout, "E2BIG\n");
    else assert.equal(over.exitCode, 1);
  }
});

test("a host global that never settles cannot outlive the deadline", async () => {
  const engine = await createEngine(bytes);
  const started = Date.now();
  // Suspension is the one point with no guest checkpoint to observe the clock,
  // so the wait itself has to be bounded.
  const result = await engine.runCode("(async () => { await hang({}); console.log('never') })()", {
    globals: { hang: () => new Promise(() => {}) },
    timeoutMs: 200,
  });
  assert.equal(result.exitCode, 124);
  assert.match(result.stderr, /command timed out/);
  assert.equal(result.stdout, "");
  assert.ok(Date.now() - started < 3000, "must not wait past the deadline");
});

test("concurrent awaited globals settle in completion order", async () => {
  const engine = await createEngine(bytes);
  const result = await engine.runCode(
    "(async () => { console.log((await Promise.all([slow({}), fast({})])).join(',')) })()",
    {
      globals: {
        slow: async () => { await new Promise((r) => setTimeout(r, 60)); return "slow"; },
        fast: async () => { await new Promise((r) => setTimeout(r, 5)); return "fast"; },
      },
    },
  );
  assert.equal(result.exitCode, 0, result.stderr);
  assert.equal(result.stdout, "slow,fast\n");
});

test("exceeding the concurrent host call limit fails the call, not the run", async () => {
  const engine = await createEngine(bytes);
  // 64 slots: 65 simultaneous awaits must report a bounded failure rather than
  // corrupting the registry or hanging.
  const result = await engine.runCode(
    `(async () => {
       const calls = Array.from({ length: 65 }, (_, i) => hold({ i }))
       const settled = await Promise.allSettled(calls)
       console.log(settled.filter(s => s.status === 'rejected').length > 0)
     })()`,
    {
      globals: { hold: async ({ i }) => { await new Promise((r) => setTimeout(r, 5)); return i; } },
      timeoutMs: 5000,
    },
  );
  assert.equal(result.exitCode, 0, result.stderr);
  assert.equal(result.stdout, "true\n");
});

test("sparse arrays serialize the way JSON.stringify does", async () => {
  // The guest config and VFS directory listings reach the response encoder
  // without passing assertJsonValue, so holes arrive here rather than being
  // rejected earlier. Skipping them shifted every later entry, and a leading
  // hole produced JSON the guest could not parse at all.
  const engine = await createEngine(bytes);
  const argv = ["js", "-e", "third"];
  delete argv[0];
  delete argv[1];

  const result = await engine.runCode("console.log(JSON.stringify(process.argv))", { argv });
  assert.equal(result.exitCode, 0, result.stderr);
  assert.equal(result.stdout, `${JSON.stringify(argv)}\n`);
  assert.equal(result.stdout, '[null,null,"third"]\n');
});

test("host-call admission precedes callback side effects and includes synchronous globals", async () => {
  const engine = await createEngine(bytes);
  for (const count of [64, 65, 80]) {
    for (const rejects of [false, true]) {
      let calls = 0, synchronous = 0;
      const contexts = [];
      const result = await engine.runCode(`(async () => {
        const calls = Array.from({length:${count}}, () => work(null));
        calls.push(sync(null));
        const results = await Promise.allSettled(calls);
        console.log(results.filter(r => r.status === 'rejected').length);
        console.log(await sync(null));
      })()`, { globals: {
        work: async (_value, context) => {
          calls++;
          contexts.push(context);
          if (rejects) throw new Error('expected');
          return null;
        },
        sync: () => { synchronous++; return 'recovered'; },
      }});
      assert.equal(result.exitCode, 0, result.stderr);
      assert.equal(calls, 64, "overflow never invokes the host callback");
      assert.equal(synchronous, 1, "sync calls are admitted again after pending work settles");
      assert.equal(result.stdout, `${rejects ? count + 1 : count - 64 + 1}\nrecovered\n`);
      assert.ok(contexts.every(context => context.signal.aborted));
    }
  }
});

test("callback promises remain observed when cancellation happens during invocation", async () => {
  const { execFile } = await import('node:child_process');
  const { promisify } = await import('node:util');
  const script = `
    import assert from 'node:assert/strict';
    import {readFile} from 'node:fs/promises';
    import {createEngine} from ${JSON.stringify(new URL('../dist/index.js', import.meta.url).href)};
    const engine = await createEngine(await readFile(new URL(${JSON.stringify(new URL('../quickjs.wasm', import.meta.url).href)})));
    for (const abort of [false, true]) {
      const controller = new AbortController();
      let calls = 0;
      const result = await engine.runCode('(async()=>{await Promise.allSettled(Array.from({length:80},()=>work(null)))})()', {
        signal: controller.signal,
        globals: {work: () => {
          calls++;
          if (abort) controller.abort();
          return Promise.reject(new Error('expected host rejection'));
        }},
      });
      assert.equal(result.exitCode, abort ? 124 : 0, result.stderr);
      assert.equal(calls, abort ? 1 : 64);
      await new Promise(resolve => setTimeout(resolve, 10));
    }
    console.log('contained');
  `;
  const child = await promisify(execFile)(process.execPath,
    ['--unhandled-rejections=strict', '--input-type=module', '-e', script], { timeout: 10000 });
  assert.equal(child.stdout, 'contained\n');
});
