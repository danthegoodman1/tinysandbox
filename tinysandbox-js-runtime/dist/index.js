function record(value) {
    return value !== null && typeof value === "object" ? value : {};
}
function thenable(value) {
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
    "setTimeout", "clearTimeout", "setInterval", "clearInterval", "setImmediate", "clearImmediate",
]);
const ABI_VERSION = 15;
/* Host answer to a guest call: response staged now, or settled later. */
const HOST_INLINE = 0;
const HOST_DEFERRED = 1;
/* tinysandbox_run/tinysandbox_resolve/tinysandbox_wake returned with the guest
 * still alive. */
const SUSPENDED = -2147483648;
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
    "tinysandbox_resolve:function",
    "tinysandbox_timer_delay:function",
    "tinysandbox_wake:function",
].sort();
const encoder = new TextEncoder();
const decoder = new TextDecoder();
const fatalDecoder = new TextDecoder("utf-8", { fatal: true });
export const QUICKJS_INITIAL_MEMORY_BYTES = INITIAL_PAGES * PAGE_BYTES;
const VFS_ERRNOS = Object.freeze({
    EAGAIN: [-35, "resource temporarily unavailable"],
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
    code;
    constructor(code, message = code) {
        if (!Object.hasOwn(VFS_ERRNOS, code))
            throw new TypeError(`unsupported VFS errno '${String(code)}'`);
        super(message);
        this.name = "VfsError";
        this.code = code;
    }
}
class RunLimitError extends Error {
    kind;
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
    if (globals === undefined)
        return [];
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
    if (JSON.stringify(actualImports) !== JSON.stringify(REQUIRED_IMPORTS))
        throw new TypeError(`incompatible QuickJS wasm imports: ${actualImports.join(", ")}`);
    if (JSON.stringify(actualExports) !== JSON.stringify(REQUIRED_EXPORTS))
        throw new TypeError(`incompatible QuickJS wasm exports: ${actualExports.join(", ")}`);
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
    }
    catch (error) {
        throw new TypeError(`incompatible QuickJS wasm memory contract: ${error instanceof Error ? error.message : String(error)}`);
    }
    if (instance.exports.memory !== memory)
        throw new TypeError("incompatible QuickJS wasm: env.memory is not re-exported");
    for (const [name, arity] of [["tinysandbox_abi_version", 0], ["tinysandbox_alloc", 1], ["tinysandbox_free", 1], ["tinysandbox_run", 3], ["tinysandbox_resolve", 1], ["tinysandbox_timer_delay", 0], ["tinysandbox_wake", 0]]) {
        if (instance.exports[name].length !== arity)
            throw new TypeError(`incompatible QuickJS wasm export signature '${name}'`);
    }
    if (instance.exports.tinysandbox_abi_version() !== ABI_VERSION)
        throw new TypeError(`incompatible QuickJS wasm ABI version; expected ${ABI_VERSION}`);
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
/* Resolves at the deadline or when the caller aborts. The timer is created on
 * first use, so a run that never waits on the host creates none, and release
 * frees it so a finished run does not keep the event loop alive. */
function expiryTimer(deadline, signal) {
    let expiry;
    let release = () => { };
    return {
        wait() {
            expiry ??= new Promise((resolve) => {
                const timer = setTimeout(() => resolve(undefined), Math.max(0, deadline - monotonicNow()));
                const onAborted = () => resolve(undefined);
                signal?.addEventListener("abort", onAborted, { once: true });
                if (signal?.aborted)
                    resolve(undefined);
                release = () => {
                    clearTimeout(timer);
                    signal?.removeEventListener("abort", onAborted);
                    resolve(undefined);
                };
            });
            return expiry;
        },
        release: () => release(),
    };
}
function jsonError(error) {
    const message = error instanceof Error ? error.message : String(error);
    const code = error && typeof error === "object" && typeof record(error).code === "string" ? record(error).code : undefined;
    return code === undefined ? { message } : { message, code };
}
function assertJsonValue(value, path = "value", seen = new Set()) {
    if (value === null || typeof value === "string" || typeof value === "boolean")
        return;
    if (typeof value === "number") {
        if (Number.isFinite(value))
            return;
        throw new TypeError(`${path} must contain only finite JSON numbers`);
    }
    if (typeof value !== "object")
        throw new TypeError(`${path} must contain only JSON-safe values`);
    if (seen.has(value))
        throw new TypeError(`${path} must not contain cycles`);
    const prototype = Object.getPrototypeOf(value);
    if (!Array.isArray(value) && prototype !== Object.prototype && prototype !== null) {
        throw new TypeError(`${path} must contain only plain objects and arrays`);
    }
    if (Object.getOwnPropertySymbols(value).length !== 0)
        throw new TypeError(`${path} must not contain symbol keys`);
    seen.add(value);
    if (Array.isArray(value)) {
        for (let index = 0; index < value.length; index++) {
            if (!Object.hasOwn(value, index))
                throw new TypeError(`${path} must not contain array holes`);
            assertJsonValue(value[index], `${path}[${index}]`, seen);
        }
    }
    else {
        for (const key of Object.keys(value))
            assertJsonValue(record(value)[key], `${path}.${key}`, seen);
    }
    seen.delete(value);
}
class HostCallFailure extends Error {
    payload;
    /** The synchronous API stopped at a Vfs promise, and that call is still running. */
    wouldBlock;
    constructor(payload, wouldBlock = false) {
        super(payload.message);
        this.payload = payload;
        this.wouldBlock = wouldBlock;
    }
}
function vfsFailure(error, syscall, path) {
    const candidate = record(error).code;
    const code = typeof candidate === "string" && Object.hasOwn(VFS_ERRNOS, candidate) ? candidate : "EIO";
    const [errno, message] = VFS_ERRNOS[code];
    return new HostCallFailure({ code, errno, message, syscall, ...(path === undefined ? {} : { path }) });
}
function invalidVfsCall(syscall, path) {
    return vfsFailure(new VfsError("EINVAL"), syscall, path);
}
function hostFailure(error) {
    return error instanceof HostCallFailure ? error.payload : jsonError(error);
}
const VFS_METHODS = ["stat", "readdir", "mkdir", "rename", "unlink", "rmdir", "open", "readAt", "writeAt", "truncate", "close"];
function validateVfs(vfs) {
    if (vfs === null || typeof vfs !== "object")
        throw new TypeError("vfs must be an object implementing the Vfs interface");
    for (const method of VFS_METHODS) {
        if (typeof record(vfs)[method] !== "function")
            throw new TypeError(`vfs.${method} must be a function`);
    }
    if (record(vfs).abort !== undefined && typeof record(vfs).abort !== "function")
        throw new TypeError("vfs.abort must be a function when supplied");
    return vfs;
}
function withContext(vfs, context) {
    const bind = (method) => (...args) => vfs[method].call(vfs, ...args, context);
    const bound = Object.fromEntries(VFS_METHODS.map((method) => [method, bind(method)]));
    if (vfs.abort !== undefined)
        bound.abort = bind("abort");
    return bound;
}
/* `refresh` lets polling observe a deadline or abort that no timer has
 * reported yet. */
function vfsContext(deadline, signal, refresh) {
    return Object.freeze({
        signal,
        deadlineMs: Date.now() + Math.max(0, deadline - monotonicNow()),
        remainingTimeMs: () => { refresh(); return signal.aborted ? 0 : Math.max(0, deadline - monotonicNow()); },
        isCancelled: () => { refresh(); return signal.aborted; },
    });
}
/* Thrown into an operation at a Vfs promise its caller cannot wait for. */
class WouldBlock extends Error {
}
function* callVfs(vfs, method, args, syscall, path) {
    try {
        const operation = vfs[method];
        const value = operation.apply(vfs, args);
        if (!thenable(value))
            return value;
        const release = method === "open" ? (handle) => discardHandle(vfs, handle) : undefined;
        return (yield { promise: value, detach: method === "close" || method === "abort", release });
    }
    catch (error) {
        if (error instanceof WouldBlock) {
            const [errno] = VFS_ERRNOS.EAGAIN;
            throw new HostCallFailure({ code: "EAGAIN", errno, message: "operation would block; use fs.promises", syscall, ...(path === undefined ? {} : { path }) }, true);
        }
        throw vfsFailure(error, syscall, path);
    }
}
function discardHandle(vfs, handle) {
    if (typeof handle === "number" && Number.isSafeInteger(handle) && handle >= 0)
        return (vfs.abort ?? vfs.close).call(vfs, handle);
}
/* Runs an operation to completion before returning. Each Vfs promise goes to
 * `settleLater`. A detached close or abort counts as done; any other call fails
 * with EAGAIN, and the operation's own cleanup still runs. */
function runInline(operation, settleLater) {
    let step = operation.next();
    while (!step.done) {
        settleLater(step.value);
        step = step.value.detach ? operation.next(undefined) : operation.throw(new WouldBlock());
    }
    return step.value;
}
/* Runs an operation inline until its Vfs first returns a promise, then
 * continues asynchronously, so a Vfs that answers directly never suspends the
 * guest. */
function runAwaitable(operation) {
    const first = operation.next();
    if (first.done)
        return first.value;
    return (async () => {
        let step = first;
        while (!step.done) {
            let value;
            try {
                value = await step.value.promise;
            }
            catch (error) {
                step = operation.throw(error);
                continue;
            }
            step = operation.next(value);
        }
        return step.value;
    })();
}
function normalizeAbsolute(path) {
    const parts = [];
    for (const part of String(path).split("/")) {
        if (part === "" || part === ".")
            continue;
        if (part === "..")
            parts.pop();
        else
            parts.push(part);
    }
    return parts.length === 0 ? "/" : `/${parts.join("/")}`;
}
function resolvePath(cwd, path) {
    return normalizeAbsolute(String(path).startsWith("/") ? path : cwd === "/" ? `/${path}` : `${cwd}/${path}`);
}
function checkedInteger(value, syscall, path, nullable = false) {
    if (nullable && (value === null || value === undefined))
        return null;
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0)
        throw invalidVfsCall(syscall, path);
    return value;
}
function checkedMetadata(value, syscall, path) {
    if (!value || typeof value !== "object" || !["file", "directory"].includes(value.fileType) || !Number.isSafeInteger(value.len) || value.len < 0) {
        throw vfsFailure(new Error("invalid VFS metadata"), syscall, path);
    }
    return value;
}
function encodeBase64(data) {
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
function decodeBase64(value, maximumBytes = Number.MAX_SAFE_INTEGER) {
    const input = String(value);
    if (input.length === 0)
        return new Uint8Array();
    if (input.length % 4 !== 0 || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(input)) {
        throw invalidVfsCall("tinysandbox");
    }
    const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const padding = input.endsWith("==") ? 2 : input.endsWith("=") ? 1 : 0;
    const length = input.length / 4 * 3 - padding;
    if (length > maximumBytes)
        throw vfsFailure(new VfsError("EFBIG"), "write");
    const output = new Uint8Array(length);
    let outputIndex = 0;
    for (let index = 0; index < input.length; index += 4) {
        const bits = (chars.indexOf(input[index]) << 18) | (chars.indexOf(input[index + 1]) << 12)
            | ((input[index + 2] === "=" ? 0 : chars.indexOf(input[index + 2])) << 6)
            | (input[index + 3] === "=" ? 0 : chars.indexOf(input[index + 3]));
        output[outputIndex++] = (bits >>> 16) & 255;
        if (outputIndex < output.length)
            output[outputIndex++] = (bits >>> 8) & 255;
        if (outputIndex < output.length)
            output[outputIndex++] = bits & 255;
    }
    return output;
}
function openMode(flags, path) {
    switch (flags) {
        case "r": return { read: true, write: false, create: false, createNew: false, truncate: false, append: false };
        case "r+": return { read: true, write: true, create: false, createNew: false, truncate: false, append: false };
        case "w": return { read: false, write: true, create: true, createNew: false, truncate: true, append: false };
        case "wx":
        case "xw": return { read: false, write: true, create: true, createNew: true, truncate: true, append: false };
        case "w+": return { read: true, write: true, create: true, createNew: false, truncate: true, append: false };
        case "wx+":
        case "w+x":
        case "xw+":
        case "x+w": return { read: true, write: true, create: true, createNew: true, truncate: true, append: false };
        case "a": return { read: false, write: true, create: true, createNew: false, truncate: false, append: true };
        case "ax":
        case "xa": return { read: false, write: true, create: true, createNew: true, truncate: false, append: true };
        case "a+": return { read: true, write: true, create: true, createNew: false, truncate: false, append: true };
        case "ax+":
        case "a+x":
        case "xa+":
        case "x+a": return { read: true, write: true, create: true, createNew: true, truncate: false, append: true };
        default: throw invalidVfsCall("open", path);
    }
}
function* openVfs(vfs, path, mode) {
    const handle = yield* callVfs(vfs, "open", [path, mode], "open", path);
    if (!Number.isSafeInteger(handle) || handle < 0) {
        try {
            yield* callVfs(vfs, "close", [handle], "close");
        }
        catch { }
        throw vfsFailure(new Error("invalid VFS handle"), "open", path);
    }
    return handle;
}
function* readAll(vfs, path, maximumBytes, checkpoint = () => { }) {
    checkpoint();
    const handle = yield* openVfs(vfs, path, openMode("r", path));
    const chunks = [];
    let length = 0;
    let failure;
    try {
        for (;;) {
            checkpoint();
            const buffer = new Uint8Array(Math.min(8192, maximumBytes - Math.min(length, maximumBytes) + 1));
            const count = yield* callVfs(vfs, "readAt", [handle, length, buffer], "read");
            checkpoint();
            if (!Number.isSafeInteger(count) || count < 0 || count > buffer.byteLength)
                throw vfsFailure(new Error("invalid VFS read count"), "read");
            if (count === 0)
                break;
            chunks.push(buffer.slice(0, count));
            length += count;
            if (length > maximumBytes)
                throw new RangeError(`source exceeded limit of ${maximumBytes} bytes`);
        }
    }
    catch (error) {
        failure = error;
    }
    try {
        yield* callVfs(vfs, "close", [handle], "close");
    }
    catch (error) {
        if (failure === undefined)
            failure = error;
    }
    if (failure !== undefined)
        throw failure;
    return join(chunks, length);
}
function* writeAll(vfs, path, data, append, checkpoint) {
    checkpoint();
    const mode = openMode(append ? "a" : "w", path);
    const handle = yield* openVfs(vfs, path, mode);
    let written = 0;
    let failure;
    try {
        while (written < data.byteLength) {
            checkpoint();
            const count = yield* callVfs(vfs, "writeAt", [handle, written, data.subarray(written)], "write");
            if (!Number.isSafeInteger(count) || count < 0 || count > data.byteLength - written)
                throw vfsFailure(new Error("invalid VFS write count"), "write");
            if (count === 0)
                throw vfsFailure(new VfsError("ENOSPC"), "write");
            written += count;
        }
    }
    catch (error) {
        failure = error;
    }
    try {
        yield* callVfs(vfs, failure !== undefined && vfs.abort ? "abort" : "close", [handle], "close");
    }
    catch (error) {
        if (failure === undefined)
            failure = error;
    }
    if (failure !== undefined)
        throw failure;
}
function createVfsDispatcher(vfs, cwd, hostInputBytes, hostResponseBytes, maxOpenFiles, checkpoint) {
    const files = new Map();
    let nextFd = 3;
    let opening = 0;
    // Vfs work the run started and has not seen settle, and the first failure
    // among the closes it settles on the guest's behalf.
    const inflight = new Set();
    let failure;
    const track = (work) => {
        const settled = Promise.resolve(work).then(() => { }, () => { }).then(() => { inflight.delete(settled); });
        inflight.add(settled);
    };
    const settleLater = ({ promise, detach, release }) => {
        if (detach)
            track(Promise.resolve(promise).catch((error) => { failure ??= vfsFailure(error, "close"); }));
        else
            track(release === undefined ? promise : Promise.resolve(promise).then(release));
    };
    const absolute = (value) => {
        if (typeof value !== "string")
            throw invalidVfsCall("tinysandbox");
        const path = resolvePath(cwd, value);
        if (path.includes("\0") || path.split("/").length > 257)
            throw invalidVfsCall("tinysandbox", path);
        return path;
    };
    const readBytes = Math.min(hostInputBytes, Math.max(0, Math.floor((hostResponseBytes - 128) / 4) * 3));
    const readFile = function* (path, cap) {
        try {
            return yield* readAll(vfs, path, cap, checkpoint);
        }
        catch (error) {
            if (error instanceof RangeError)
                throw vfsFailure(new VfsError("EFBIG"), "open", path);
            throw error;
        }
    };
    const metadata = function* (path, syscall = "stat") {
        return checkedMetadata(yield* callVfs(vfs, "stat", [path], syscall, path), syscall, path);
    };
    // Reads reuse a spare buffer. The guest picks the length, so allocating and
    // zeroing one per call let a script spend host time reading a 1-byte file
    // with megabyte requests. A read waiting on the Vfs keeps its buffer, and one
    // the synchronous API gave up on never returns it, so only overlapping reads
    // allocate.
    let spare;
    const takeBuffer = (length) => {
        const buffer = spare !== undefined && spare.byteLength >= length ? spare : new Uint8Array(length);
        if (buffer === spare)
            spare = undefined;
        return buffer;
    };
    const returnBuffer = (buffer) => {
        if (spare === undefined || buffer.byteLength > spare.byteLength)
            spare = buffer;
    };
    const mkdirRecursive = function* (path) {
        checkpoint();
        if (path === "/")
            return;
        let current = "";
        for (const part of path.slice(1).split("/")) {
            checkpoint();
            current += `/${part}`;
            try {
                yield* callVfs(vfs, "mkdir", [current], "mkdir", path);
            }
            catch (error) {
                if (!(error instanceof HostCallFailure) || error.payload.code !== "EEXIST" || (yield* metadata(current)).fileType !== "directory")
                    throw error;
            }
        }
    };
    const removePath = function* (path, recursive) {
        checkpoint();
        if (path.split("/").length > 257)
            throw invalidVfsCall("rm", path);
        const info = yield* metadata(path, "rm");
        if (info.fileType === "file")
            return yield* callVfs(vfs, "unlink", [path], "unlink", path);
        if (!recursive)
            throw vfsFailure(new VfsError("EISDIR"), "rm", path);
        const entries = yield* callVfs(vfs, "readdir", [path], "scandir", path);
        if (!Array.isArray(entries))
            throw vfsFailure(new Error("invalid VFS directory entries"), "scandir", path);
        for (const entry of entries) {
            if (!entry || typeof entry.name !== "string" || entry.name.includes("/"))
                throw vfsFailure(new Error("invalid VFS directory entry"), "scandir", path);
            yield* removePath(path === "/" ? `/${entry.name}` : `${path}/${entry.name}`, true);
        }
        return yield* callVfs(vfs, "rmdir", [path], "rmdir", path);
    };
    const dispatch = function* (op, args) {
        checkpoint();
        switch (op) {
            case "readFile": {
                const path = absolute(args?.path);
                return encodeBase64(yield* remapHostFailure(() => readFile(path, readBytes), "open", path));
            }
            case "writeFile": {
                const path = absolute(args?.path);
                yield* remapHostFailure(() => writeAll(vfs, path, decodeBase64(args?.data, hostInputBytes), false, checkpoint), "open", path);
                return null;
            }
            case "appendFile": {
                const path = absolute(args?.path);
                yield* remapHostFailure(() => writeAll(vfs, path, decodeBase64(args?.data, hostInputBytes), true, checkpoint), "open", path);
                return null;
            }
            case "mkdir": {
                const path = absolute(args?.path);
                if (args?.recursive)
                    yield* mkdirRecursive(path);
                else
                    yield* callVfs(vfs, "mkdir", [path], "mkdir", path);
                return null;
            }
            case "readdir": {
                const path = absolute(args?.path);
                const entries = yield* callVfs(vfs, "readdir", [path], "scandir", path);
                if (!Array.isArray(entries))
                    throw vfsFailure(new Error("invalid VFS directory entries"), "scandir", path);
                return entries.map((entry) => {
                    if (!entry || typeof entry.name !== "string" || entry.name.includes("/"))
                        throw vfsFailure(new Error("invalid VFS directory entry"), "scandir", path);
                    if (!args?.withFileTypes)
                        return entry.name;
                    const info = checkedMetadata(entry.metadata, "scandir", path);
                    return { name: entry.name, isFile: info.fileType === "file", isDirectory: info.fileType === "directory" };
                });
            }
            case "stat": {
                const info = yield* metadata(absolute(args?.path));
                return { size: info.len, isFile: info.fileType === "file", isDirectory: info.fileType === "directory" };
            }
            case "rename": {
                const from = absolute(args?.from);
                const to = absolute(args?.to);
                yield* callVfs(vfs, "rename", [from, to], "rename", from);
                return null;
            }
            case "rm": {
                const path = absolute(args?.path);
                try {
                    yield* removePath(path, !!args?.recursive);
                }
                catch (error) {
                    if (!(args?.force && error instanceof HostCallFailure && error.payload.code === "ENOENT"))
                        throw error;
                }
                return null;
            }
            case "unlink": {
                const path = absolute(args?.path);
                yield* callVfs(vfs, "unlink", [path], "unlink", path);
                return null;
            }
            case "rmdir": {
                const path = absolute(args?.path);
                if (args?.recursive)
                    yield* removePath(path, true);
                else
                    yield* callVfs(vfs, "rmdir", [path], "rmdir", path);
                return null;
            }
            case "exists": {
                try {
                    yield* metadata(absolute(args?.path));
                    return true;
                }
                // A stat the synchronous API could not wait for says nothing either way.
                catch (error) {
                    if (error instanceof HostCallFailure && error.wouldBlock)
                        throw error;
                    return false;
                }
            }
            case "open": {
                if (files.size + opening >= maxOpenFiles || nextFd >= Number.MAX_SAFE_INTEGER)
                    throw vfsFailure(new VfsError("ENOSPC"), "open");
                const path = absolute(args?.path);
                const mode = openMode(args?.flags, path);
                opening++;
                let handle;
                try {
                    handle = yield* openVfs(vfs, path, mode);
                }
                finally {
                    opening--;
                }
                try {
                    // An awaited open can finish after the run has ended.
                    checkpoint();
                }
                catch (error) {
                    try {
                        yield* callVfs(vfs, vfs.abort ? "abort" : "close", [handle], "close");
                    }
                    catch { }
                    throw error;
                }
                const fd = nextFd++;
                files.set(fd, { handle, position: 0 });
                return fd;
            }
            case "read": {
                const fd = checkedInteger(args?.fd, "tinysandbox");
                const file = files.get(fd);
                if (!file)
                    throw vfsFailure(new VfsError("EBADF"), "read");
                const position = checkedInteger(args?.position, "tinysandbox", undefined, true);
                const offset = position ?? file.position;
                const length = Math.min(MAX_HOST_READ_BYTES, readBytes, checkedInteger(args?.length, "tinysandbox"));
                const buffer = takeBuffer(length);
                const count = yield* callVfs(vfs, "readAt", [file.handle, offset, buffer.subarray(0, length)], "read");
                if (!Number.isSafeInteger(count) || count < 0 || count > length)
                    throw vfsFailure(new Error("invalid VFS read count"), "read");
                if (position === null)
                    file.position += count;
                const data = encodeBase64(buffer.subarray(0, count));
                returnBuffer(buffer);
                return { bytesRead: count, data };
            }
            case "write": {
                const fd = checkedInteger(args?.fd, "tinysandbox");
                const file = files.get(fd);
                if (!file)
                    throw vfsFailure(new VfsError("EBADF"), "write");
                const position = checkedInteger(args?.position, "tinysandbox", undefined, true);
                const offset = position ?? file.position;
                const data = decodeBase64(args?.data, hostInputBytes);
                const count = yield* callVfs(vfs, "writeAt", [file.handle, offset, data], "write");
                if (!Number.isSafeInteger(count) || count < 0 || count > data.byteLength)
                    throw vfsFailure(new Error("invalid VFS write count"), "write");
                if (position === null)
                    file.position += count;
                return count;
            }
            case "ftruncate": {
                const fd = checkedInteger(args?.fd, "tinysandbox");
                const file = files.get(fd);
                if (!file)
                    throw vfsFailure(new VfsError("EBADF"), "ftruncate");
                yield* callVfs(vfs, "truncate", [file.handle, checkedInteger(args?.len ?? 0, "tinysandbox")], "ftruncate");
                return null;
            }
            case "close": {
                const fd = checkedInteger(args?.fd, "tinysandbox");
                const file = files.get(fd);
                if (!file)
                    throw vfsFailure(new VfsError("EBADF"), "close");
                // close() consumes the descriptor even when it fails, and the Vfs may
                // still be closing when the run ends, so the entry goes first. A failed
                // close discards whatever it staged instead.
                files.delete(fd);
                try {
                    yield* callVfs(vfs, "close", [file.handle], "close");
                }
                catch (error) {
                    if (vfs.abort) {
                        try {
                            yield* callVfs(vfs, "abort", [file.handle], "close");
                        }
                        catch { }
                    }
                    throw error;
                }
                return null;
            }
            case "copyFile": {
                const source = absolute(args?.src);
                const destination = absolute(args?.dest);
                const data = yield* remapHostFailure(() => readFile(source, hostInputBytes), "copyfile", source);
                yield* remapHostFailure(() => writeAll(vfs, destination, data, false, checkpoint), "copyfile", destination);
                return null;
            }
            default: throw invalidVfsCall("tinysandbox");
        }
    };
    // For a guest that needs the answer before the call returns.
    const dispatchInline = (op, args) => runInline(dispatch(op, args), settleLater);
    // For a guest that awaits the answer.
    const dispatchAwaitable = (op, args) => {
        const answer = runAwaitable(dispatch(op, args));
        if (answer instanceof Promise)
            track(answer);
        return answer;
    };
    const settle = async (expiry) => {
        while (inflight.size > 0) {
            if (!await Promise.race([Promise.all(inflight).then(() => true), expiry().then(() => false)]))
                return false;
        }
        return true;
    };
    // Ends the run's filesystem work. Calls still running settle first; now that
    // the run is over they stop at their next checkpoint. Remaining descriptors
    // then close, or abort when the run was cut short. Waiting stops at expiry.
    const finish = async (commit, expiry) => {
        let expired = !await settle(expiry);
        for (const { handle } of files.values()) {
            try {
                const closing = runAwaitable(callVfs(vfs, !commit && vfs.abort ? "abort" : "close", [handle], "close"));
                if (closing instanceof Promise)
                    track(closing.catch((error) => { failure ??= error; }));
            }
            catch (error) {
                failure ??= error;
            }
        }
        files.clear();
        if (!await settle(expiry))
            expired = true;
        return { failure, expired };
    };
    return { dispatchInline, dispatchAwaitable, finish };
}
function exposedHostError(payload) {
    const where = payload.path === undefined ? (payload.syscall ? `, ${payload.syscall}` : "") : `, ${payload.syscall} '${payload.path}'`;
    const error = new Error(`${payload.code}: ${payload.message}${where}`);
    error.code = payload.code;
    error.errno = payload.errno;
    error.syscall = payload.syscall;
    if (payload.path !== undefined)
        error.path = payload.path;
    return error;
}
function* remapHostFailure(operation, syscall, path) {
    try {
        return yield* operation();
    }
    catch (error) {
        if (!(error instanceof HostCallFailure))
            throw error;
        const { code, errno, message } = error.payload;
        throw new HostCallFailure({ code, errno, message, syscall, ...(path === undefined ? {} : { path }) }, error.wouldBlock);
    }
}
const TIMER_DUE = Symbol("timer due");
/* Resolves when the guest's next timer is due; cancel frees the host timer. */
function timerWait(delay) {
    let timer;
    const due = new Promise((resolve) => { timer = setTimeout(() => resolve(TIMER_DUE), delay); });
    return { due, cancel: () => clearTimeout(timer) };
}
export async function createEngine(wasm) {
    let module;
    if (wasm instanceof WebAssembly.Module) {
        module = wasm;
    }
    else if (ArrayBuffer.isView(wasm) || wasm instanceof ArrayBuffer) {
        module = await WebAssembly.compile(wasm);
    }
    else {
        throw new TypeError("createEngine expects wasm bytes or a WebAssembly.Module");
    }
    requireArtifactAbi(module);
    probeArtifactAbi(module);
    const engine = {
        async runCode(code, options = {}) {
            if (typeof code !== "string")
                throw new TypeError("code must be a string");
            if (options === null || typeof options !== "object")
                throw new TypeError("options must be an object");
            const wasmMemoryBytes = integerOption("wasmMemoryBytes", options.wasmMemoryBytes, DEFAULT_WASM_MEMORY_BYTES);
            const maximumPages = Math.min(65_536, Math.floor(wasmMemoryBytes / PAGE_BYTES));
            if (maximumPages < INITIAL_PAGES) {
                throw new RangeError(`wasmMemoryBytes must be at least ${QUICKJS_INITIAL_MEMORY_BYTES} bytes`);
            }
            const quickjsHeapBytes = integerOption("quickjsHeapBytes", options.quickjsHeapBytes, DEFAULT_QUICKJS_HEAP_BYTES, 1);
            if (quickjsHeapBytes > 0x7fff_ffff)
                throw new RangeError("quickjsHeapBytes must be at most 2147483647");
            const timeoutMs = integerOption("timeoutMs", options.timeoutMs, DEFAULT_TIMEOUT_MS, 1);
            const sourceBytes = integerOption("sourceBytes", options.sourceBytes, DEFAULT_SOURCE_BYTES);
            const hostResponseBytes = integerOption("hostResponseBytes", options.hostResponseBytes, DEFAULT_HOST_RESPONSE_BYTES);
            const hostInputBytes = integerOption("hostInputBytes", options.hostInputBytes, DEFAULT_HOST_INPUT_BYTES);
            const maxOpenFiles = integerOption("maxOpenFiles", options.maxOpenFiles, DEFAULT_MAX_OPEN_FILES);
            const stdoutBytes = integerOption("stdoutBytes", options.stdoutBytes, DEFAULT_OUTPUT_BYTES);
            const stderrBytes = integerOption("stderrBytes", options.stderrBytes, DEFAULT_OUTPUT_BYTES);
            const sourceLength = utf8Length(code, sourceBytes);
            if (sourceLength > sourceBytes)
                throw new RangeError(`source exceeded limit of ${sourceBytes} bytes`);
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
            // Vfs calls keep their signal through teardown, so commits can finish
            // after the guest does. It aborts once the run stops waiting on them.
            const vfsController = new AbortController();
            const stopVfs = () => {
                if (!vfsController.signal.aborted)
                    vfsController.abort(signal?.reason ?? new DOMException("Execution ended", "AbortError"));
            };
            const memory = new WebAssembly.Memory({ initial: INITIAL_PAGES, maximum: maximumPages });
            const stdout = [];
            const stderr = [];
            let stdoutLength = 0;
            let stderrLength = 0;
            let response = new Uint8Array();
            // Host calls the guest is awaiting, keyed by the slot the guest reserved.
            const deferred = new Map();
            let timedOut = false;
            let finished = false;
            let limitFailure;
            const isCancelled = () => {
                if (signal?.aborted || monotonicNow() >= deadline)
                    timedOut = true;
                if ((finished || timedOut) && !controller.signal.aborted) {
                    controller.abort(signal?.reason ?? new DOMException("Execution ended", "AbortError"));
                }
                if (timedOut)
                    stopVfs();
                return finished || timedOut;
            };
            const deadlineMs = Date.now() + Math.max(0, deadline - monotonicNow());
            const callbackContext = () => {
                const callback = new AbortController();
                const abort = () => { callback.abort(controller.signal.reason); };
                const cancelled = () => {
                    if (isCancelled())
                        abort();
                    return callback.signal.aborted;
                };
                controller.signal.addEventListener("abort", abort, { once: true });
                const context = Object.freeze({
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
            // One timer for the whole run, so a guest making many host calls does not
            // accumulate timers.
            const expiry = expiryTimer(deadline, signal);
            const checkpoint = () => {
                if (isCancelled())
                    throw new RunLimitError("timeout", cancellationMessage());
            };
            const vfsDispatcher = vfs === undefined ? undefined : createVfsDispatcher(withContext(vfs, vfsContext(deadline, vfsController.signal, isCancelled)), resolvePath("/", options.cwd ?? "/"), hostInputBytes, hostResponseBytes, maxOpenFiles, checkpoint);
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
                if (stream === "stdout")
                    stdoutLength += length;
                else
                    stderrLength += length;
                return length;
            };
            const setResponse = (value) => {
                let bytes;
                try {
                    bytes = boundedJson(value, hostResponseBytes);
                }
                catch (error) {
                    try {
                        bytes = boundedJson({ error: { code: "E2BIG", message: `host response exceeded limit of ${hostResponseBytes} bytes` } }, hostResponseBytes);
                    }
                    catch {
                        limitFailure = new RunLimitError("hostResponse", `host response exceeded limit of ${hostResponseBytes} bytes`);
                        throw limitFailure;
                    }
                }
                // A caller that caught the throw above and sent something that fits has
                // recovered: the guest gets an answer and keeps running, so the run is
                // no longer failing on this limit and its writes still deserve to commit.
                if (limitFailure?.kind === "hostResponse")
                    limitFailure = undefined;
                response = bytes;
            };
            const tinysandbox = {
                should_interrupt() {
                    return isCancelled() ? 1 : 0;
                },
                host_call(opPointer, opLength, jsonPointer, jsonLength, callId) {
                    checkpoint();
                    const op = decoder.decode(read(opPointer, opLength));
                    let argument;
                    try {
                        argument = record(JSON.parse(decoder.decode(read(jsonPointer, jsonLength))));
                    }
                    catch (error) {
                        setResponse({ error: { code: "EINVAL", message: `invalid host call JSON: ${error instanceof Error ? error.message : String(error)}` } });
                        return 0;
                    }
                    if (op !== "global" && (!VFS_OPERATIONS.has(op) || vfsDispatcher === undefined)) {
                        setResponse({ error: { code: "ENOSYS", message: `host capability '${op}' is not available` } });
                        return 0;
                    }
                    if (op !== "global") {
                        let answer;
                        try {
                            // Without a slot the guest needs the answer before this returns:
                            // the synchronous fs API, or fs.promises with every slot taken.
                            answer = callId < 0 ? vfsDispatcher.dispatchInline(op, argument) : vfsDispatcher.dispatchAwaitable(op, argument);
                        }
                        catch (error) {
                            setResponse({ error: hostFailure(error) });
                            return HOST_INLINE;
                        }
                        if (answer instanceof Promise) {
                            deferred.set(callId, answer.then((value) => ({ callId, response: { value } }), (error) => ({ callId, response: { error: hostFailure(error) } })));
                            checkpoint();
                            return HOST_DEFERRED;
                        }
                        try {
                            checkpoint();
                            setResponse({ value: answer });
                        }
                        catch (error) {
                            setResponse({ error: hostFailure(error) });
                        }
                        return HOST_INLINE;
                    }
                    const name = argument?.name;
                    const handler = typeof name === "string" ? globals[name] : undefined;
                    if (typeof handler !== "function") {
                        setResponse({ error: { message: `unknown global '${String(name)}'` } });
                        return 0;
                    }
                    // Reserve before invoking even a synchronous global: a callback may
                    // start work or return a rejected promise before we can inspect it.
                    if (callId < 0) {
                        setResponse({ error: { message: `global '${String(name)}' exceeded the concurrent host call limit` } });
                        return HOST_INLINE;
                    }
                    const host = callbackContext();
                    let value;
                    try {
                        const payload = argument.args ?? null;
                        assertJsonValue(payload);
                        value = handler(payload, host.context);
                    }
                    catch (error) {
                        host.dispose();
                        setResponse({ error: jsonError(error) });
                        return HOST_INLINE;
                    }
                    if (thenable(value)) {
                        // Defer: the guest gets a promise and parks, this call returns, and
                        // the wasm stack unwinds so the driver can reach the event loop.
                        deferred.set(callId, Promise.resolve(value).then((resolved) => {
                            assertJsonValue(resolved, `global '${String(name)}' response`);
                            return { callId, response: { value: resolved } };
                        }).catch((error) => ({ callId, response: { error: jsonError(error) } }))
                            .then((settled) => { host.dispose(); return settled; }));
                        // Own the promise before a timeout/abort can throw. Its settlement
                        // remains observed even when the guest never consumes the answer.
                        checkpoint();
                        return HOST_DEFERRED;
                    }
                    try {
                        checkpoint();
                        assertJsonValue(value, `global '${String(name)}' response`);
                        setResponse({ value });
                    }
                    catch (error) {
                        setResponse({ error: jsonError(error) });
                    }
                    finally {
                        host.dispose();
                    }
                    return HOST_INLINE;
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
                    const nanos = clockId === 1 ? BigInt(Math.floor((monotonicNow() - started) * 1_000_000)) : BigInt(Date.now()) * 1000000n;
                    new DataView(memory.buffer).setBigUint64(pointer, nanos, true);
                    return 0;
                },
                fd_close() { return 0; },
                fd_fdstat_get() { return 8; },
                fd_seek() { return 8; },
                fd_write(fd, iovs, iovsLength, writtenPointer) {
                    if (fd !== 1 && fd !== 2)
                        return 8;
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
            let cleanupFailure;
            let cleanupExpired = false;
            let completed = false;
            try {
                signal?.addEventListener("abort", onAbort, { once: true });
                checkpoint();
                instance = new WebAssembly.Instance(module, { env: { memory }, tinysandbox, wasi_snapshot_preview1: wasi });
                if (instance.exports.memory !== memory)
                    throw new TypeError("QuickJS wasm did not re-export env.memory");
                if (typeof instance.exports._initialize === "function")
                    instance.exports._initialize();
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
                const pointer = instance.exports.tinysandbox_alloc(configBytes.byteLength);
                if (!pointer)
                    throw new WebAssembly.RuntimeError("QuickJS input allocation failed");
                read(pointer, configBytes.byteLength).set(configBytes);
                exitCode = instance.exports.tinysandbox_run(pointer, configBytes.byteLength, quickjsHeapBytes);
                while (exitCode === SUSPENDED) {
                    isCancelled();
                    if (timedOut)
                        break;
                    // The guest waits on its outstanding host calls and, when it has
                    // one, its next timer.
                    const delay = instance.exports.tinysandbox_timer_delay();
                    if (deferred.size === 0 && delay < 0)
                        throw new Error("guest suspended with nothing to wait on");
                    // Settle whichever finishes first so concurrent awaits in the guest
                    // progress in completion order rather than call order. A host promise
                    // that never settles must not outlive the deadline, so the wait races
                    // a timer: suspension is the one point where no guest checkpoint can
                    // observe the clock.
                    const wake = delay < 0 ? undefined : timerWait(delay);
                    const settled = await Promise.race(wake === undefined ? [...deferred.values(), expiry.wait()] : [...deferred.values(), expiry.wait(), wake.due]);
                    wake?.cancel();
                    if (settled === undefined) {
                        timedOut = true;
                        break;
                    }
                    if (settled === TIMER_DUE) {
                        checkpoint();
                        exitCode = instance.exports.tinysandbox_wake();
                        continue;
                    }
                    deferred.delete(settled.callId);
                    checkpoint();
                    setResponse(settled.response);
                    exitCode = instance.exports.tinysandbox_resolve(settled.callId);
                }
                isCancelled();
                if (timedOut) {
                    return { exitCode: 124, stdout: "", stderr: `js: ${cancellationMessage()}\n`, initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: memory.buffer.byteLength };
                }
                instance.exports.tinysandbox_free(pointer);
                completed = true;
            }
            catch (error) {
                if (timedOut) {
                    return { exitCode: 124, stdout: "", stderr: `js: ${cancellationMessage()}\n`, initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: memory.buffer.byteLength };
                }
                const isMemory = error instanceof WebAssembly.RuntimeError && /memory|allocation|out of bounds/i.test(error.message);
                const message = limitFailure?.message ?? (isMemory ? "wasm memory limit exceeded" : `runtime trap: ${error instanceof Error ? error.message : String(error)}`);
                return { exitCode: 1, stdout: decoder.decode(join(stdout, stdoutLength)), stderr: `js: ${message}\n`, initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: memory.buffer.byteLength };
            }
            finally {
                finished = true;
                signal?.removeEventListener("abort", onAbort);
                isCancelled();
                // Commit whenever the guest reached its own end. The exit status is the
                // script's to choose, and a descriptor it closed itself already
                // committed regardless of it; keying durability on the status instead
                // would split one run's writes between committed and discarded.
                // Rollback belongs to the runs the host cut short. Like a host promise
                // the guest awaits, Vfs work is not waited on past the deadline.
                if (vfsDispatcher !== undefined) {
                    const settled = await vfsDispatcher.finish(completed && !timedOut && limitFailure === undefined, expiry.wait);
                    cleanupFailure = settled.failure;
                    cleanupExpired = settled.expired;
                }
                stopVfs();
                expiry.release();
            }
            if (cleanupExpired) {
                return { exitCode: 124, stdout: "", stderr: `js: ${cancellationMessage()}\n`, initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: memory.buffer.byteLength };
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
        async runFile(path, options) {
            if (typeof path !== "string")
                throw new TypeError("path must be a string");
            if (options === null || typeof options !== "object")
                throw new TypeError("options must be an object");
            const deadline = monotonicNow() + integerOption("timeoutMs", options.timeoutMs, DEFAULT_TIMEOUT_MS, 1);
            const checkpoint = () => {
                if (options.signal?.aborted)
                    throw new RunLimitError("timeout", "command cancelled");
                if (monotonicNow() >= deadline)
                    throw new RunLimitError("timeout", "command timed out");
            };
            const vfs = validateVfs(options.vfs);
            const cwd = resolvePath("/", options.cwd ?? "/");
            const resolved = resolvePath(cwd, path);
            const sourceBytes = integerOption("sourceBytes", options.sourceBytes, DEFAULT_SOURCE_BYTES);
            const sourceCap = Math.min(sourceBytes, integerOption("hostInputBytes", options.hostInputBytes, DEFAULT_HOST_INPUT_BYTES));
            const reading = new AbortController();
            const stopReading = () => {
                if (!reading.signal.aborted)
                    reading.abort(options.signal?.reason ?? new DOMException("Execution ended", "AbortError"));
            };
            const context = vfsContext(deadline, reading.signal, () => { if (options.signal?.aborted || monotonicNow() >= deadline)
                stopReading(); });
            let bytes;
            try {
                bytes = runAwaitable(remapHostFailure(() => readAll(withContext(vfs, context), resolved, sourceCap, checkpoint), "open", resolved));
                if (bytes instanceof Promise) {
                    // The read keeps running if the deadline wins; its next checkpoint
                    // stops it and closes the entry.
                    const expiry = expiryTimer(deadline, options.signal);
                    try {
                        bytes = await Promise.race([bytes, expiry.wait()]);
                    }
                    finally {
                        expiry.release();
                    }
                    if (bytes === undefined)
                        throw new RunLimitError("timeout", options.signal?.aborted ? "command cancelled" : "command timed out");
                }
            }
            catch (error) {
                if (error instanceof RunLimitError && error.kind === "timeout")
                    return { exitCode: 124, stdout: "", stderr: `js: ${error.message}\n`, initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: 0 };
                if (error instanceof HostCallFailure)
                    throw exposedHostError(error.payload);
                throw error;
            }
            finally {
                stopReading();
            }
            let code;
            try {
                code = fatalDecoder.decode(bytes);
            }
            catch {
                throw new TypeError(`runFile entry '${resolved}' is not valid UTF-8`);
            }
            if (monotonicNow() >= deadline)
                return { exitCode: 124, stdout: "", stderr: "js: command timed out\n", initialWasmMemoryBytes: QUICKJS_INITIAL_MEMORY_BYTES, peakWasmMemoryBytes: 0 };
            return await engine.runCode(code, {
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
function join(chunks, length) {
    const result = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) {
        result.set(chunk, offset);
        offset += chunk.byteLength;
    }
    return result;
}
// Count UTF-8 without allocating a temporary encoding; stop once the cap fails.
function utf8Length(value, cap) {
    let bytes = 0;
    for (let index = 0; index < value.length; index++) {
        const code = value.charCodeAt(index);
        if (code < 0x80)
            bytes++;
        else if (code < 0x800)
            bytes += 2;
        else if (code >= 0xd800 && code <= 0xdbff && value.charCodeAt(index + 1) >= 0xdc00 && value.charCodeAt(index + 1) <= 0xdfff) {
            bytes += 4;
            index++;
        }
        else
            bytes += 3;
        if (bytes > cap)
            return bytes;
    }
    return bytes;
}
function boundedJson(value, cap) {
    const parts = [];
    let bytes = 0;
    const append = (part, length = part.length) => {
        if (length > cap - bytes)
            throw new RunLimitError("hostResponse", "host response exceeded limit");
        bytes += length;
        parts.push(part);
    };
    const string = (text) => {
        let length = 2;
        for (let index = 0; index < text.length; index++) {
            const code = text.charCodeAt(index);
            if (code === 34 || code === 92 || [8, 9, 10, 12, 13].includes(code))
                length += 2;
            else if (code < 32)
                length += 6;
            else if (code < 0x80)
                length++;
            else if (code < 0x800)
                length += 2;
            else if (code >= 0xd800 && code <= 0xdbff && text.charCodeAt(index + 1) >= 0xdc00 && text.charCodeAt(index + 1) <= 0xdfff) {
                length += 4;
                index++;
            }
            else
                length += code >= 0xd800 && code <= 0xdfff ? 6 : 3;
            if (length > cap - bytes)
                throw new RunLimitError("hostResponse", "host response exceeded limit");
        }
        append(JSON.stringify(text), length);
    };
    const write = (item, depth) => {
        if (depth > 256)
            throw new TypeError("JSON response exceeds maximum depth of 256");
        if (item === null)
            append("null");
        else if (typeof item === "string")
            string(item);
        else if (typeof item === "number" && Number.isFinite(item))
            append(JSON.stringify(item));
        else if (typeof item === "boolean")
            append(String(item));
        else if (Array.isArray(item)) {
            append("[");
            for (let index = 0; index < item.length; index++) {
                if (index)
                    append(",");
                // Match JSON.stringify: holes and undefined entries serialize as null.
                // Skipping them, as forEach does, shifts or corrupts the output.
                const entry = item[index];
                if (entry === undefined)
                    append("null");
                else
                    write(entry, depth + 1);
            }
            append("]");
        }
        else if (item !== null && typeof item === "object") {
            append("{");
            let first = true;
            for (const key in item) {
                if (!Object.hasOwn(item, key))
                    continue;
                const entry = record(item)[key];
                if (entry === undefined)
                    continue;
                if (!first)
                    append(",");
                first = false;
                string(key);
                append(":");
                write(entry, depth + 1);
            }
            append("}");
        }
        else
            throw new TypeError("response contains a non-JSON value");
    };
    write(value, 0);
    return encoder.encode(parts.join(""));
}
