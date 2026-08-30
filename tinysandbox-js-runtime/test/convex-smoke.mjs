import { build } from "esbuild";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const result = await build({
  entryPoints: [new URL("../examples/convex/action.ts", import.meta.url).pathname],
  bundle: true,
  format: "esm",
  platform: "browser",
  target: "es2022",
  write: false,
  plugins: [{
    name: "convex-wasm-module",
    setup(build) {
      build.onResolve({ filter: /^\.\/_generated\/server$/ }, () => ({ path: "server", namespace: "convex-test" }));
      build.onLoad({ filter: /.*/, namespace: "convex-test" }, () => ({ contents: "export const action = definition => definition", loader: "js" }));
      build.onLoad({ filter: /quickjs\.wasm$/ }, async ({ path }) => {
        const base64 = (await readFile(path)).toString("base64");
        return { contents: `export default new WebAssembly.Module(Uint8Array.from(atob(${JSON.stringify(base64)}), c => c.charCodeAt(0)))`, loader: "js" };
      });
    },
  }],
});
const directory = await mkdtemp(join(tmpdir(), "tinysandbox-convex-"));
const bundle = join(directory, "action.mjs");
await writeFile(bundle, result.outputFiles[0].contents);
const action = await import(pathToFileURL(bundle));
const output = await action.jsRuntimeSmoke.handler();
if (output !== "convex") throw new Error(`Convex-compatible bundle smoke returned ${JSON.stringify(output)}`);
console.log(`convex_v8_bundle_bytes=${result.outputFiles[0].contents.byteLength}`);
console.log("convex_v8_bundle_smoke=PASS");
console.log("convex_remote_smoke=NOT_RUN_NO_CREDENTIALS");
