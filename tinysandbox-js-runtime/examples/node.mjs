import { readFile } from "node:fs/promises";
import { createEngine } from "../dist/index.js";

const wasm = await readFile(new URL("../quickjs.wasm", import.meta.url));
const engine = await createEngine(wasm);
const result = engine.runCode("console.log(tools.answer({ question: 'life' }))", {
  globals: { "tools.answer": ({ question }) => `${question}: 42` },
});
console.log(result);
