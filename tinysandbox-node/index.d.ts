import type {
  CommandTiming,
  DirEntryJs,
  ExecResult,
  FileStat,
  NativeSandbox,
  OpenModeJs,
  SandboxFs,
  SandboxStats,
  VfsStatsJs
} from './native'

export {
  CommandTiming,
  DirEntryJs,
  ExecResult,
  FileStat,
  NativeSandbox,
  OpenModeJs,
  SandboxFs,
  SandboxStats,
  VfsStatsJs
}

export declare class Sandbox extends NativeSandbox {
  constructor(options?: SandboxOptions | null)
  override get fs(): SandboxFs
  override stats(): Promise<SandboxStats>
}

export interface SandboxOptions {
  limits?: LimitsOptions
  env?: Record<string, string>
  cwd?: string
  persistSession?: boolean
  commands?: Record<string, JsCommand>
  syscalls?: Record<string, JsSyscall>
  jsPrelude?: string
  fetch?: JsFetch
  /** Custom VFS implemented in JavaScript. Mutually exclusive with localVfs. */
  vfs?: JsVfs
  /** Persist the sandbox filesystem under a host directory. Mutually exclusive with vfs. */
  localVfs?: LocalVfsOptions
}

/**
 * Backs the sandbox filesystem with a directory on the host (Unix only).
 *
 * Sandbox paths resolve strictly beneath the root: `..` is clamped at the
 * sandbox root and symlinks are never followed. The directory must exist and
 * should be dedicated to the sandbox; existing regular files and directories
 * are visible inside and count toward the quota.
 */
export interface LocalVfsOptions {
  /** Existing host directory that becomes the sandbox root `/`. */
  root: string
  /** Storage limits. Unset fields are unlimited. */
  quota?: {
    maxBytes?: number
    maxFiles?: number
    maxFileSize?: number
  }
}

export interface LimitsOptions {
  /** Milliseconds; must be finite and non-negative. */
  wallTimeMs?: number
  /** Byte counts must be non-negative integers at or below Number.MAX_SAFE_INTEGER. */
  stdoutBytes?: number
  stderrBytes?: number
  maxCommands?: number
  sortInputBytes?: number
  /** Maximum total bytes accepted by jq across stdin and file operands. */
  jqInputBytes?: number
  wasmMemoryBytes?: number
  fetchResponseBytes?: number
}

export type JsCommand = (call: CommandCall) => Promise<CommandOutput> | CommandOutput

export interface CommandCall {
  args: Array<string>
  env: Record<string, string>
  cwd: string
  stdin: Buffer
}

export interface CommandOutput {
  exitCode?: number
  stdout?: Buffer | Uint8Array | string
  stderr?: Buffer | Uint8Array | string
}

/** Host function exposed to sandboxed JavaScript as synchronous sandbox.<name>(args). */
export type JsSyscall = (args: unknown) => Promise<JsonValue> | JsonValue

export type JsonValue =
  | null
  | boolean
  | number
  | string
  | Array<JsonValue>
  | { [key: string]: JsonValue }

/** Host transport used by sandboxed JavaScript fetch(). */
export type JsFetch = (request: FetchRequest) => Promise<FetchResponse> | FetchResponse

export interface FetchRequest {
  url: string
  method: string
  headers: Array<[string, string]>
  body: Buffer | null
}

export interface FetchResponse {
  status: number
  headers?: Array<[string, string]>
  body?: Buffer | string
}

export interface JsVfs {
  stat(request: VfsRequest): Promise<VfsResponse> | VfsResponse
  readdir(request: VfsRequest): Promise<Array<DirEntryJs> | VfsResponse> | Array<DirEntryJs> | VfsResponse
  mkdir(request: VfsRequest): Promise<VfsResponse | void> | VfsResponse | void
  rename(request: VfsRequest): Promise<VfsResponse | void> | VfsResponse | void
  unlink(request: VfsRequest): Promise<VfsResponse | void> | VfsResponse | void
  rmdir(request: VfsRequest): Promise<VfsResponse | void> | VfsResponse | void
  open(request: VfsRequest): Promise<number | VfsResponse> | number | VfsResponse
  readAt(request: VfsRequest): Promise<Buffer | VfsResponse> | Buffer | VfsResponse
  writeAt(request: VfsRequest): Promise<number | VfsResponse> | number | VfsResponse
  truncate(request: VfsRequest): Promise<VfsResponse | void> | VfsResponse | void
  close(request: VfsRequest): Promise<VfsResponse | void> | VfsResponse | void
  stats?(request: VfsRequest): Promise<VfsResponse> | VfsResponse
}

export interface VfsRequest {
  path?: string
  from?: string
  to?: string
  mode?: OpenModeJs
  handle?: number
  /** Handle, offset, and len values must be safe JS integers. */
  offset?: number
  len?: number
  data?: Buffer
}

export interface VfsResponse {
  fileType?: 'file' | 'directory'
  len?: number
  entries?: Array<DirEntryJs>
  handle?: number
  bytesRead?: number
  bytesWritten?: number
  data?: Buffer
  usedBytes?: number
  fileCount?: number
  error?: VfsCallbackError
}

export interface VfsCallbackError {
  /** Unknown codes are treated as EINVAL by the native adapter. */
  code?: VfsErrno
  message?: string
}

export type VfsErrno =
  | 'EBADF'
  | 'EBUSY'
  | 'EACCES'
  | 'EEXIST'
  | 'EINVAL'
  | 'EISDIR'
  | 'ENOENT'
  | 'ENOSPC'
  | 'ENOTDIR'
  | 'ENOTEMPTY'

/**
 * Prompt chunks for agent system prompts. Each chunk is a short,
 * self-contained block describing one part of the sandbox; pick the chunks
 * that match your sandbox configuration and join them with blank lines.
 *
 * Skip `syscalls` when no syscalls are registered and `fetch` when no fetch
 * handler is set. Include exactly one of `sessionEphemeral` or
 * `sessionPersistent` depending on the `persistSession` option.
 */
export declare const prompts: {
  /** What the environment is and its hard boundaries. */
  readonly overview: string
  /** The supported shell subset and what fails to parse. */
  readonly shell: string
  /** The available commands (the `js` command is introduced by `js`). */
  readonly builtins: string
  /** The supported jq CLI subset. */
  readonly jq: string
  /** The `js` command and its Node-compatible runtime, including the fs API. */
  readonly js: string
  /** Host syscalls exposed to sandboxed JavaScript as sandbox.<name>(). */
  readonly syscalls: string
  /** The fetch capability inside sandboxed JavaScript. */
  readonly fetch: string
  /** Session behavior with the default per-exec cwd/env reset. */
  readonly sessionEphemeral: string
  /** Session behavior when `persistSession` is enabled. */
  readonly sessionPersistent: string
}

export declare function runConformance(
  vfsFactory: (quota: VfsQuota) => JsVfs | Promise<JsVfs>
): Promise<{ ok: true; snapshots: 'unsupported' }>

export interface VfsQuota {
  maxBytes: number
  maxFiles: number
  maxFileSize: number
}
