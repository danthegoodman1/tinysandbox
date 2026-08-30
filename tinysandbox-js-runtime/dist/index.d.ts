export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export type HostGlobal = (argument: JsonValue) => JsonValue;

export interface RunCodeOptions {
  globals?: Record<string, HostGlobal>;
  wasmMemoryBytes?: number;
  quickjsHeapBytes?: number;
  timeoutMs?: number;
  sourceBytes?: number;
  hostResponseBytes?: number;
  stdoutBytes?: number;
  stderrBytes?: number;
  scriptPath?: string;
  argv?: string[];
  env?: Record<string, string>;
  cwd?: string;
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
}

export declare const QUICKJS_INITIAL_MEMORY_BYTES: number;
export declare function createEngine(wasm: BufferSource | WebAssembly.Module): Promise<JsEngine>;
