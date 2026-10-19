export type JsonValue = null | boolean | number | string | JsonValue[] | {
    [key: string]: JsonValue;
};
export interface HostContext {
    /** Aborts on callback completion; cancellation is refreshed at checkpoints and by context polling. */
    readonly signal: AbortSignal;
    /** Approximate Unix timestamp; remainingTimeMs uses the monotonic deadline. */
    readonly deadlineMs: number;
    remainingTimeMs(): number;
    isCancelled(): boolean;
}
/** A global may answer immediately or return a promise the guest awaits. */
export type HostGlobal = (argument: JsonValue, context: HostContext) => JsonValue | Promise<JsonValue>;
export type VfsErrno = "EAGAIN" | "EBADF" | "EBUSY" | "EXDEV" | "EACCES" | "EEXIST" | "EFBIG" | "EIO" | "EINVAL" | "EISDIR" | "ENOENT" | "ENOSPC" | "ENOTDIR" | "ENOTEMPTY";
export interface VfsMetadata {
    fileType: "file" | "directory";
    len: number;
}
export interface VfsDirEntry {
    name: string;
    metadata: VfsMetadata;
}
export interface VfsOpenMode {
    read: boolean;
    write: boolean;
    create: boolean;
    createNew: boolean;
    truncate: boolean;
    append: boolean;
}
/**
 * Absolute-path, handle-and-offset filesystem capability. A method may answer
 * directly or return a promise. Guest `fs.promises` calls wait for a promise;
 * the synchronous guest API and `require()` fail with EAGAIN instead.
 */
export interface Vfs {
    stat(path: string): VfsMetadata | Promise<VfsMetadata>;
    readdir(path: string): VfsDirEntry[] | Promise<VfsDirEntry[]>;
    mkdir(path: string): void | Promise<void>;
    rename(from: string, to: string): void | Promise<void>;
    unlink(path: string): void | Promise<void>;
    rmdir(path: string): void | Promise<void>;
    open(path: string, mode: VfsOpenMode): number | Promise<number>;
    /** A promise must fill `buffer` before it resolves. */
    readAt(handle: number, offset: number, buffer: Uint8Array): number | Promise<number>;
    writeAt(handle: number, offset: number, data: Uint8Array): number | Promise<number>;
    truncate(handle: number, len: number): void | Promise<void>;
    close(handle: number): void | Promise<void>;
    /** Discard staged writes on failure; falls back to close when omitted. */
    abort?(handle: number): void | Promise<void>;
}
export interface RunCodeOptions {
    globals?: Record<string, HostGlobal>;
    vfs?: Vfs;
    wasmMemoryBytes?: number;
    quickjsHeapBytes?: number;
    timeoutMs?: number;
    /** Synchronous execution observes aborts at checkpoints; timers cannot preempt host callbacks. */
    signal?: AbortSignal;
    sourceBytes?: number;
    hostResponseBytes?: number;
    hostInputBytes?: number;
    maxOpenFiles?: number;
    stdoutBytes?: number;
    stderrBytes?: number;
    scriptPath?: string;
    argv?: string[];
    env?: Record<string, string>;
    cwd?: string;
}
export interface RunFileOptions extends Omit<RunCodeOptions, "vfs" | "scriptPath"> {
    vfs: Vfs;
}
export interface RunResult {
    exitCode: number;
    stdout: string;
    stderr: string;
    initialWasmMemoryBytes: number;
    peakWasmMemoryBytes: number;
}
export interface JsEngine {
    runCode(code: string, options?: RunCodeOptions): Promise<RunResult>;
    runFile(path: string, options: RunFileOptions): Promise<RunResult>;
}
