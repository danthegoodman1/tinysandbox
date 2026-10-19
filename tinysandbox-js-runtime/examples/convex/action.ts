import { createEngine, VfsError, type Vfs } from "@tinysandbox/js-runtime";
import quickjsModule from "@tinysandbox/js-runtime/quickjs.wasm";
import { action } from "./_generated/server";

// A read-only Vfs over an object store such as S3. Every call returns a
// promise, as a network client would, so guest code reads through fs.promises.
function objectStoreVfs(get: (key: string) => Promise<Uint8Array | undefined>): Vfs {
  const handles = new Map<number, Uint8Array>();
  let nextHandle = 1;
  const readOnly = async () => { throw new VfsError("EACCES"); };
  const object = async (path: string) => {
    const data = await get(path);
    if (data === undefined) throw new VfsError("ENOENT");
    return data;
  };
  return {
    async stat(path) { return { fileType: "file", len: (await object(path)).byteLength }; },
    async open(path, mode) {
      if (mode.write) throw new VfsError("EACCES");
      handles.set(nextHandle, await object(path));
      return nextHandle++;
    },
    async readAt(handle, offset, buffer) {
      const data = handles.get(handle);
      if (data === undefined) throw new VfsError("EBADF");
      const chunk = data.subarray(offset, offset + buffer.byteLength);
      buffer.set(chunk);
      return chunk.byteLength;
    },
    async close(handle) { handles.delete(handle); },
    readdir: readOnly, mkdir: readOnly, rename: readOnly, unlink: readOnly, rmdir: readOnly, writeAt: readOnly, truncate: readOnly,
  };
}

export const jsRuntimeSmoke = action({
  handler: async (): Promise<string> => {
    const engine = await createEngine(quickjsModule);
    const objects = new Map([["/notes/today.txt", new TextEncoder().encode("stored")]]);
    // Awaited work can happen inside a global or a Vfs call: the guest suspends
    // while the promise settles, so Convex queries and object reads no longer
    // have to be hoisted above the run.
    const result = await engine.runCode(
      "(async () => { console.log(await context.value(null), await require('fs').promises.readFile('/notes/today.txt', 'utf8')) })()",
      {
        globals: { "context.value": async () => await Promise.resolve("convex") },
        vfs: objectStoreVfs(async (key) => objects.get(key)),
      },
    );
    if (result.exitCode !== 0) throw new Error(result.stderr);
    return result.stdout.trim();
  },
});
