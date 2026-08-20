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
  /** Static top-level filesystem mounts. Replaces the default in-memory workspace when present. */
  mounts?: Record<string, MountOptions>
}

export type MountOptions =
  | ({ type: 'memory' } & MemoryVfsOptions)
  | ({ type: 'local' } & LocalVfsOptions)
  | ({ type: 's3' } & S3VfsOptions)
  | { type: 'custom', vfs: JsVfs }

export interface MemoryVfsOptions {
  /** Storage limits. Unset fields are unlimited. */
  quota?: VfsQuotaOptions
}

export interface VfsQuotaOptions {
  maxBytes?: number
  maxFiles?: number
  maxFileSize?: number
}

/**
 * Backs the sandbox filesystem with a directory on the host (Unix only).
 *
 * Paths within the mount resolve strictly beneath the root: `..` is clamped at
 * the virtual sandbox root and symlinks are never followed. The directory must exist and
 * should be dedicated to the sandbox; existing regular files and directories
 * are visible inside and count toward the quota.
 */
export interface LocalVfsOptions {
  /** Existing host directory that becomes this mount's root. */
  root: string
  /** Storage limits. Unset fields are unlimited. */
  quota?: VfsQuotaOptions
}

/**
 * Exposes one S3 bucket/key prefix as a read-write mount root.
 *
 * Region and credentials use the AWS SDK default provider chains when they
 * are omitted. Endpoint and path-style overrides support compatible services.
 *
 * S3 has no partial-object update, so a writable handle stages its contents
 * and lands them as one object operation when the handle closes. Writes become
 * visible to other handles and other readers at that point, not before.
 */
export interface S3VfsOptions {
  /** Bucket containing the exposed objects. */
  bucket: string
  /** Key prefix exposed at the mount root. Leading/trailing slashes are normalized. */
  prefix?: string
  /**
   * Rejects every write and path mutation with EACCES. Defaults to false.
   *
   * Credentials remain the enforcing boundary; this only stops the mount from
   * issuing mutating requests with a client that would accept them.
   */
  readOnly?: boolean
  /**
   * Ceiling on bytes staged in memory to modify an existing object, 32 MiB by
   * default. Modifying an object reads it, applies the writes, and puts it
   * back, so its whole body is held in memory. Longer objects fail with EFBIG
   * and must be rewritten instead. Zero removes the limit. Forward-only writes
   * that replace an object stream through a multipart upload and ignore it.
   */
  maxEditBytes?: number
  /**
   * Allows renaming a directory by copying and deleting every key beneath it,
   * enabled by default. S3 has no atomic directory rename: it costs two
   * requests per key, and an interrupted rename leaves keys under both
   * prefixes. When false, renaming a directory fails with EXDEV.
   */
  directoryRename?: boolean
  /**
   * Guards writes with If-Match and If-None-Match preconditions so a concurrent
   * replacement fails instead of being silently overwritten. Enabled by
   * default; disable only for a service that rejects conditional writes.
   */
  conditionalWrites?: boolean
  /** AWS region override. */
  region?: string
  /** S3-compatible service endpoint override. */
  endpointUrl?: string
  /** Force path-style bucket addressing. */
  forcePathStyle?: boolean
  /** Explicit credentials; omit to use the AWS SDK default provider chain. */
  credentials?: {
    accessKeyId: string
    secretAccessKey: string
    sessionToken?: string
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
  | 'EXDEV'
  | 'EACCES'
  | 'EEXIST'
  | 'EFBIG'
  | 'EIO'
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
