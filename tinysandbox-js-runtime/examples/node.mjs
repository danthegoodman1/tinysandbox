import { readFile } from "node:fs/promises";
import { createEngine } from "../dist/index.js";
import { AsyncTestVfs, TestVfs } from "../test/test-vfs.mjs";

const wasm = await readFile(new URL("../quickjs.wasm", import.meta.url));
const engine = await createEngine(wasm);
// A synchronous global returns its value straight to the guest.
const result = await engine.runCode("console.log(tools.answer({ question: 'life' }))", {
  globals: { "tools.answer": ({ question }) => `${question}: 42` },
});
console.log(result);

// An async global suspends the guest until the promise settles. The guest
// awaits it like any other promise.
const searched = await engine.runCode(
  "(async () => { console.log(JSON.stringify(await tools.search({ q: 'kittens' }))) })()",
  {
    globals: {
      "tools.search": async ({ q }) => {
        await new Promise((resolve) => setTimeout(resolve, 10));
        return { hits: [`result for ${q}`] };
      },
    },
  },
);
console.log(searched);

// TestVfs and AsyncTestVfs are deterministic example fixtures, not part of the
// runtime API. Production callers supply their own storage implementation.
const vfs = new TestVfs({
  "/app/main.js": "console.log(__filename, require('./message').text)",
  "/app/message.js": "exports.text = require('fs').readFileSync('./value', 'utf8')",
  "/app/value": "from-vfs",
});
console.log(await engine.runFile("main.js", { vfs, cwd: "/app" }));

// A Vfs may return promises, as a network-backed store would. Guest code
// awaits them through fs.promises; the synchronous fs API fails with EAGAIN.
const remote = new AsyncTestVfs({ "/data/report.txt": "from-async-vfs" });
console.log(await engine.runCode(
  "(async () => { console.log(await require('fs').promises.readFile('/data/report.txt', 'utf8')) })()",
  { vfs: remote },
));
