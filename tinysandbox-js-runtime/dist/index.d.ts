import type { VfsErrno, JsEngine } from "./types.js";
export type * from "./types.js";
export declare const QUICKJS_INITIAL_MEMORY_BYTES: number;
export declare class VfsError extends Error {
    readonly code: VfsErrno;
    constructor(code: VfsErrno, message?: string);
}
export declare function createEngine(wasm: BufferSource | WebAssembly.Module): Promise<JsEngine>;
