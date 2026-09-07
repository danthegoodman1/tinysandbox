// Independent Node/Wasm smoke checks for the custom byte ABI; no npm install.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';

const module = new WebAssembly.Module(fs.readFileSync(new URL('../../assets/jq.wasm', import.meta.url)));

function run(filter, data = [], overrides = {}) {
  const options = {
    filter, files: [], raw_output: false, join_output: false,
    compact_output: true, exit_status: false, null_input: data.length === 0,
    slurp: false, sort_keys: false, indent: '  ', vars: [], ...overrides,
  };
  const paths = data.map((_, index) => `input-${index}`);
  const inputs = [Buffer.from(JSON.stringify({ options, paths })), ...data.map(value => Buffer.from(value))];
  const chunks = { 1: [], 2: [] };
  let instance;
  let maxRead = 0;
  let maxWrite = 0;
  instance = new WebAssembly.Instance(module, {
    tinysandbox_jq: {
      input_len(index) { return inputs[index]?.length ?? -1; },
      read_input(index, offset, ptr, len) {
        assert.ok(len >= 0 && len <= 65536);
        const source = inputs[index];
        assert.ok(source && offset >= 0 && offset + len <= source.length);
        new Uint8Array(instance.exports.memory.buffer, ptr, len).set(source.subarray(offset, offset + len));
        maxRead = Math.max(maxRead, len);
        return len;
      },
      write_output(kind, ptr, len) {
        assert.ok(kind === 1 || kind === 2);
        assert.ok(len >= 0 && len <= 65536);
        chunks[kind].push(Buffer.from(new Uint8Array(instance.exports.memory.buffer, ptr, len)));
        maxWrite = Math.max(maxWrite, len);
        return len;
      },
      now() { return 1700000000; },
    },
  });
  const code = instance.exports.run();
  return { code, stdout: Buffer.concat(chunks[1]).toString(), stderr: Buffer.concat(chunks[2]).toString(), maxRead, maxWrite };
}

test('the guest exposes only the explicit ABI and no WASI capability', () => {
  assert.deepEqual(WebAssembly.Module.imports(module).map(({ module, name, kind }) => `${module}.${name}:${kind}`).sort(), [
    'tinysandbox_jq.input_len:function', 'tinysandbox_jq.now:function',
    'tinysandbox_jq.read_input:function', 'tinysandbox_jq.write_output:function',
  ]);
  assert.deepEqual(WebAssembly.Module.exports(module), [
    { name: 'memory', kind: 'memory' }, { name: 'run', kind: 'function' },
  ]);
});

test('input/output stay chunked across a large single JSON value', () => {
  const value = 'x'.repeat(200000);
  const result = run('.value', [JSON.stringify({ value })], { raw_output: true });
  assert.equal(result.code, 0);
  assert.equal(result.stdout, `${value}\n`);
  assert.equal(result.stderr, '');
  assert.equal(result.maxRead, 65536);
  assert.equal(result.maxWrite, 65536);
});

test('multiple sources, slurp, and malformed --argjson preserve CLI status', () => {
  assert.equal(run('.', ['1', '2'], { slurp: true }).stdout, '[1,2]\n');
  const invalid = run('.[', [], { vars: [{ name: '$bad', value: '[', json: true }] });
  assert.equal(invalid.code, 2);
  assert.match(invalid.stderr, /invalid JSON for --argjson bad/);
  const compile = run('.[');
  assert.equal(compile.code, 3);
  assert.match(compile.stderr, /compile error/);
});

test('clock and local time behavior are explicit', () => {
  assert.equal(run('now').stdout, '1700000000.0\n');
  assert.equal(run('0 | localtime').stdout, '[1970,0,1,0,0,0,4,0]\n');
  assert.equal(run('0 | strflocaltime("%Y-%m-%d %H:%M:%S %z")', [], { raw_output: true }).stdout,
    '1970-01-01 00:00:00 +0000\n');
});

test('the evaluator cannot see the host process environment', () => {
  assert.equal(run('env').stdout, '{}\n');
});
