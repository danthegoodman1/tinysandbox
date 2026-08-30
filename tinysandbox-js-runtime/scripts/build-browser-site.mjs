import { copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";

const output = new URL("../site-dist/", import.meta.url);
const source = new URL("../examples/browser/index.html", import.meta.url);
const runtime = new URL("../dist/index.js", import.meta.url);
const wasm = new URL("../quickjs.wasm", import.meta.url);

let html = await readFile(source, "utf8");
const replacements = [
  ["../../dist/index.js", "./runtime.js"],
  ["../../quickjs.wasm", "./quickjs.wasm"],
];
for (const [from, to] of replacements) {
  if (!html.includes(from)) throw new Error(`browser example is missing expected asset path ${from}`);
  html = html.replaceAll(from, to);
}

await rm(output, { recursive: true, force: true });
await mkdir(output, { recursive: true });
await writeFile(new URL("index.html", output), html);
await copyFile(runtime, new URL("runtime.js", output));
await copyFile(wasm, new URL("quickjs.wasm", output));

console.log("Cloudflare Pages site built in site-dist/");
