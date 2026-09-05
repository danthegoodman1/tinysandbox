export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export interface HostContext {
  /** Aborts on callback completion; cancellation is refreshed at checkpoints and by context polling. */
  readonly signal: AbortSignal;
  /** Approximate Unix timestamp; remainingTimeMs uses the monotonic deadline. */
  readonly deadlineMs: number;
  remainingTimeMs(): number;
  isCancelled(): boolean;
}

export type HostGlobal = (argument: JsonValue, context: HostContext) => JsonValue;

export type VfsErrno = "EBADF" | "EBUSY" | "EXDEV" | "EACCES" | "EEXIST" | "EFBIG" | "EIO" | "EINVAL" | "EISDIR" | "ENOENT" | "ENOSPC" | "ENOTDIR" | "ENOTEMPTY";

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

/** Synchronous, absolute-path, handle-and-offset filesystem capability. */
export interface Vfs {
  stat(path: string): VfsMetadata;
  readdir(path: string): VfsDirEntry[];
  mkdir(path: string): void;
  rename(from: string, to: string): void;
  unlink(path: string): void;
  rmdir(path: string): void;
  open(path: string, mode: VfsOpenMode): number;
  readAt(handle: number, offset: number, buffer: Uint8Array): number;
  writeAt(handle: number, offset: number, data: Uint8Array): number;
  truncate(handle: number, len: number): void;
  close(handle: number): void;
  /** Discard staged writes on failure; falls back to close when omitted. */
  abort?(handle: number): void;
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
  runCode(code: string, options?: RunCodeOptions): RunResult;
  runFile(path: string, options: RunFileOptions): RunResult;
}
