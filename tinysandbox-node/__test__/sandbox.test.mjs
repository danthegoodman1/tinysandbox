import test from 'node:test'
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Sandbox, runConformance } from '../index.js'
import { createMemoryVfs } from './helpers.mjs'

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')

test('sandbox lifecycle returns output, exit codes, and metrics', async () => {
  // Pins the basic async lifecycle and camelCase ExecResult shape.
  const sandbox = new Sandbox()
  const result = await sandbox.exec('echo hello && false')
  assert.equal(result.stdout, 'hello\n')
  assert.equal(result.exitCode, 1)
  assert.equal(typeof result.wallTimeMs, 'number')
  assert.equal(result.stdoutTruncated, false)
})

test('limits report wall-clock timeout and wasm memory failure', async () => {
  // Wall-clock timeout should use the same conventional 124 code as the Rust API.
  const impatient = new Sandbox({ limits: { wallTimeMs: 50 } })
  const timeout = await impatient.exec("js -e 'while (true) {}'")
  assert.equal(timeout.exitCode, 124)

  // A tight wasm heap should make large JS allocation fail without crashing Node.
  const constrained = new Sandbox({ limits: { wasmMemoryBytes: 4 * 1024 * 1024 } })
  const oom = await constrained.exec("js -e 'globalThis.x = new ArrayBuffer(64 * 1024 * 1024)'")
  assert.notEqual(oom.exitCode, 0)
  assert.match(oom.stderr, /memory|alloc/i)
})

test('wall-clock limit validation rejects invalid numbers without aborting Node', () => {
  // Constructor errors must cross N-API as exceptions, not Rust panics.
  assert.throws(() => new Sandbox({ limits: { wallTimeMs: -5 } }), /wallTimeMs/)
})

test('numeric validation rejects unsafe byte counts and VFS lengths', async () => {
  // Oversized lengths must fail before native code allocates a Vec for the read.
  const unsafeInteger = Number.MAX_SAFE_INTEGER + 1
  assert.throws(() => new Sandbox({ limits: { stdoutBytes: unsafeInteger } }), /EINVAL/)
  assert.throws(() => new Sandbox({ limits: { jqInputBytes: unsafeInteger } }), /EINVAL/)

  const sandbox = new Sandbox()
  await sandbox.fs.writeFile('/x', Buffer.from('abc'))
  const handle = await sandbox.fs.open('/x', { read: true })
  try {
    await assert.rejects(
      () => sandbox.fs.readAt(handle, 0, unsafeInteger),
      (err) => {
        assert.equal(err.code, 'EINVAL')
        return true
      }
    )
  } finally {
    await sandbox.fs.close(handle)
  }
})

test('jq builtin and jqInputBytes limit are available through Node options', async () => {
  const sandbox = new Sandbox()
  const result = await sandbox.exec(`echo '{"items":[{"name":"Ada"},{"name":"Grace"}]}' | jq -r '.items[].name'`)
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, 'Ada\nGrace\n')

  const limited = new Sandbox({ limits: { jqInputBytes: 3 } })
  const overLimit = await limited.exec("echo 1234 | jq '.'")
  assert.equal(overLimit.exitCode, 2)
  assert.match(overLimit.stderr, /input too large/)

  const recursiveFilter = await sandbox.exec("jq -n 'def f: f + 1; f'")
  assert.equal(recursiveFilter.exitCode, 3)
  assert.match(recursiveFilter.stderr, /user-defined jq functions are not supported/)

  const deepFilter = await sandbox.exec(`jq -n '${'['.repeat(3500)}0${']'.repeat(3500)}'`)
  assert.equal(deepFilter.exitCode, 3)
  assert.match(deepFilter.stderr, /jq filter nesting exceeds/)

  for (const [name, filter] of [
    ['unary minus', `${'-'.repeat(1100)}0`],
    ['try', `${'try '.repeat(1100)}0`],
    ['alt', `1${'//1'.repeat(1100)}`],
    ['pipe', Array(1100).fill('.').join('|')]
  ]) {
    // These filters previously overflowed jaq parser recursion and aborted Node.
    const rejected = await sandbox.exec(`jq -n -- '${filter}'`)
    assert.equal(rejected.exitCode, 3, `${name}: ${rejected.stderr}`)
    assert.match(rejected.stderr, /jq filter complexity exceeds/, name)
  }

  const deepJson = `${'['.repeat(1100)}0${']'.repeat(1100)}`
  await sandbox.fs.writeFile('/deep.json', Buffer.from(deepJson))
  const deepInput = await sandbox.exec("jq -c '.' /deep.json")
  assert.equal(deepInput.exitCode, 2)
  assert.match(deepInput.stderr, /JSON nesting exceeds/)
})

test('direct VFS calls read, write, stat, readdir, and unlink', async () => {
  // Host-side VFS calls should work without shelling through exec.
  const sandbox = new Sandbox()
  assert.equal(sandbox.fs, sandbox.fs)
  await sandbox.fs.mkdir('/work')
  await sandbox.fs.writeFile('/work/a.txt', Buffer.from('alpha'))
  assert.equal(String(await sandbox.fs.readFile('/work/a.txt')), 'alpha')
  assert.deepEqual(await sandbox.fs.stat('/work/a.txt'), {
    fileType: 'file',
    len: 5,
    isFile: true,
    isDir: false
  })
  assert.deepEqual((await sandbox.fs.readdir('/work')).map((entry) => entry.name), ['a.txt'])
  await sandbox.fs.unlink('/work/a.txt')
  await assert.rejects(
    () => sandbox.fs.stat('/work/a.txt'),
    (err) => {
      assert.equal(err.code, 'ENOENT')
      assert.match(err.message, /\/work\/a\.txt/)
      return true
    }
  )
})

test('cached direct VFS calls use the current persistent session cwd', async () => {
  // The JS facade stays stable while native path resolution follows later cd calls.
  const sandbox = new Sandbox({ persistSession: true })
  const fs = sandbox.fs
  await fs.mkdir('/a')
  await sandbox.exec('cd /a')
  await fs.writeFile('y.txt', Buffer.from('cwd-aware'))
  assert.equal(String(await fs.readFile('/a/y.txt')), 'cwd-aware')
  await assert.rejects(() => fs.stat('/y.txt'), { code: 'ENOENT' })
})

test('custom JS command composes in a pipeline', async () => {
  // Custom commands are buffered at the JS boundary but still stream through Rust pipelines.
  const sandbox = new Sandbox({
    commands: {
      upper: async ({ stdin }) => {
        assert.equal(Buffer.isBuffer(stdin), true)
        return { stdout: Buffer.from(stdin.toString('utf8').toUpperCase()) }
      }
    }
  })
  const result = await sandbox.exec('echo make noise | upper | wc -w')
  assert.equal(result.stdout, '      2\n')
})

test('JS VFS conformance runner accepts callback implementations', async () => {
  // Third-party JS VFS implementations can self-certify the public VFS contract.
  const result = await runConformance((quota) => createMemoryVfs(quota))
  assert.deepEqual(result, { ok: true, snapshots: 'unsupported' })
})

test('Sandbox can execute against a JS VFS adapter', async () => {
  // Exercises the Rust sync Vfs trait backed by async JS callbacks through TSFN.
  const sandbox = new Sandbox({ vfs: createMemoryVfs() })
  await sandbox.fs.mkdir('/app')
  await sandbox.fs.writeFile('/app/input.txt', Buffer.from('one\ntwo\n'))
  const result = await sandbox.exec('cat /app/input.txt | wc -l')
  assert.equal(result.stdout, '      2\n')
})

test('Sandbox stats can call a JS VFS stats callback without deadlocking', async () => {
  // stats() is async because JS VFS callbacks need the main thread to keep pumping promises.
  const sandbox = new Sandbox({ vfs: createMemoryVfs() })
  await sandbox.fs.writeFile('/stats.txt', Buffer.from('abc'))
  const stats = await Promise.race([
    sandbox.stats(),
    new Promise((_, reject) => setTimeout(() => reject(new Error('deadlocked')), 1000))
  ])
  assert.deepEqual(stats.vfs, { usedBytes: 3, fileCount: 1 })
})

test('Node e2e pipeline can cat into the sandboxed js command', async () => {
  // Direct host writes and the built-in Wasmtime QuickJS command share the same VFS.
  const sandbox = new Sandbox()
  await sandbox.fs.writeFile('/x', Buffer.from('abc'))
  await sandbox.fs.writeFile('/t.js', Buffer.from('const fs = require("fs")\nconsole.log(fs.readFileSync("/x", "utf8").toUpperCase())\n'))
  const result = await sandbox.exec('cat /x | js /t.js')
  assert.equal(result.exitCode, 0)
  assert.equal(result.stdout, 'ABC\n')
})

test('JS syscalls round-trip JSON values through Node handlers', async () => {
  // Generated sandbox.<name> functions synchronously call the async Node host callback.
  const sandbox = new Sandbox({
    syscalls: {
      inspect: async (args) => {
        assert.deepEqual(args, { value: 7 })
        return { doubled: args.value * 2, nested: [true, null, 'ok'] }
      }
    }
  })
  const result = await sandbox.exec("js -e 'const out = sandbox.inspect({ value: 7 }); console.log(JSON.stringify(out))'")
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, '{"doubled":14,"nested":[true,null,"ok"]}\n')
})

test('JS syscall handler errors preserve string code fields', async () => {
  // Thrown Node errors become guest Error objects with message and optional code.
  const sandbox = new Sandbox({
    syscalls: {
      deny: () => {
        const err = new Error('denied by host')
        err.code = 'E_DENIED'
        throw err
      }
    }
  })
  const script = `
try {
  sandbox.deny({ id: 1 })
} catch (err) {
  console.log(err.message)
  console.log(err.code)
}
`
  const result = await sandbox.exec(`js -e ${singleQuote(script)}`)
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, 'denied by host\nE_DENIED\n')
})

test('jsPrelude can wrap a Node-backed syscall', async () => {
  // The prelude can expose a narrower global and hide the generated sandbox object.
  const sandbox = new Sandbox({
    syscalls: {
      kvGet: ({ key }) => ({ value: key === 'answer' ? 42 : null })
    },
    jsPrelude: 'const kvGet = sandbox.kvGet; globalThis.getAnswer = () => kvGet({ key: "answer" }).value; delete globalThis.sandbox'
  })
  const result = await sandbox.exec("js -e 'console.log(getAnswer(), typeof sandbox)'")
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, '42 undefined\n')
})

test('fetch calls a Node transport with Buffer request and response bodies', async () => {
  // Request bytes arrive as Buffer and response bytes return through Response body helpers.
  const sandbox = new Sandbox({
    fetch: async (request) => {
      assert.equal(request.url, 'https://example.test/echo')
      assert.equal(request.method, 'POST')
      assert.equal(Buffer.isBuffer(request.body), true)
      assert.equal(request.body.toString('utf8'), 'ping')
      assert.deepEqual(request.headers.find(([name]) => name === 'x-token'), ['x-token', 'abc'])
      return {
        status: 201,
        headers: [['content-type', 'text/plain']],
        body: Buffer.from(`reply:${request.body.toString('utf8')}`)
      }
    }
  })
  const script = `
(async () => {
  const response = await fetch('https://example.test/echo', {
    method: 'POST',
    headers: { 'x-token': 'abc' },
    body: Buffer.from('ping')
  })
  console.log(response.status)
  console.log(response.headers.get('content-type'))
  console.log(await response.text())
})()
`
  const result = await sandbox.exec(`js -e ${singleQuote(script)}`)
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, '201\ntext/plain\nreply:ping\n')
})

test('fetch without a Node handler rejects with network unavailable', async () => {
  // The fetch global exists, but network access is absent until the embedder grants it.
  const sandbox = new Sandbox()
  const script = `
(async () => {
  try {
    await fetch('https://example.test/')
  } catch (err) {
    console.log(err.name, err.message)
    console.log(err.cause && err.cause.message)
  }
})()
`
  const result = await sandbox.exec(`js -e ${singleQuote(script)}`)
  assert.equal(result.exitCode, 0, result.stderr)
  assert.match(result.stdout, /^TypeError fetch failed\n/)
  assert.match(result.stdout, /network is not available/)
})

test('fetchResponseBytes limits Node fetch response bodies', async () => {
  // The configured cap is enforced after the Node handler returns bytes.
  const sandbox = new Sandbox({
    limits: { fetchResponseBytes: 3 },
    fetch: () => ({ status: 200, body: Buffer.from('toolong') })
  })
  const script = `
(async () => {
  try {
    await fetch('https://example.test/large')
  } catch (err) {
    console.log(err.name, err.message)
    console.log(err.cause && err.cause.message)
  }
})()
`
  const result = await sandbox.exec(`js -e ${singleQuote(script)}`)
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, 'TypeError fetch failed\nfetch response body exceeded limit of 3 bytes\n')
})

test('invalid syscall names throw during Sandbox construction', () => {
  // Constructor validation prevents Rust builder panics from crossing N-API.
  assert.throws(
    () => new Sandbox({ syscalls: { 'bad-name': () => null } }),
    /invalid syscall name/
  )
})

test('direct VFS calls proceed while exec is in flight', async () => {
  // The command bridge must not monopolize the JS event loop while an exec awaits.
  const sandbox = new Sandbox({
    commands: {
      wait: async () => new Promise((resolve) => setTimeout(() => resolve({ stdout: Buffer.from('done\n') }), 50))
    }
  })
  const running = sandbox.exec('wait')
  await sandbox.fs.writeFile('/during.txt', Buffer.from('ok'))
  assert.equal(String(await sandbox.fs.readFile('/during.txt')), 'ok')
  assert.equal((await running).stdout, 'done\n')
})

test('JS VFS callbacks do not deadlock on the same event loop', async () => {
  // readAt yields back to the same JS loop that is awaiting exec; a synchronous main-thread call would hang here.
  const vfs = createMemoryVfs()
  const originalReadAt = vfs.readAt
  vfs.readAt = async (request) => {
    await Promise.resolve()
    return originalReadAt(request)
  }
  const sandbox = new Sandbox({ vfs })
  await sandbox.fs.writeFile('/x', Buffer.from('deadlock-free'))
  const result = await Promise.race([
    sandbox.exec('cat /x'),
    new Promise((_, reject) => setTimeout(() => reject(new Error('deadlocked')), 1000))
  ])
  assert.equal(result.stdout, 'deadlock-free')
})

test('unknown JS VFS error text collapses to EINVAL instead of substring matching', async () => {
  const vfs = createMemoryVfs()
  vfs.stat = async () => {
    throw new Error('file ENOENT-ish not really')
  }
  const sandbox = new Sandbox({ vfs })
  await assert.rejects(
    () => sandbox.fs.stat('/x'),
    (err) => {
      assert.equal(err.code, 'EINVAL')
      return true
    }
  )
})

test('JS VFS request data is delivered as a Buffer', async () => {
  const vfs = createMemoryVfs()
  const originalWriteAt = vfs.writeAt
  vfs.writeAt = async (request) => {
    assert.equal(Buffer.isBuffer(request.data), true)
    return originalWriteAt(request)
  }
  const sandbox = new Sandbox({ vfs })
  await sandbox.fs.writeFile('/buffer.txt', Buffer.from('buffered'))
  assert.equal(String(await sandbox.fs.readFile('/buffer.txt')), 'buffered')
})

test('JS VFS adapters do not keep child processes alive', async () => {
  const script = `
    import { Sandbox } from './index.js'
    import { createMemoryVfs } from './__test__/helpers.mjs'
    const sandbox = new Sandbox({ vfs: createMemoryVfs() })
    await sandbox.fs.writeFile('/x', Buffer.from('ok'))
    const result = await sandbox.exec('cat /x')
    if (result.stdout !== 'ok') process.exit(2)
  `

  const child = spawn(process.execPath, ['--import', 'tsx', '--input-type=module', '-e', script], {
    cwd: packageRoot,
    stdio: ['ignore', 'pipe', 'pipe']
  })
  const result = await waitForChild(child, 2000)
  assert.equal(result.code, 0, result.stderr)
})

function waitForChild(child, timeoutMs) {
  return new Promise((resolvePromise, reject) => {
    let stdout = ''
    let stderr = ''
    const timeout = setTimeout(() => {
      child.kill()
      reject(new Error('child process did not exit naturally'))
    }, timeoutMs)

    child.stdout.on('data', (chunk) => {
      stdout += chunk
    })
    child.stderr.on('data', (chunk) => {
      stderr += chunk
    })
    child.on('error', (err) => {
      clearTimeout(timeout)
      reject(err)
    })
    child.on('close', (code) => {
      clearTimeout(timeout)
      resolvePromise({ code, stdout, stderr })
    })
  })
}

function singleQuote(value) {
  return `'${value.replaceAll("'", "'\\''")}'`
}
