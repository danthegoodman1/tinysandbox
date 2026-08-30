import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { createEngine, VfsError } from "../dist/index.js";
import { TestVfs } from "./test-vfs.mjs";

const bytes = await readFile(new URL("../quickjs.wasm", import.meta.url));
const engine = await createEngine(bytes);
const portableVfsCorpus = JSON.parse(await readFile(new URL("../../tests/fixtures/js_vfs_portable_corpus.json", import.meta.url), "utf8"));

test("runs the shared Rust/V8 VFS and CommonJS corpus", () => {
  for (const item of portableVfsCorpus) {
    const vfs = new TestVfs(Object.fromEntries(item.files.map(file => [file.path, file.text])));
    const result = engine.runFile(item.entry, { vfs, argv: item.argv });
    assert.equal(result.exitCode, item.exitCode, item.name);
    assert.equal(result.stdout, item.stdout, item.name);
    assert.ok(result.stderr.startsWith(item.stderrPrefix), `${item.name}: ${result.stderr}`);
  }
});

test("omitting VFS exposes no filesystem or Buffer capability", () => {
  const result = engine.runCode(`
console.log(typeof Buffer)
for (const request of ['fs', './module']) {
  try { require(request) } catch (error) { console.log(request, error.code, error.message) }
}
`);
  assert.equal(result.exitCode, 0);
  assert.equal(result.stdout,
    "undefined\n" +
    "fs ERR_CAPABILITY_UNAVAILABLE filesystem capability is not available in this runtime\n" +
    "./module ERR_CAPABILITY_UNAVAILABLE filesystem capability is not available in this runtime\n");
});

test("supplying VFS does not grant fetch or route it into storage", () => {
  const code = "fetch('https://example.invalid').catch(error => console.log(error.message, error.cause.code, error.cause.message))";
  const withoutVfs = engine.runCode(code);
  const vfs = new TestVfs();
  const withVfs = engine.runCode(code, { vfs });
  const expected = "fetch failed ENOSYS host capability 'fetch' is not available\n";
  assert.equal(withoutVfs.stdout, expected);
  assert.equal(withVfs.stdout, expected);
  assert.deepEqual(vfs.calls, []);
});

test("guest fs preserves binary, positional fd, directory, and errno behavior", () => {
  const vfs = new TestVfs({
    "/data/utf8": "hello λ",
    "/data/binary": Uint8Array.of(0, 127, 128, 255),
    "/data/io": "abcdef",
  });
  const result = engine.runCode(`
const fs = require('fs')
console.log(fs.readFileSync('/data/utf8', 'utf8'))
console.log(Array.from(fs.readFileSync('/data/binary')).join(','))
const fd = fs.openSync('/data/io', 'r+')
const first = Buffer.alloc(2), second = Buffer.alloc(2), positional = Buffer.alloc(2), final = Buffer.alloc(2)
console.log(fs.readSync(fd, first, 0, 2, null), first.toString())
console.log(fs.readSync(fd, second, 0, 2, null), second.toString())
console.log(fs.readSync(fd, positional, 0, 2, 1), positional.toString())
console.log(fs.readSync(fd, final, 0, 2, null), final.toString())
console.log(fs.writeSync(fd, Buffer.from('XY'), 0, 2, 1))
console.log(fs.writeSync(fd, 'Z'))
fs.ftruncateSync(fd, 6)
fs.closeSync(fd)
console.log(fs.readFileSync('/data/io', 'utf8'))
fs.mkdirSync('/data/dir')
fs.writeFileSync('/data/dir/a', 'A')
fs.mkdirSync('/data/dir/sub')
console.log(fs.readdirSync('/data/dir', { withFileTypes: true }).map(e => [e.name, e.isFile(), e.isDirectory()].join(':')).join(','))
fs.renameSync('/data/dir/a', '/data/dir/b')
fs.unlinkSync('/data/dir/b')
fs.rmdirSync('/data/dir/sub')
fs.rmdirSync('/data/dir')
try { fs.readFileSync('/missing') } catch (error) { console.log(error.code, error.errno, error.syscall, error.path) }
fs.mkdirSync('/nonempty')
fs.writeFileSync('/nonempty/file', 'x')
try { fs.rmdirSync('/nonempty') } catch (error) { console.log(error.code, error.errno) }
` , { vfs });
  assert.equal(result.exitCode, 0, result.stderr);
  assert.equal(result.stdout,
    "hello λ\n0,127,128,255\n2 ab\n2 cd\n2 bc\n2 ef\n2\n1\naXYdef\n" +
    "a:true:false,sub:false:true\nENOENT -2 open /missing\nENOTEMPTY -66\n");
  for (const method of ["stat", "readdir", "mkdir", "rename", "unlink", "rmdir", "open", "readAt", "writeAt", "truncate", "close"]) {
    assert.ok(vfs.calls.some(call => call.method === method), `expected ${method} to reach supplied VFS`);
  }
  assert.equal(vfs.handles.size, 0);
});

test("runFile sets entry paths and preserves CommonJS cache, cycles, and JSON", () => {
  const vfs = new TestVfs({
    "/app/main.js": `
const first = require('./counter')
const second = require('/app/counter.js')
const cycle = require('./a')
const data = require('./data.json')
console.log(process.argv.join('|'))
console.log(__filename, __dirname, require.main === module, module.id)
console.log(first.count, second.count, first === second, cycle.fromB, data.answer)
`,
    "/app/counter.js": "globalThis.moduleLoads = (globalThis.moduleLoads || 0) + 1; exports.count = globalThis.moduleLoads",
    "/app/a.js": "exports.name='a'; const b=require('./b'); exports.fromB=b.sawA",
    "/app/b.js": "const a=require('./a'); exports.sawA=a.name",
    "/app/data.json": "{\"answer\":42}",
  });
  const result = engine.runFile("main.js", { vfs, cwd: "/app", argv: ["js", "main.js", "one"] });
  assert.equal(result.exitCode, 0, result.stderr);
  assert.equal(result.stdout,
    "js|main.js|one\n/app/main.js /app true .\n1 1 true a 42\n");
  assert.ok(vfs.calls.every(call => typeof call.method === "string"));
  assert.ok(vfs.calls.filter(call => call.method === "open" && call.args[0] === "/app/counter.js").length === 1);
  assert.equal(vfs.handles.size, 0);
});

test("runFile preserves the original default argv path while resolving its filename", () => {
  const vfs = new TestVfs({
    "/app/main.js": "console.log(JSON.stringify(process.argv), __filename, __dirname)",
  });
  const result = engine.runFile("./nested/../main.js", { vfs, cwd: "/app" });
  assert.equal(result.exitCode, 0, result.stderr);
  assert.equal(result.stdout, '["js","./nested/../main.js"] /app/main.js /app\n');
});

test("runFile missing, source-limit, invalid UTF-8, and stack failures are deterministic", () => {
  const vfs = new TestVfs({
    "/app/bad.js": Uint8Array.of(0xff, 0xfe),
    "/app/large.js": "12345",
    "/app/throws.js": "function boom(){ throw new Error('broken') } boom()",
  });
  assert.throws(
    () => engine.runFile("missing.js", { vfs, cwd: "/app" }),
    error => error.code === "ENOENT" && error.path === "/app/missing.js" && /open/.test(error.message),
  );
  assert.throws(() => engine.runFile("bad.js", { vfs, cwd: "/app" }), /not valid UTF-8/);
  assert.throws(() => engine.runFile("large.js", { vfs, cwd: "/app", sourceBytes: 4 }), /source exceeded limit of 4 bytes/);
  const result = engine.runFile("throws.js", { vfs, cwd: "/app" });
  assert.equal(result.exitCode, 1);
  assert.match(result.stderr, /broken/);
  assert.match(result.stderr, /\/app\/throws\.js/);
  assert.equal(vfs.handles.size, 0);
});

test("CommonJS depth remains bounded and leaked guest fds close at run teardown", () => {
  const files = { "/deep/leak.js": "require('fs').openSync('/deep/value', 'r'); console.log('open')", "/deep/value": "x" };
  for (let index = 0; index < 258; index++) files[`/deep/m${index}.js`] = `module.exports = require('./m${index + 1}')`;
  files["/deep/m258.js"] = "module.exports = 1";
  const vfs = new TestVfs(files);
  const depth = engine.runCode("try { require('/deep/m0') } catch (error) { console.log(error.code, /256/.test(error.message)) }", { vfs });
  assert.equal(depth.exitCode, 0, depth.stderr);
  assert.equal(depth.stdout, "ERR_REQUIRE_DEPTH true\n");
  const leak = engine.runFile("/deep/leak.js", { vfs });
  assert.equal(leak.stdout, "open\n");
  assert.equal(vfs.handles.size, 0);
});

test("VFS failures use stable errno mapping and rejecting async implementations are contained", async () => {
  const denied = new TestVfs();
  denied.stat = () => { throw new VfsError("EACCES"); };
  const deniedResult = engine.runCode("const fs=require('fs'); try { fs.statSync('/x') } catch (e) { console.log(e.code, e.errno, e.message) }", { vfs: denied });
  assert.equal(deniedResult.stdout, "EACCES -13 EACCES: permission denied, stat '/x'\n");

  const failedRead = new TestVfs({ "/x": "x" });
  failedRead.readAt = () => { throw new VfsError("EIO"); };
  const readResult = engine.runCode("const fs=require('fs'); try { fs.readFileSync('/x') } catch (e) { console.log(e.code, e.syscall, e.path) }", { vfs: failedRead });
  assert.equal(readResult.stdout, "EIO open /x\n");

  const asynchronous = new TestVfs({ "/x": "x" });
  asynchronous.stat = async () => { throw new VfsError("EACCES"); };
  const asyncResult = engine.runCode("const fs=require('fs'); try { fs.statSync('/x') } catch (e) { console.log(e.code, e.errno) }", { vfs: asynchronous });
  assert.equal(asyncResult.stdout, "EIO -5\n");
  await new Promise(resolve => setImmediate(resolve));
});
