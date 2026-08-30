// @ts-nocheck -- this source intentionally uses only JavaScript syntax so the
// zero-dependency build can copy it verbatim to the ESM distribution.
const PAGE_BYTES = 64 * 1024;
const INITIAL_PAGES = 19;
const DEFAULT_WASM_MEMORY_BYTES = 64 * 1024 * 1024;
const DEFAULT_QUICKJS_HEAP_BYTES = 32 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS = 30_000;
const DEFAULT_SOURCE_BYTES = 1024 * 1024;
const DEFAULT_HOST_RESPONSE_BYTES = 1024 * 1024;
const DEFAULT_OUTPUT_BYTES = 1024 * 1024;
const RESERVED_GLOBALS = new Set([
  "Buffer", "Headers", "Response", "__dirname", "__filename", "console",
  "exports", "fetch", "globalThis", "module", "process", "require",
]);
const ABI_VERSION = 12;
const REQUIRED_IMPORTS = [
  "env.memory:memory",
  "tinysandbox.host_call:function",
  "tinysandbox.host_response_len:function",
  "tinysandbox.host_response_read:function",
  "tinysandbox.should_interrupt:function",
  "tinysandbox.write_stderr:function",
  "tinysandbox.write_stdout:function",
  "wasi_snapshot_preview1.clock_time_get:function",
  "wasi_snapshot_preview1.fd_close:function",
  "wasi_snapshot_preview1.fd_fdstat_get:function",
  "wasi_snapshot_preview1.fd_seek:function",
  "wasi_snapshot_preview1.fd_write:function",
].sort();
const REQUIRED_EXPORTS = [
  "_initialize:function",
  "memory:memory",
  "tinysandbox_abi_version:function",
  "tinysandbox_alloc:function",
  "tinysandbox_free:function",
  "tinysandbox_run:function",
].sort();
const encoder = new TextEncoder();
const decoder = new TextDecoder();

export const QUICKJS_INITIAL_MEMORY_BYTES = INITIAL_PAGES * PAGE_BYTES;

class RunLimitError extends Error {
  constructor(kind, message) {
    super(message);
    this.kind = kind;
  }
}

function integerOption(name, value, fallback, minimum = 0) {
  const result = value === undefined ? fallback : value;
  if (!Number.isSafeInteger(result) || result < minimum) {
    throw new TypeError(`${name} must be a safe integer of at least ${minimum}`);
  }
  return result;
}

function validateGlobals(globals) {
  if (globals === undefined) return [];
  if (globals === null || typeof globals !== "object" || Array.isArray(globals)) {
    throw new TypeError("globals must be an object of synchronous functions");
  }
  const names = Object.keys(globals).sort();
  for (const name of names) {
    if (typeof globals[name] !== "function") {
      throw new TypeError(`global '${name}' must be a function`);
    }
    const parts = name.split(".");
    if (parts.some((part) => !/^[A-Za-z_][A-Za-z0-9_]*$/.test(part))) {
      throw new TypeError(`cannot register invalid name '${name}'; names are dot-separated paths of [A-Za-z_][A-Za-z0-9_]* segments`);
    }
    if (RESERVED_GLOBALS.has(parts[0])) {
      throw new TypeError(`cannot register reserved name '${name}'; '${parts[0]}' is provided by the JavaScript runtime`);
    }
  }
  for (let index = 1; index < names.length; index++) {
    const previous = names[index - 1];
    const current = names[index];
    if (current.startsWith(`${previous}.`)) {
      throw new TypeError(`cannot register '${current}'; it conflicts with '${previous}'`);
    }
  }
  return names;
}

function requireArtifactAbi(module) {
  const imports = WebAssembly.Module.imports(module);
  const actualImports = imports.map((entry) => `${entry.module}.${entry.name}:${entry.kind}`).sort();
  const actualExports = WebAssembly.Module.exports(module).map((entry) => `${entry.name}:${entry.kind}`).sort();
  if (JSON.stringify(actualImports) !== JSON.stringify(REQUIRED_IMPORTS)) throw new TypeError(`incompatible QuickJS wasm imports: ${actualImports.join(", ")}`);
  if (JSON.stringify(actualExports) !== JSON.stringify(REQUIRED_EXPORTS)) throw new TypeError(`incompatible QuickJS wasm exports: ${actualExports.join(", ")}`);
}

function probeArtifactAbi(module) {
  const memory = new WebAssembly.Memory({ initial: INITIAL_PAGES, maximum: 65_536 });
  const zero = () => 0;
  let instance;
  try {
    instance = new WebAssembly.Instance(module, {
      env: { memory },
      tinysandbox: { host_call: zero, host_response_len: zero, host_response_read: zero, should_interrupt: zero, write_stderr: zero, write_stdout: zero },
      wasi_snapshot_preview1: { clock_time_get: zero, fd_close: zero, fd_fdstat_get: zero, fd_seek: zero, fd_write: zero },
    });
  } catch (error) {
    throw new TypeError(`incompatible QuickJS wasm memory contract: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (instance.exports.memory !== memory) throw new TypeError("incompatible QuickJS wasm: env.memory is not re-exported");
  for (const [name, arity] of [["tinysandbox_abi_version", 0], ["tinysandbox_alloc", 1], ["tinysandbox_free", 1], ["tinysandbox_run", 3]]) {
    if (instance.exports[name].length !== arity) throw new TypeError(`incompatible QuickJS wasm export signature '${name}'`);
  }
  if (instance.exports.tinysandbox_abi_version() !== ABI_VERSION) throw new TypeError(`incompatible QuickJS wasm ABI version; expected ${ABI_VERSION}`);
}

function writeU32(memory, pointer, value) {
  new DataView(memory.buffer).setUint32(pointer, value >>> 0, true);
}

function monotonicNow() {
  const now = globalThis.performance?.now;
  if (typeof now !== "function") {
    throw new Error("@tinysandbox/js-runtime requires performance.now() for monotonic deadlines");
  }
  return now.call(globalThis.performance);
}

function jsonError(error) {
  const message = error instanceof Error ? error.message : String(error);
  const code = error && typeof error === "object" && typeof error.code === "string" ? error.code : undefined;
  return code === undefined ? { message } : { message, code };
}

function assertJsonValue(value, path = "value", seen = new Set()) {
  if (value === null || typeof value === "string" || typeof value === "boolean") return;
  if (typeof value === "number") {
    if (Number.isFinite(value)) return;
    throw new TypeError(`${path} must contain only finite JSON numbers`);
  }
  if (typeof value !== "object") throw new TypeError(`${path} must contain only JSON-safe values`);
  if (seen.has(value)) throw new TypeError(`${path} must not contain cycles`);
  const prototype = Object.getPrototypeOf(value);
  if (!Array.isArray(value) && prototype !== Object.prototype && prototype !== null) {
    throw new TypeError(`${path} must contain only plain objects and arrays`);
  }
  if (Object.getOwnPropertySymbols(value).length !== 0) throw new TypeError(`${path} must not contain symbol keys`);
  seen.add(value);
  if (Array.isArray(value)) {
    for (let index = 0; index < value.length; index++) {
      if (!Object.hasOwn(value, index)) throw new TypeError(`${path} must not contain array holes`);
      assertJsonValue(value[index], `${path}[${index}]`, seen);
    }
  } else {
    for (const key of Object.keys(value)) assertJsonValue(value[key], `${path}.${key}`, seen);
  }
  seen.delete(value);
}

export async function createEngine(wasm) {
  let module;
  if (wasm instanceof WebAssembly.Module) {
    module = wasm;
  } else if (ArrayBuffer.isView(wasm) || wasm instanceof ArrayBuffer) {
    module = await WebAssembly.compile(wasm);
  } else {
    throw new TypeError("createEngine expects wasm bytes or a WebAssembly.Module");
  }
  requireArtifactAbi(module);
  probeArtifactAbi(module);

  return Object.freeze({
    runCode(code, options = {}) {
      if (typeof code !== "string") throw new TypeError("code must be a string");
      if (options === null || typeof options !== "object") throw new TypeError("options must be an object");

      const wasmMemoryBytes = integerOption("wasmMemoryBytes", options.wasmMemoryBytes, DEFAULT_WASM_MEMORY_BYTES);
      const maximumPages = Math.min(65_536, Math.floor(wasmMemoryBytes / PAGE_BYTES));
      if (maximumPages < INITIAL_PAGES) {
        throw new RangeError(`wasmMemoryBytes must be at least ${QUICKJS_INITIAL_MEMORY_BYTES} bytes`);
      }
      const quickjsHeapBytes = integerOption("quickjsHeapBytes", options.quickjsHeapBytes, DEFAULT_QUICKJS_HEAP_BYTES, 1);
      if (quickjsHeapBytes > 0x7fff_ffff) throw new RangeError("quickjsHeapBytes must be at most 2147483647");
      const timeoutMs = integerOption("timeoutMs", options.timeoutMs, DEFAULT_TIMEOUT_MS, 1);
      const sourceBytes = integerOption("sourceBytes", options.sourceBytes, DEFAULT_SOURCE_BYTES);
      const hostResponseBytes = integerOption("hostResponseBytes", options.hostResponseBytes, DEFAULT_HOST_RESPONSE_BYTES);
      const stdoutBytes = integerOption("stdoutBytes", options.stdoutBytes, DEFAULT_OUTPUT_BYTES);
      const stderrBytes = integerOption("stderrBytes", options.stderrBytes, DEFAULT_OUTPUT_BYTES);
      const sourceLength = encoder.encode(code).byteLength;
      if (sourceLength > sourceBytes) throw new RangeError(`source exceeded limit of ${sourceBytes} bytes`);
      const globals = options.globals ?? {};
      const globalNames = validateGlobals(globals);
      const started = monotonicNow();
      const deadline = started + timeoutMs;
      const memory = new WebAssembly.Memory({ initial: INITIAL_PAGES, maximum: maximumPages });
      const stdout = [];
      const stderr = [];
      let stdoutLength = 0;
      let stderrLength = 0;
      let response = new Uint8Array();
      let timedOut = false;
      let limitFailure;

      const read = (pointer, length) => {
        if (!Number.isInteger(pointer) || !Number.isInteger(length) || pointer < 0 || length < 0 || pointer + length > memory.buffer.byteLength) {
          throw new WebAssembly.RuntimeError("guest memory access out of bounds");
        }
        return new Uint8Array(memory.buffer, pointer, length);
      };
      const capture = (target, pointer, length, cap, stream) => {
        const current = stream === "stdout" ? stdoutLength : stderrLength;
        if (length > cap - current) {
          limitFailure = new RunLimitError(stream, `${stream} exceeded limit of ${cap} bytes`);
          throw limitFailure;
        }
        target.push(read(pointer, length).slice());
        if (stream === "stdout") stdoutLength += length;
        else stderrLength += length;
        return length;
      };
      const setResponse = (value) => {
        let bytes;
        try {
          bytes = encoder.encode(JSON.stringify(value));
        } catch (error) {
          bytes = encoder.encode(JSON.stringify({ error: jsonError(error) }));
        }
        if (bytes.byteLength > hostResponseBytes) {
          bytes = encoder.encode(JSON.stringify({ error: { code: "E2BIG", message: `host response exceeded limit of ${hostResponseBytes} bytes` } }));
          if (bytes.byteLength > hostResponseBytes) {
            limitFailure = new RunLimitError("hostResponse", `host response exceeded limit of ${hostResponseBytes} bytes`);
            throw limitFailure;
          }
        }
        response = bytes;
      };
      const tinysandbox = {
        should_interrupt() {
          if (monotonicNow() >= deadline) timedOut = true;
          return timedOut ? 1 : 0;
        },
        host_call(opPointer, opLength, jsonPointer, jsonLength) {
          const op = decoder.decode(read(opPointer, opLength));
          let argument;
          try {
            argument = JSON.parse(decoder.decode(read(jsonPointer, jsonLength)));
          } catch (error) {
            setResponse({ error: { code: "EINVAL", message: `invalid host call JSON: ${error instanceof Error ? error.message : String(error)}` } });
            return 0;
          }
          if (op !== "global") {
            setResponse({ error: { code: "ENOSYS", message: `host capability '${op}' is not available` } });
            return 0;
          }
          const name = argument?.name;
          const handler = typeof name === "string" ? globals[name] : undefined;
          if (typeof handler !== "function") {
            setResponse({ error: { message: `unknown global '${String(name)}'` } });
            return 0;
          }
          try {
            const value = handler(argument.args ?? null);
            if (value && typeof value === "object" && typeof value.then === "function") {
              throw new TypeError(`global '${name}' returned a Promise; host globals must be synchronous`);
            }
            assertJsonValue(value, `global '${name}' response`);
            setResponse({ value });
          } catch (error) {
            setResponse({ error: jsonError(error) });
          }
          return 0;
        },
        host_response_len() { return response.byteLength; },
        host_response_read(pointer, length) {
          const count = Math.min(length, response.byteLength);
          read(pointer, count).set(response.subarray(0, count));
          return count;
        },
        write_stdout(pointer, length) { return capture(stdout, pointer, length, stdoutBytes, "stdout"); },
        write_stderr(pointer, length) { return capture(stderr, pointer, length, stderrBytes, "stderr"); },
      };
      const wasi = {
        clock_time_get(clockId, _precision, pointer) {
          const nanos = clockId === 1 ? BigInt(Math.floor((monotonicNow() - started) * 1_000_000)) : BigInt(Date.now()) * 1_000_000n;
          new DataView(memory.buffer).setBigUint64(pointer, nanos, true);
          return 0;
        },
        fd_close() { return 0; },
        fd_fdstat_get() { return 8; },
        fd_seek() { return 8; },
        fd_write(fd, iovs, iovsLength, writtenPointer) {
          if (fd !== 1 && fd !== 2) return 8;
          let total = 0;
          for (let index = 0; index < iovsLength; index++) {
            const view = new DataView(memory.buffer);
            const pointer = view.getUint32(iovs + index * 8, true);
            const length = view.getUint32(iovs + index * 8 + 4, true);
            capture(fd === 1 ? stdout : stderr, pointer, length, fd === 1 ? stdoutBytes : stderrBytes, fd === 1 ? "stdout" : "stderr");
            total += length;
          }
          writeU32(memory, writtenPointer, total);
          return 0;
        },
      };

      let instance;
      let exitCode = 1;
      try {
        instance = new WebAssembly.Instance(module, { env: { memory }, tinysandbox, wasi_snapshot_preview1: wasi });
        if (instance.exports.memory !== memory) throw new TypeError("QuickJS wasm did not re-export env.memory");
        if (typeof instance.exports._initialize === "function") instance.exports._initialize();
        const configBytes = encoder.encode(JSON.stringify({
          code,
          scriptPath: options.scriptPath ?? "[eval]",
          argv: options.argv ?? ["js", "-e"],
          env: options.env ?? {},
          cwd: options.cwd ?? "/",
          globals: globalNames,
          prelude: "",
        }));
        const pointer = instance.exports.tinysandbox_alloc(configBytes.byteLength);
        if (!pointer) throw new WebAssembly.RuntimeError("QuickJS input allocation failed");
        read(pointer, configBytes.byteLength).set(configBytes);
        exitCode = instance.exports.tinysandbox_run(pointer, configBytes.byteLength, quickjsHeapBytes);
        if (timedOut) {
          return { exitCode: 124, stdout: "", stderr: "js: command timed out\n", initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: memory.buffer.byteLength };
        }
        instance.exports.tinysandbox_free(pointer);
      } catch (error) {
        if (timedOut) {
          return { exitCode: 124, stdout: "", stderr: "js: command timed out\n", initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: memory.buffer.byteLength };
        }
        const isMemory = error instanceof WebAssembly.RuntimeError && /memory|allocation|out of bounds/i.test(error.message);
        const message = limitFailure?.message ?? (isMemory ? "wasm memory limit exceeded" : `runtime trap: ${error instanceof Error ? error.message : String(error)}`);
        return { exitCode: 1, stdout: decoder.decode(join(stdout, stdoutLength)), stderr: `js: ${message}\n`, initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: memory.buffer.byteLength };
      }
      return {
        exitCode,
        stdout: decoder.decode(join(stdout, stdoutLength)),
        stderr: decoder.decode(join(stderr, stderrLength)),
        initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES,
        peakWasmMemoryBytes: memory.buffer.byteLength,
      };
    },
  });
}

function join(chunks, length) {
  const result = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}
