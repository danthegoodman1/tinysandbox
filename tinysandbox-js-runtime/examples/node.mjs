import { readFile } from "node:fs/promises";
import { createEngine } from "../dist/index.js";
import { TestVfs } from "../test/test-vfs.mjs";

const wasm = await readFile(new URL("../quickjs.wasm", import.meta.url));
const engine = await createEngine(wasm);
const result = engine.runCode("console.log(tools.answer({ question: 'life' }))", {
  globals: { "tools.answer": ({ question }) => `${question}: 42` },
});
console.log(result);

// TestVfs is a deterministic example fixture, not part of the runtime API.
// Production callers supply their own synchronous storage implementation.
const vfs = new TestVfs({
  "/app/main.js": "console.log(__filename, require('./message').text)",
  "/app/message.js": "exports.text = require('fs').readFileSync('./value', 'utf8')",
  "/app/value": "from-vfs",
});
console.log(engine.runFile("main.js", { vfs, cwd: "/app" }));
