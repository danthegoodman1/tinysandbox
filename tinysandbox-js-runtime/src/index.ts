import type { JsonValue, HostGlobal, HostContext, VfsErrno, VfsMetadata, VfsOpenMode, Vfs, RunCodeOptions, RunFileOptions, RunResult, JsEngine } from "./types.js";
export type * from "./types.js";

type ErrorPayload = { message: string; code?: string };
type VfsErrorPayload = { code: VfsErrno; errno: number; message: string; syscall: string; path?: string };
type GuestExports = WebAssembly.Exports & {
  memory: WebAssembly.Memory;
  tinysandbox_abi_version(): number;
  tinysandbox_alloc(length: number): number;
  tinysandbox_free(pointer: number): void;
  tinysandbox_run(pointer: number, length: number, heap: number): number;
};
function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" ? value as Record<string, unknown> : {};
}
function thenable(value: unknown): value is PromiseLike<unknown> {
  return typeof record(value).then === "function";
}

const PAGE_BYTES = 64 * 1024;
const INITIAL_PAGES = 19;
const DEFAULT_WASM_MEMORY_BYTES = 64 * 1024 * 1024;
const DEFAULT_QUICKJS_HEAP_BYTES = 32 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS = 30_000;
const DEFAULT_SOURCE_BYTES = 1024 * 1024;
const DEFAULT_HOST_RESPONSE_BYTES = 1024 * 1024;
const DEFAULT_OUTPUT_BYTES = 1024 * 1024;
const DEFAULT_HOST_INPUT_BYTES = 8 * 1024 * 1024;
const DEFAULT_MAX_OPEN_FILES = 1024;
const MAX_HOST_READ_BYTES = 16 * 1024 * 1024;
const VFS_OPERATIONS = new Set([
  "readFile", "writeFile", "appendFile", "mkdir", "readdir", "stat", "rename",
  "rm", "unlink", "rmdir", "exists", "open", "read", "write", "ftruncate",
  "close", "copyFile",
]);
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
const fatalDecoder = new TextDecoder("utf-8", { fatal: true });

export const QUICKJS_INITIAL_MEMORY_BYTES = INITIAL_PAGES * PAGE_BYTES;

const VFS_ERRNOS: Readonly<Record<VfsErrno, readonly [number, string]>> = Object.freeze({
  EBADF: [-9, "bad file descriptor"],
  EBUSY: [-16, "resource busy or locked"],
  EXDEV: [-18, "cross-device link not permitted"],
  EACCES: [-13, "permission denied"],
  EEXIST: [-17, "file already exists"],
  EFBIG: [-27, "file too large"],
  EIO: [-5, "i/o error"],
  EINVAL: [-22, "invalid argument"],
  EISDIR: [-21, "illegal operation on a directory"],
  ENOENT: [-2, "no such file or directory"],
  ENOSPC: [-28, "no space left on device"],
  ENOTDIR: [-20, "not a directory"],
  ENOTEMPTY: [-66, "directory not empty"],
});

export class VfsError extends Error {
  readonly code: VfsErrno;
  constructor(code: VfsErrno, message: string = code) {
    if (!Object.hasOwn(VFS_ERRNOS, code)) throw new TypeError(`unsupported VFS errno '${String(code)}'`);
    super(message);
    this.name = "VfsError";
    this.code = code;
  }
}

class RunLimitError extends Error {
  readonly kind: string;
  constructor(kind: string, message: string) {
    super(message);
    this.kind = kind;
  }
}

function integerOption(name: string, value: number | undefined, fallback: number, minimum = 0) {
  const result = value === undefined ? fallback : value;
  if (!Number.isSafeInteger(result) || result < minimum) {
    throw new TypeError(`${name} must be a safe integer of at least ${minimum}`);
  }
  return result;
}

function validateGlobals(globals: Record<string, HostGlobal> | undefined) {
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

function requireArtifactAbi(module: WebAssembly.Module) {
  const imports = WebAssembly.Module.imports(module);
  const actualImports = imports.map((entry) => `${entry.module}.${entry.name}:${entry.kind}`).sort();
  const actualExports = WebAssembly.Module.exports(module).map((entry) => `${entry.name}:${entry.kind}`).sort();
  if (JSON.stringify(actualImports) !== JSON.stringify(REQUIRED_IMPORTS)) throw new TypeError(`incompatible QuickJS wasm imports: ${actualImports.join(", ")}`);
  if (JSON.stringify(actualExports) !== JSON.stringify(REQUIRED_EXPORTS)) throw new TypeError(`incompatible QuickJS wasm exports: ${actualExports.join(", ")}`);
}

function probeArtifactAbi(module: WebAssembly.Module) {
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
  for (const [name, arity] of [["tinysandbox_abi_version", 0], ["tinysandbox_alloc", 1], ["tinysandbox_free", 1], ["tinysandbox_run", 3]] as const) {
    if ((instance.exports[name] as Function).length !== arity) throw new TypeError(`incompatible QuickJS wasm export signature '${name}'`);
  }
  if ((instance.exports as GuestExports).tinysandbox_abi_version() !== ABI_VERSION) throw new TypeError(`incompatible QuickJS wasm ABI version; expected ${ABI_VERSION}`);
}

function writeU32(memory: WebAssembly.Memory, pointer: number, value: number) {
  new DataView(memory.buffer).setUint32(pointer, value >>> 0, true);
}

function monotonicNow() {
  const now = globalThis.performance?.now;
  if (typeof now !== "function") {
    throw new Error("@tinysandbox/js-runtime requires performance.now() for monotonic deadlines");
  }
  return now.call(globalThis.performance);
}

function jsonError(error: unknown): ErrorPayload {
  const message = error instanceof Error ? error.message : String(error);
  const code = error && typeof error === "object" && typeof record(error).code === "string" ? record(error).code as string : undefined;
  return code === undefined ? { message } : { message, code };
}

function assertJsonValue(value: unknown, path = "value", seen = new Set<unknown>()): asserts value is JsonValue {
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
    for (const key of Object.keys(value)) assertJsonValue(record(value)[key], `${path}.${key}`, seen);
  }
  seen.delete(value);
}

class HostCallFailure extends Error {
  readonly payload: VfsErrorPayload;
  constructor(payload: VfsErrorPayload) {
    super(payload.message);
    this.payload = payload;
  }
}

function vfsFailure(error: unknown, syscall: string, path?: string) {
  const candidate = record(error).code;
  const code: VfsErrno = typeof candidate === "string" && Object.hasOwn(VFS_ERRNOS, candidate) ? candidate as VfsErrno : "EIO";
  const [errno, message] = VFS_ERRNOS[code];
  return new HostCallFailure({ code, errno, message, syscall, ...(path === undefined ? {} : { path }) });
}

function invalidVfsCall(syscall: string, path?: string) {
  return vfsFailure(new VfsError("EINVAL"), syscall, path);
}

function validateVfs(vfs: unknown): Vfs {
  if (vfs === null || typeof vfs !== "object") throw new TypeError("vfs must be an object implementing the synchronous Vfs interface");
  for (const method of ["stat", "readdir", "mkdir", "rename", "unlink", "rmdir", "open", "readAt", "writeAt", "truncate", "close"]) {
    if (typeof record(vfs)[method] !== "function") throw new TypeError(`vfs.${method} must be a function`);
  }
  if (record(vfs).abort !== undefined && typeof record(vfs).abort !== "function") throw new TypeError("vfs.abort must be a function when supplied");
  return vfs as Vfs;
}

function callVfs<K extends keyof Vfs>(vfs: Vfs, method: K, args: Parameters<NonNullable<Vfs[K]>>, syscall: string, path?: string): ReturnType<NonNullable<Vfs[K]>> {
  try {
    const operation = vfs[method] as (...parameters: Parameters<NonNullable<Vfs[K]>>) => ReturnType<NonNullable<Vfs[K]>>;
    const value = operation.apply(vfs, args);
    if (thenable(value)) {
      // Async VFS methods are unsupported, but attach a rejection handler so a
      // mistaken async implementation cannot escape this deterministic error.
      try { value.then(() => {}, () => {}); } catch {}
      throw new TypeError(`vfs.${method} returned a Promise; Vfs methods must be synchronous`);
    }
    return value;
  } catch (error) {
    throw vfsFailure(error, syscall, path);
  }
}

function normalizeAbsolute(path: string) {
  const parts = [];
  for (const part of String(path).split("/")) {
    if (part === "" || part === ".") continue;
    if (part === "..") parts.pop();
    else parts.push(part);
  }
  return parts.length === 0 ? "/" : `/${parts.join("/")}`;
}

function resolvePath(cwd: string, path: string) {
  return normalizeAbsolute(String(path).startsWith("/") ? path : cwd === "/" ? `/${path}` : `${cwd}/${path}`);
}

function checkedInteger(value: unknown, syscall: string, path?: string, nullable?: false): number;
function checkedInteger(value: unknown, syscall: string, path: string | undefined, nullable: true): number | null;
function checkedInteger(value: unknown, syscall: string, path?: string, nullable = false): number | null {
  if (nullable && (value === null || value === undefined)) return null;
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw invalidVfsCall(syscall, path);
  return value;
}

function checkedMetadata(value: VfsMetadata, syscall: string, path?: string) {
  if (!value || typeof value !== "object" || !["file", "directory"].includes(value.fileType) || !Number.isSafeInteger(value.len) || value.len < 0) {
    throw vfsFailure(new Error("invalid VFS metadata"), syscall, path);
  }
  return value;
}

function encodeBase64(data: Uint8Array) {
  const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const output = new Uint8Array(Math.ceil(data.byteLength / 3) * 4);
  for (let index = 0, target = 0; index < data.byteLength; index += 3) {
    const first = data[index];
    const second = index + 1 < data.byteLength ? data[index + 1] : 0;
    const third = index + 2 < data.byteLength ? data[index + 2] : 0;
    const bits = (first << 16) | (second << 8) | third;
    output[target++] = chars.charCodeAt((bits >>> 18) & 63);
    output[target++] = chars.charCodeAt((bits >>> 12) & 63);
    output[target++] = index + 1 < data.byteLength ? chars.charCodeAt((bits >>> 6) & 63) : 61;
    output[target++] = index + 2 < data.byteLength ? chars.charCodeAt(bits & 63) : 61;
  }
  return decoder.decode(output);
}

function decodeBase64(value: unknown, maximumBytes = Number.MAX_SAFE_INTEGER) {
  const input = String(value);
  if (input.length === 0) return new Uint8Array();
  if (input.length % 4 !== 0 || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(input)) {
    throw invalidVfsCall("tinysandbox");
  }
  const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const padding = input.endsWith("==") ? 2 : input.endsWith("=") ? 1 : 0;
  const length = input.length / 4 * 3 - padding;
  if (length > maximumBytes) throw vfsFailure(new VfsError("EFBIG"), "write");
  const output = new Uint8Array(length);
  let outputIndex = 0;
  for (let index = 0; index < input.length; index += 4) {
    const bits = (chars.indexOf(input[index]) << 18) | (chars.indexOf(input[index + 1]) << 12)
      | ((input[index + 2] === "=" ? 0 : chars.indexOf(input[index + 2])) << 6)
      | (input[index + 3] === "=" ? 0 : chars.indexOf(input[index + 3]));
    output[outputIndex++] = (bits >>> 16) & 255;
    if (outputIndex < output.length) output[outputIndex++] = (bits >>> 8) & 255;
    if (outputIndex < output.length) output[outputIndex++] = bits & 255;
  }
  return output;
}

function openMode(flags: unknown, path: string): VfsOpenMode {
  switch (flags) {
    case "r": return { read: true, write: false, create: false, createNew: false, truncate: false, append: false };
    case "r+": return { read: true, write: true, create: false, createNew: false, truncate: false, append: false };
    case "w": return { read: false, write: true, create: true, createNew: false, truncate: true, append: false };
    case "wx": case "xw": return { read: false, write: true, create: true, createNew: true, truncate: true, append: false };
    case "w+": return { read: true, write: true, create: true, createNew: false, truncate: true, append: false };
    case "wx+": case "w+x": case "xw+": case "x+w": return { read: true, write: true, create: true, createNew: true, truncate: true, append: false };
    case "a": return { read: false, write: true, create: true, createNew: false, truncate: false, append: true };
    case "ax": case "xa": return { read: false, write: true, create: true, createNew: true, truncate: false, append: true };
    case "a+": return { read: true, write: true, create: true, createNew: false, truncate: false, append: true };
    case "ax+": case "a+x": case "xa+": case "x+a": return { read: true, write: true, create: true, createNew: true, truncate: false, append: true };
    default: throw invalidVfsCall("open", path);
  }
}

function openVfs(vfs: Vfs, path: string, mode: VfsOpenMode) {
  const handle = callVfs(vfs, "open", [path, mode], "open", path);
  if (!Number.isSafeInteger(handle) || handle < 0) {
    try { callVfs(vfs, "close", [handle], "close"); } catch {}
    throw vfsFailure(new Error("invalid VFS handle"), "open", path);
  }
  return handle;
}

function readAll(vfs: Vfs, path: string, maximumBytes: number, checkpoint: () => void = () => {}) {
  checkpoint();
  const handle = openVfs(vfs, path, openMode("r", path));
  const chunks: Uint8Array[] = [];
  let length = 0;
  let failure: unknown;
  try {
    for (;;) {
      checkpoint();
      const buffer = new Uint8Array(Math.min(8192, maximumBytes - Math.min(length, maximumBytes) + 1));
      const count = callVfs(vfs, "readAt", [handle, length, buffer], "read");
      checkpoint();
      if (!Number.isSafeInteger(count) || count < 0 || count > buffer.byteLength) throw vfsFailure(new Error("invalid VFS read count"), "read");
      if (count === 0) break;
      chunks.push(buffer.slice(0, count));
      length += count;
      if (length > maximumBytes) throw new RangeError(`source exceeded limit of ${maximumBytes} bytes`);
    }
  } catch (error) {
    failure = error;
  }
  try {
    callVfs(vfs, "close", [handle], "close");
  } catch (error) {
    if (failure === undefined) failure = error;
  }
  if (failure !== undefined) throw failure;
  return join(chunks, length);
}

function writeAll(vfs: Vfs, path: string, data: Uint8Array, append: boolean, checkpoint: () => void) {
  checkpoint();
  const mode = openMode(append ? "a" : "w", path);
  const handle = openVfs(vfs, path, mode);
  let written = 0;
  let failure: unknown;
  try {
    while (written < data.byteLength) {
      checkpoint();
      const count = callVfs(vfs, "writeAt", [handle, written, data.subarray(written)], "write");
      if (!Number.isSafeInteger(count) || count < 0 || count > data.byteLength - written) throw vfsFailure(new Error("invalid VFS write count"), "write");
      if (count === 0) throw vfsFailure(new VfsError("ENOSPC"), "write");
      written += count;
    }
  } catch (error) {
    failure = error;
  }
  try {
    callVfs(vfs, failure !== undefined && vfs.abort ? "abort" : "close", [handle], "close");
  } catch (error) {
    if (failure === undefined) failure = error;
  }
  if (failure !== undefined) throw failure;
}

function createVfsDispatcher(vfs: Vfs, cwd: string, hostInputBytes: number, hostResponseBytes: number, maxOpenFiles: number, checkpoint: () => void) {
  validateVfs(vfs);
  const files = new Map<number, { handle: number; position: number }>();
  let nextFd = 3;
  const absolute = (value: unknown) => {
    if (typeof value !== "string") throw invalidVfsCall("tinysandbox");
    const path = resolvePath(cwd, value);
    if (path.includes("\0") || path.split("/").length > 257) throw invalidVfsCall("tinysandbox", path);
    return path;
  };
  const readBytes = Math.min(hostInputBytes, Math.max(0, Math.floor((hostResponseBytes - 128) / 4) * 3));
  const readFile = (path: string, cap: number) => {
    try { return readAll(vfs, path, cap, checkpoint); }
    catch (error) {
      if (error instanceof RangeError) throw vfsFailure(new VfsError("EFBIG"), "open", path);
      throw error;
    }
  };
  const metadata = (path: string, syscall = "stat") => checkedMetadata(callVfs(vfs, "stat", [path], syscall, path), syscall, path);
  const mkdirRecursive = (path: string) => {
    checkpoint();
    if (path === "/") return;
    let current = "";
    for (const part of path.slice(1).split("/")) {
      checkpoint();
      current += `/${part}`;
      try {
        callVfs(vfs, "mkdir", [current], "mkdir", path);
      } catch (error) {
        if (!(error instanceof HostCallFailure) || error.payload.code !== "EEXIST" || metadata(current).fileType !== "directory") throw error;
      }
    }
  };
  const removePath = (path: string, recursive: boolean): void => {
    checkpoint();
    if (path.split("/").length > 257) throw invalidVfsCall("rm", path);
    const info = metadata(path, "rm");
    if (info.fileType === "file") return callVfs(vfs, "unlink", [path], "unlink", path);
    if (!recursive) throw vfsFailure(new VfsError("EISDIR"), "rm", path);
    const entries = callVfs(vfs, "readdir", [path], "scandir", path);
    if (!Array.isArray(entries)) throw vfsFailure(new Error("invalid VFS directory entries"), "scandir", path);
    for (const entry of entries) {
      if (!entry || typeof entry.name !== "string" || entry.name.includes("/")) throw vfsFailure(new Error("invalid VFS directory entry"), "scandir", path);
      removePath(path === "/" ? `/${entry.name}` : `${path}/${entry.name}`, true);
    }
    return callVfs(vfs, "rmdir", [path], "rmdir", path);
  };
  const dispatch = (op: string, args: Record<string, unknown>): JsonValue => {
    checkpoint();
    switch (op) {
      case "readFile": {
        const path = absolute(args?.path);
        return encodeBase64(remapHostFailure(() => readFile(path, readBytes), "open", path));
      }
      case "writeFile": {
        const path = absolute(args?.path);
        remapHostFailure(() => writeAll(vfs, path, decodeBase64(args?.data, hostInputBytes), false, checkpoint), "open", path);
        return null;
      }
      case "appendFile": {
        const path = absolute(args?.path);
        remapHostFailure(() => writeAll(vfs, path, decodeBase64(args?.data, hostInputBytes), true, checkpoint), "open", path);
        return null;
      }
      case "mkdir": { const path = absolute(args?.path); args?.recursive ? mkdirRecursive(path) : callVfs(vfs, "mkdir", [path], "mkdir", path); return null; }
      case "readdir": {
        const path = absolute(args?.path);
        const entries = callVfs(vfs, "readdir", [path], "scandir", path);
        if (!Array.isArray(entries)) throw vfsFailure(new Error("invalid VFS directory entries"), "scandir", path);
        return entries.map((entry) => {
          if (!entry || typeof entry.name !== "string" || entry.name.includes("/")) throw vfsFailure(new Error("invalid VFS directory entry"), "scandir", path);
          if (!args?.withFileTypes) return entry.name;
          const info = checkedMetadata(entry.metadata, "scandir", path);
          return { name: entry.name, isFile: info.fileType === "file", isDirectory: info.fileType === "directory" };
        });
      }
      case "stat": { const info = metadata(absolute(args?.path)); return { size: info.len, isFile: info.fileType === "file", isDirectory: info.fileType === "directory" }; }
      case "rename": { const from = absolute(args?.from); const to = absolute(args?.to); callVfs(vfs, "rename", [from, to], "rename", from); return null; }
      case "rm": {
        const path = absolute(args?.path);
        try { removePath(path, !!args?.recursive); } catch (error) { if (!(args?.force && error instanceof HostCallFailure && error.payload.code === "ENOENT")) throw error; }
        return null;
      }
      case "unlink": { const path = absolute(args?.path); callVfs(vfs, "unlink", [path], "unlink", path); return null; }
      case "rmdir": { const path = absolute(args?.path); args?.recursive ? removePath(path, true) : callVfs(vfs, "rmdir", [path], "rmdir", path); return null; }
      case "exists": { try { metadata(absolute(args?.path)); return true; } catch { return false; } }
      case "open": {
        if (files.size >= maxOpenFiles || nextFd >= Number.MAX_SAFE_INTEGER) throw vfsFailure(new VfsError("ENOSPC"), "open");
        const path = absolute(args?.path);
        const handle = openVfs(vfs, path, openMode(args?.flags, path));
        const fd = nextFd++;
        files.set(fd, { handle, position: 0 });
        return fd;
      }
      case "read": {
        const fd = checkedInteger(args?.fd, "tinysandbox");
        const file = files.get(fd);
        if (!file) throw vfsFailure(new VfsError("EBADF"), "read");
        const position = checkedInteger(args?.position, "tinysandbox", undefined, true);
        const offset = position ?? file.position;
        const length = Math.min(MAX_HOST_READ_BYTES, readBytes, checkedInteger(args?.length, "tinysandbox"));
        const buffer = new Uint8Array(length);
        const count = callVfs(vfs, "readAt", [file.handle, offset, buffer], "read");
        if (!Number.isSafeInteger(count) || count < 0 || count > length) throw vfsFailure(new Error("invalid VFS read count"), "read");
        if (position === null) file.position += count;
        return { bytesRead: count, data: encodeBase64(buffer.subarray(0, count)) };
      }
      case "write": {
        const fd = checkedInteger(args?.fd, "tinysandbox");
        const file = files.get(fd);
        if (!file) throw vfsFailure(new VfsError("EBADF"), "write");
        const position = checkedInteger(args?.position, "tinysandbox", undefined, true);
        const offset = position ?? file.position;
        const data = decodeBase64(args?.data, hostInputBytes);
        const count = callVfs(vfs, "writeAt", [file.handle, offset, data], "write");
        if (!Number.isSafeInteger(count) || count < 0 || count > data.byteLength) throw vfsFailure(new Error("invalid VFS write count"), "write");
        if (position === null) file.position += count;
        return count;
      }
      case "ftruncate": {
        const fd = checkedInteger(args?.fd, "tinysandbox");
        const file = files.get(fd);
        if (!file) throw vfsFailure(new VfsError("EBADF"), "ftruncate");
        callVfs(vfs, "truncate", [file.handle, checkedInteger(args?.len ?? 0, "tinysandbox")], "ftruncate");
        return null;
      }
      case "close": {
        const fd = checkedInteger(args?.fd, "tinysandbox");
        const file = files.get(fd);
        if (!file) throw vfsFailure(new VfsError("EBADF"), "close");
        files.delete(fd);
        callVfs(vfs, "close", [file.handle], "close");
        return null;
      }
      case "copyFile": {
        const source = absolute(args?.src);
        const destination = absolute(args?.dest);
        const data = remapHostFailure(() => readFile(source, hostInputBytes), "copyfile", source);
        remapHostFailure(() => writeAll(vfs, destination, data, false, checkpoint), "copyfile", destination);
        return null;
      }
      default: throw invalidVfsCall("tinysandbox");
    }
  };
  const closeAll = (commit: boolean) => {
    let failure: unknown;
    for (const { handle } of files.values()) {
      try { callVfs(vfs, !commit && vfs.abort ? "abort" : "close", [handle], "close"); } catch (error) { failure ??= error; }
    }
    files.clear();
    return failure;
  };
  return { dispatch, closeAll };
}

function exposedHostError(payload: VfsErrorPayload) {
  const where = payload.path === undefined ? (payload.syscall ? `, ${payload.syscall}` : "") : `, ${payload.syscall} '${payload.path}'`;
  const error = new Error(`${payload.code}: ${payload.message}${where}`) as Error & VfsErrorPayload;
  error.code = payload.code;
  error.errno = payload.errno;
  error.syscall = payload.syscall;
  if (payload.path !== undefined) error.path = payload.path;
  return error;
}

function remapHostFailure<T>(operation: () => T, syscall: string, path?: string): T {
  try {
    return operation();
  } catch (error) {
    if (error instanceof HostCallFailure) throw vfsFailure(new VfsError(error.payload.code), syscall, path);
    throw error;
  }
}

export async function createEngine(wasm: BufferSource | WebAssembly.Module): Promise<JsEngine> {
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

  const engine = {
    runCode(code: string, options: RunCodeOptions = {}): RunResult {
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
      const hostInputBytes = integerOption("hostInputBytes", options.hostInputBytes, DEFAULT_HOST_INPUT_BYTES);
      const maxOpenFiles = integerOption("maxOpenFiles", options.maxOpenFiles, DEFAULT_MAX_OPEN_FILES);
      const stdoutBytes = integerOption("stdoutBytes", options.stdoutBytes, DEFAULT_OUTPUT_BYTES);
      const stderrBytes = integerOption("stderrBytes", options.stderrBytes, DEFAULT_OUTPUT_BYTES);
      const sourceLength = utf8Length(code, sourceBytes);
      if (sourceLength > sourceBytes) throw new RangeError(`source exceeded limit of ${sourceBytes} bytes`);
      const globals = options.globals ?? {};
      const globalNames = validateGlobals(globals);
      const vfs = options.vfs === undefined ? undefined : validateVfs(options.vfs);
      const started = monotonicNow();
      const deadline = started + timeoutMs;
      const signal = options.signal;
      if (signal !== undefined && (typeof signal.aborted !== "boolean" || typeof signal.addEventListener !== "function" || typeof signal.removeEventListener !== "function")) {
        throw new TypeError("signal must be an AbortSignal");
      }
      const controller = new AbortController();
      const memory = new WebAssembly.Memory({ initial: INITIAL_PAGES, maximum: maximumPages });
      const stdout: Uint8Array[] = [];
      const stderr: Uint8Array[] = [];
      let stdoutLength = 0;
      let stderrLength = 0;
      let response: Uint8Array = new Uint8Array();
      let timedOut = false;
      let finished = false;
      let limitFailure: RunLimitError | undefined;
      const isCancelled = () => {
        if (signal?.aborted || monotonicNow() >= deadline) timedOut = true;
        if ((finished || timedOut) && !controller.signal.aborted) {
          controller.abort(signal?.reason ?? new DOMException("Execution ended", "AbortError"));
        }
        return finished || timedOut;
      };
      const deadlineMs = Date.now() + Math.max(0, deadline - monotonicNow());
      const callbackContext = () => {
        const callback = new AbortController();
        const abort = () => { callback.abort(controller.signal.reason); };
        const cancelled = () => {
          if (isCancelled()) abort();
          return callback.signal.aborted;
        };
        controller.signal.addEventListener("abort", abort, { once: true });
        const context: HostContext = Object.freeze({
          signal: callback.signal,
          deadlineMs,
          remainingTimeMs: () => { cancelled(); return Math.max(0, deadline - monotonicNow()); },
          isCancelled: cancelled,
        });
        return {
          context,
          dispose: () => {
            controller.signal.removeEventListener("abort", abort);
            callback.abort(new DOMException("Host callback completed", "AbortError"));
          },
        };
      };
      const onAbort = () => { isCancelled(); };
      const cancellationMessage = () => signal?.aborted ? "command cancelled" : "command timed out";
      const checkpoint = () => {
        if (isCancelled()) throw new RunLimitError("timeout", cancellationMessage());
      };
      const vfsDispatcher = vfs === undefined ? undefined : createVfsDispatcher(vfs, resolvePath("/", options.cwd ?? "/"), hostInputBytes, hostResponseBytes, maxOpenFiles, checkpoint);

      const read = (pointer: number, length: number) => {
        if (!Number.isInteger(pointer) || !Number.isInteger(length) || pointer < 0 || length < 0 || pointer + length > memory.buffer.byteLength) {
          throw new WebAssembly.RuntimeError("guest memory access out of bounds");
        }
        return new Uint8Array(memory.buffer, pointer, length);
      };
      const capture = (target: Uint8Array[], pointer: number, length: number, cap: number, stream: string) => {
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
      const setResponse = (value: unknown) => {
        let bytes: Uint8Array;
        try {
          bytes = boundedJson(value, hostResponseBytes);
        } catch (error) {
          try {
            bytes = boundedJson({ error: { code: "E2BIG", message: `host response exceeded limit of ${hostResponseBytes} bytes` } }, hostResponseBytes);
          } catch {
            limitFailure = new RunLimitError("hostResponse", `host response exceeded limit of ${hostResponseBytes} bytes`);
            throw limitFailure;
          }
        }
        response = bytes;
      };
      const tinysandbox = {
        should_interrupt() {
          return isCancelled() ? 1 : 0;
        },
        host_call(opPointer: number, opLength: number, jsonPointer: number, jsonLength: number) {
          checkpoint();
          const op = decoder.decode(read(opPointer, opLength));
          let argument: Record<string, unknown>;
          try {
            argument = record(JSON.parse(decoder.decode(read(jsonPointer, jsonLength))));
          } catch (error) {
            setResponse({ error: { code: "EINVAL", message: `invalid host call JSON: ${error instanceof Error ? error.message : String(error)}` } });
            return 0;
          }
          if (op !== "global" && (!VFS_OPERATIONS.has(op) || vfsDispatcher === undefined)) {
            setResponse({ error: { code: "ENOSYS", message: `host capability '${op}' is not available` } });
            return 0;
          }
          if (op !== "global") {
            try {
              const value = vfsDispatcher!.dispatch(op, argument);
              checkpoint();
              setResponse({ value });
            } catch (error) {
              setResponse({ error: error instanceof HostCallFailure ? error.payload : jsonError(error) });
            }
            return 0;
          }
          const name = argument?.name;
          const handler = typeof name === "string" ? globals[name] : undefined;
          if (typeof handler !== "function") {
            setResponse({ error: { message: `unknown global '${String(name)}'` } });
            return 0;
          }
          const host = callbackContext();
          try {
            const payload = argument.args ?? null;
            assertJsonValue(payload);
            const value = handler(payload, host.context);
            checkpoint();
            if (thenable(value)) {
              throw new TypeError(`global '${name}' returned a Promise; host globals must be synchronous`);
            }
            assertJsonValue(value, `global '${name}' response`);
            setResponse({ value });
          } catch (error) {
            setResponse({ error: jsonError(error) });
          } finally {
            host.dispose();
          }
          return 0;
        },
        host_response_len() { return response.byteLength; },
        host_response_read(pointer: number, length: number) {
          const count = Math.min(length, response.byteLength);
          read(pointer, count).set(response.subarray(0, count));
          return count;
        },
        write_stdout(pointer: number, length: number) { return capture(stdout, pointer, length, stdoutBytes, "stdout"); },
        write_stderr(pointer: number, length: number) { return capture(stderr, pointer, length, stderrBytes, "stderr"); },
      };
      const wasi = {
        clock_time_get(clockId: number, _precision: bigint, pointer: number) {
          const nanos = clockId === 1 ? BigInt(Math.floor((monotonicNow() - started) * 1_000_000)) : BigInt(Date.now()) * 1_000_000n;
          new DataView(memory.buffer).setBigUint64(pointer, nanos, true);
          return 0;
        },
        fd_close() { return 0; },
        fd_fdstat_get() { return 8; },
        fd_seek() { return 8; },
        fd_write(fd: number, iovs: number, iovsLength: number, writtenPointer: number) {
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
      let cleanupFailure: unknown;
      let completed = false;
      try {
        signal?.addEventListener("abort", onAbort, { once: true });
        checkpoint();
        instance = new WebAssembly.Instance(module, { env: { memory }, tinysandbox, wasi_snapshot_preview1: wasi });
        if (instance.exports.memory !== memory) throw new TypeError("QuickJS wasm did not re-export env.memory");
        if (typeof instance.exports._initialize === "function") instance.exports._initialize();
        const configBytes = boundedJson({
          code,
          scriptPath: options.scriptPath ?? "[eval]",
          argv: options.argv ?? ["js", "-e"],
          env: options.env ?? {},
          cwd: options.cwd ?? "/",
          globals: globalNames,
          prelude: "",
          vfs: vfs !== undefined,
        }, wasmMemoryBytes);
        const pointer = (instance.exports as GuestExports).tinysandbox_alloc(configBytes.byteLength);
        if (!pointer) throw new WebAssembly.RuntimeError("QuickJS input allocation failed");
        read(pointer, configBytes.byteLength).set(configBytes);
        exitCode = (instance.exports as GuestExports).tinysandbox_run(pointer, configBytes.byteLength, quickjsHeapBytes);
        isCancelled();
        if (timedOut) {
          return { exitCode: 124, stdout: "", stderr: `js: ${cancellationMessage()}\n`, initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: memory.buffer.byteLength };
        }
        (instance.exports as GuestExports).tinysandbox_free(pointer);
        completed = true;
      } catch (error) {
        if (timedOut) {
          return { exitCode: 124, stdout: "", stderr: `js: ${cancellationMessage()}\n`, initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: memory.buffer.byteLength };
        }
        const isMemory = error instanceof WebAssembly.RuntimeError && /memory|allocation|out of bounds/i.test(error.message);
        const message = limitFailure?.message ?? (isMemory ? "wasm memory limit exceeded" : `runtime trap: ${error instanceof Error ? error.message : String(error)}`);
        return { exitCode: 1, stdout: decoder.decode(join(stdout, stdoutLength)), stderr: `js: ${message}\n`, initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: memory.buffer.byteLength };
      } finally {
        finished = true;
        signal?.removeEventListener("abort", onAbort);
        isCancelled();
        cleanupFailure = vfsDispatcher?.closeAll(completed && exitCode === 0 && !timedOut && limitFailure === undefined);
      }
      if (cleanupFailure !== undefined && exitCode === 0) {
        exitCode = 1;
        stderr.push(encoder.encode(`js: ${jsonError(cleanupFailure).message}\n`));
        stderrLength = stderr.reduce((total, chunk) => total + chunk.length, 0);
      }
      return {
        exitCode,
        stdout: decoder.decode(join(stdout, stdoutLength)),
        stderr: decoder.decode(join(stderr, stderrLength)),
        initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES,
        peakWasmMemoryBytes: memory.buffer.byteLength,
      };
    },
    runFile(path: string, options: RunFileOptions): RunResult {
      if (typeof path !== "string") throw new TypeError("path must be a string");
      if (options === null || typeof options !== "object") throw new TypeError("options must be an object");
      const deadline = monotonicNow() + integerOption("timeoutMs", options.timeoutMs, DEFAULT_TIMEOUT_MS, 1);
      const checkpoint = () => {
        if (options.signal?.aborted) throw new RunLimitError("timeout", "command cancelled");
        if (monotonicNow() >= deadline) throw new RunLimitError("timeout", "command timed out");
      };
      const vfs = validateVfs(options.vfs);
      const cwd = resolvePath("/", options.cwd ?? "/");
      const resolved = resolvePath(cwd, path);
      const sourceBytes = integerOption("sourceBytes", options.sourceBytes, DEFAULT_SOURCE_BYTES);
      let bytes;
      try {
        bytes = remapHostFailure(() => readAll(vfs, resolved, Math.min(sourceBytes, integerOption("hostInputBytes", options.hostInputBytes, DEFAULT_HOST_INPUT_BYTES)), checkpoint), "open", resolved);
      } catch (error) {
        if (error instanceof RunLimitError && error.kind === "timeout") return { exitCode: 124, stdout: "", stderr: `js: ${error.message}\n`, initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: 0 };
        if (error instanceof HostCallFailure) throw exposedHostError(error.payload);
        throw error;
      }
      let code;
      try {
        code = fatalDecoder.decode(bytes);
      } catch {
        throw new TypeError(`runFile entry '${resolved}' is not valid UTF-8`);
      }
      if (monotonicNow() >= deadline) return { exitCode: 124, stdout: "", stderr: "js: command timed out\n", initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: 0 };
      return engine.runCode(code, {
        ...options,
        timeoutMs: Math.max(1, Math.floor(deadline - monotonicNow())),
        vfs,
        cwd,
        scriptPath: resolved,
        argv: options.argv ?? ["js", path],
      });
    },
  };
  return Object.freeze(engine);
}

function join(chunks: Uint8Array[], length: number) {
  const result = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}

// Count UTF-8 without allocating a temporary encoding; stop once the cap fails.
function utf8Length(value: string, cap: number): number {
  let bytes = 0;
  for (let index = 0; index < value.length; index++) {
    const code = value.charCodeAt(index);
    if (code < 0x80) bytes++;
    else if (code < 0x800) bytes += 2;
    else if (code >= 0xd800 && code <= 0xdbff && value.charCodeAt(index + 1) >= 0xdc00 && value.charCodeAt(index + 1) <= 0xdfff) { bytes += 4; index++; }
    else bytes += 3;
    if (bytes > cap) return bytes;
  }
  return bytes;
}

function boundedJson(value: unknown, cap: number): Uint8Array {
  const parts: string[] = [];
  let bytes = 0;
  const append = (part: string, length = part.length) => {
    if (length > cap - bytes) throw new RunLimitError("hostResponse", "host response exceeded limit");
    bytes += length;
    parts.push(part);
  };
  const string = (text: string) => {
    let length = 2;
    for (let index = 0; index < text.length; index++) {
      const code = text.charCodeAt(index);
      if (code === 34 || code === 92 || [8, 9, 10, 12, 13].includes(code)) length += 2;
      else if (code < 32) length += 6;
      else if (code < 0x80) length++;
      else if (code < 0x800) length += 2;
      else if (code >= 0xd800 && code <= 0xdbff && text.charCodeAt(index + 1) >= 0xdc00 && text.charCodeAt(index + 1) <= 0xdfff) { length += 4; index++; }
      else length += code >= 0xd800 && code <= 0xdfff ? 6 : 3;
      if (length > cap - bytes) throw new RunLimitError("hostResponse", "host response exceeded limit");
    }
    append(JSON.stringify(text), length);
  };
  const write = (item: unknown, depth: number) => {
    if (depth > 256) throw new TypeError("JSON response exceeds maximum depth of 256");
    if (item === null) append("null");
    else if (typeof item === "string") string(item);
    else if (typeof item === "number" && Number.isFinite(item)) append(JSON.stringify(item));
    else if (typeof item === "boolean") append(String(item));
    else if (Array.isArray(item)) {
      append("[");
      item.forEach((entry, index) => { if (index) append(","); write(entry, depth + 1); });
      append("]");
    } else if (item !== null && typeof item === "object") {
      append("{");
      let first = true;
      for (const key in item) {
        if (!Object.hasOwn(item, key)) continue;
        const entry = record(item)[key];
        if (entry === undefined) continue;
        if (!first) append(",");
        first = false;
        string(key); append(":"); write(entry, depth + 1);
      }
      append("}");
    } else throw new TypeError("response contains a non-JSON value");
  };
  write(value, 0);
  return encoder.encode(parts.join(""));
}
