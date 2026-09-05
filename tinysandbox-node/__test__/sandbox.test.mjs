import test from 'node:test'
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Sandbox, jsRuntimeSource, prompts, runConformance } from '../index.js'
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

test('removed single-root VFS selectors fail loudly', () => {
  for (const option of ['vfs', 'localVfs', 's3Vfs']) {
    assert.throws(() => new Sandbox({ [option]: {} }), /replaced by the mounts option/)
  }
})

test('numeric validation rejects unsafe byte counts and VFS lengths', async () => {
  // Oversized lengths must fail before native code allocates a Vec for the read.
  const unsafeInteger = Number.MAX_SAFE_INTEGER + 1
  assert.throws(() => new Sandbox({ limits: { stdoutBytes: unsafeInteger } }), /EINVAL/)
  assert.throws(() => new Sandbox({ limits: { jqInputBytes: unsafeInteger } }), /EINVAL/)
  assert.throws(() => new Sandbox({ limits: { jqMemoryBytes: unsafeInteger } }), /EINVAL/)

  const sandbox = new Sandbox()
  await sandbox.fs.writeFile('/workspace/x', Buffer.from('abc'))
  const handle = await sandbox.fs.open('/workspace/x', { read: true })
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

test('host globals receive correlated deadlines and preserve one-argument callbacks', async () => {
  let observed
  const sandbox = new Sandbox({
    limits: { wallTimeMs: 1000 },
    commands: { delay: async () => { await new Promise((resolve) => setTimeout(resolve, 120)); return {} } },
    globals: {
      legacy: (args) => args.value,
      inspect: (args, context) => {
        assert.deepEqual(args, { value: 'new' })
        assert.ok(context.signal instanceof AbortSignal)
        assert.equal(context.isCancelled(), false)
        assert.ok(context.remainingTimeMs() > 0)
        assert.ok(context.remainingTimeMs() < 900, 'earlier command consumed the shared budget')
        assert.ok(Math.abs(context.deadlineMs - Date.now() - context.remainingTimeMs()) < 100)
        observed = context
        return args.value
      }
    }
  })
  const result = await sandbox.exec(`delay; js -e 'console.log(legacy({value:"old"}), inspect({value:"new"}))'`)
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, 'old new\n')
  assert.ok(observed)
  assert.equal(observed.signal.aborted, true, 'callback completion aborts retained signals without polling')
  assert.equal((await sandbox.exec(`js -e 'console.log(legacy({value:"healthy"}))'`)).stdout, 'healthy\n')
})

test('global, fetch, and custom-command callbacks receive abort on their actual deadline', async () => {
  for (const kind of ['global', 'fetch', 'command']) {
    let resolveAborted
    let seen = false
    const aborted = new Promise((resolve) => { resolveAborted = resolve })
    const wait = (context, response) => {
      seen = true
      assert.ok(context.remainingTimeMs() > 0)
      return new Promise((resolve) => {
        const stop = () => { resolveAborted(); resolve(response) }
        if (context.signal.aborted) stop()
        else context.signal.addEventListener('abort', stop, { once: true })
      })
    }
    const options = { limits: { wallTimeMs: 250 } }
    let script
    if (kind === 'global') {
      options.globals = { wait: (_args, context) => wait(context, null) }
      script = `js -e 'wait(null)'`
    } else if (kind === 'fetch') {
      options.fetch = (_request, context) => wait(context, { status: 499 })
      script = `js -e 'fetch("https://example.test/wait")'`
    } else {
      options.commands = { wait: (call) => wait(call, {}) }
      script = 'wait'
    }
    const sandbox = new Sandbox(options)
    const result = await sandbox.exec(script)
    // Guest host-call deadlines reserve shell cleanup time and are catchable
    // exceptions; commands use the outer execution's deadline directly.
    assert.equal(result.exitCode, kind === 'command' ? 124 : 1, `${kind}: ${result.stderr}`)
    assert.equal(seen, true, kind)
    let watchdog
    try {
      await Promise.race([aborted, new Promise((_resolve, reject) => {
        watchdog = setTimeout(() => reject(new Error(`${kind} callback did not receive abort`)), 1500)
      })])
    } finally { clearTimeout(watchdog) }
    assert.equal((await sandbox.exec('echo healthy')).stdout, 'healthy\n')
  }
})

test('callback completion aborts retained signals before the next invocation', async () => {
  let previous
  let aborts = 0
  const sandbox = new Sandbox({ globals: {
    inspect: (fail, context) => {
      if (previous) assert.equal(previous.signal.aborted, true)
      assert.equal(context.signal.aborted, false)
      context.signal.addEventListener('abort', () => { aborts += 1 }, { once: true })
      previous = context
      if (fail) throw new Error('expected')
      return null
    }
  } })
  const result = await sandbox.exec(`js -e 'inspect(false); try { inspect(true) } catch {} inspect(false)'`)
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(previous.signal.aborted, true)
  assert.equal(aborts, 3)
})

test('callbacks queued behind a blocked Node event loop never start after expiration', async () => {
  for (const kind of ['global', 'fetch', 'command']) {
    let calls = 0
    const options = { limits: { wallTimeMs: 200 } }
    let script
    if (kind === 'global') {
      options.globals = { effect: () => { calls += 1; return null } }
      script = `js -e 'effect(null)'`
    } else if (kind === 'fetch') {
      options.fetch = () => { calls += 1; return { status: 200 } }
      script = `js -e 'fetch("https://example.test/effect")'`
    } else {
      options.commands = { effect: () => { calls += 1; return {} } }
      script = 'effect'
    }
    const sandbox = new Sandbox(options)
    const pending = sandbox.exec(script)
    // Rust runs on its own threads and queues the TSFN while Node cannot
    // service it. Wait well past the deadline before delivering that queue.
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 450)
    const result = await pending
    assert.notEqual(result.exitCode, 0, kind)
    await new Promise((resolve) => setImmediate(resolve))
    assert.equal(calls, 0, `${kind} must not begin host effects after cancellation`)
    assert.equal((await sandbox.exec('echo healthy')).stdout, 'healthy\n')
  }
})

test('jqMemoryBytes constrains jq without poisoning later executions', async () => {
  const sandbox = new Sandbox({ limits: { jqMemoryBytes: 1 } })
  const result = await sandbox.exec(`echo '{}' | jq '.'`)
  assert.notEqual(result.exitCode, 0)
  assert.match(result.stderr, /memory|heap|limit/i)
  assert.equal((await sandbox.exec('echo healthy')).stdout, 'healthy\n')
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
  await sandbox.fs.writeFile('/workspace/deep.json', Buffer.from(deepJson))
  const deepInput = await sandbox.exec("jq -c '.' /workspace/deep.json")
  assert.equal(deepInput.exitCode, 2)
  assert.match(deepInput.stderr, /JSON nesting exceeds/)
})

test('direct VFS calls read, write, stat, readdir, and unlink', async () => {
  // Host-side VFS calls should work without shelling through exec.
  const sandbox = new Sandbox()
  assert.equal(sandbox.fs, sandbox.fs)
  await sandbox.fs.mkdir('/workspace/work')
  await sandbox.fs.writeFile('/workspace/work/a.txt', Buffer.from('alpha'))
  assert.equal(String(await sandbox.fs.readFile('/workspace/work/a.txt')), 'alpha')
  assert.deepEqual(await sandbox.fs.stat('/workspace/work/a.txt'), {
    fileType: 'file',
    len: 5,
    isFile: true,
    isDir: false
  })
  assert.deepEqual((await sandbox.fs.readdir('/workspace/work')).map((entry) => entry.name), ['a.txt'])
  await sandbox.fs.unlink('/workspace/work/a.txt')
  await assert.rejects(
    () => sandbox.fs.stat('/workspace/work/a.txt'),
    (err) => {
      assert.equal(err.code, 'ENOENT')
      assert.match(err.message, /\/workspace\/work\/a\.txt/)
      return true
    }
  )
})

test('cached direct VFS calls use the current persistent session cwd', async () => {
  // The JS facade stays stable while native path resolution follows later cd calls.
  const sandbox = new Sandbox({ persistSession: true })
  const fs = sandbox.fs
  await fs.mkdir('/workspace/a')
  await sandbox.exec('cd /workspace/a')
  await fs.writeFile('y.txt', Buffer.from('cwd-aware'))
  assert.equal(String(await fs.readFile('/workspace/a/y.txt')), 'cwd-aware')
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
  const sandbox = new Sandbox({ mounts: { workspace: { type: 'custom', vfs: createMemoryVfs() } } })
  await sandbox.fs.mkdir('/workspace/app')
  await sandbox.fs.writeFile('/workspace/app/input.txt', Buffer.from('one\ntwo\n'))
  const result = await sandbox.exec('cat /workspace/app/input.txt | wc -l')
  assert.equal(result.stdout, '      2\n')
})

test('static mounts are listed at root and route operations independently', async () => {
  const sandbox = new Sandbox({
    mounts: {
      input: { type: 'custom', vfs: createMemoryVfs() },
      output: { type: 'custom', vfs: createMemoryVfs() }
    },
    cwd: '/input'
  })

  assert.deepEqual((await sandbox.fs.readdir('/')).map((entry) => entry.name), ['bin', 'input', 'output'])
  await sandbox.fs.writeFile('/input/source.txt', Buffer.from('mounted'))
  const copied = await sandbox.exec('cp /input/source.txt /output/copied.txt')
  assert.equal(copied.exitCode, 0, copied.stderr)
  assert.equal(String(await sandbox.fs.readFile('/output/copied.txt')), 'mounted')
  await assert.rejects(
    () => sandbox.fs.rename('/input/source.txt', '/output/moved.txt'),
    { code: 'EXDEV' }
  )
})

test('Sandbox stats can call a JS VFS stats callback without deadlocking', async () => {
  // stats() is async because JS VFS callbacks need the main thread to keep pumping promises.
  const sandbox = new Sandbox({ mounts: { workspace: { type: 'custom', vfs: createMemoryVfs() } } })
  await sandbox.fs.writeFile('/workspace/stats.txt', Buffer.from('abc'))
  const stats = await Promise.race([
    sandbox.stats(),
    new Promise((_, reject) => setTimeout(() => reject(new Error('deadlocked')), 1000))
  ])
  assert.deepEqual(stats.vfs, { usedBytes: 3, fileCount: 1 })
})

test('Node e2e pipeline can cat into the sandboxed js command', async () => {
  // Direct host writes and the built-in Wasmtime QuickJS command share the same VFS.
  const sandbox = new Sandbox()
  await sandbox.fs.writeFile('/workspace/x', Buffer.from('abc'))
  await sandbox.fs.writeFile('/workspace/t.js', Buffer.from('const fs = require("fs")\nconsole.log(fs.readFileSync("/workspace/x", "utf8").toUpperCase())\n'))
  const result = await sandbox.exec('cat /workspace/x | js /workspace/t.js')
  assert.equal(result.exitCode, 0)
  assert.equal(result.stdout, 'ABC\n')
})

test('JS globals round-trip JSON values through Node handlers', async () => {
  // Bound globals synchronously call the async Node host callback.
  const sandbox = new Sandbox({
    globals: {
      inspect: async (args) => {
        assert.deepEqual(args, { value: 7 })
        return { doubled: args.value * 2, nested: [true, null, 'ok'] }
      }
    }
  })
  const result = await sandbox.exec("js -e 'const out = inspect({ value: 7 }); console.log(JSON.stringify(out))'")
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, '{"doubled":14,"nested":[true,null,"ok"]}\n')
})

test('JS global handler errors preserve string code fields', async () => {
  // Thrown Node errors become guest Error objects with message and optional code.
  const sandbox = new Sandbox({
    globals: {
      deny: () => {
        const err = new Error('denied by host')
        err.code = 'E_DENIED'
        throw err
      }
    }
  })
  const script = `
try {
  deny({ id: 1 })
} catch (err) {
  console.log(err.message)
  console.log(err.code)
}
`
  const result = await sandbox.exec(`js -e ${singleQuote(script)}`)
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, 'denied by host\nE_DENIED\n')
})

test('jsPrelude can wrap a Node-backed global', async () => {
  // The prelude can expose a narrower global and hide the bound one.
  const sandbox = new Sandbox({
    globals: {
      kvGet: ({ key }) => ({ value: key === 'answer' ? 42 : null })
    },
    jsPrelude: 'const bound = globalThis.kvGet; globalThis.getAnswer = () => bound({ key: "answer" }).value; delete globalThis.kvGet'
  })
  const result = await sandbox.exec("js -e 'console.log(getAnswer(), typeof kvGet)'")
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, '42 undefined\n')
})

test('dotted global names build a namespace object', async () => {
  // `tools.a` and `tools.b` share one generated namespace the script can enumerate.
  const sandbox = new Sandbox({
    globals: {
      'tools.a': () => 'a',
      'tools.b': () => 'b',
      search: () => 'top-level'
    }
  })
  const result = await sandbox.exec(
    "js -e 'console.log(search(), tools.a(), tools.b(), Object.keys(tools).join(\",\"))'"
  )
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(result.stdout, 'top-level a b a,b\n')
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

test('globals can change between commands on a live sandbox', async () => {
  // Each `js` command snapshots the registry, so a turn can grant a different
  // tool surface without rebuilding the sandbox.
  const sandbox = new Sandbox({ globals: { whoami: () => 'agent-1' } })
  sandbox.extendJsGlobals({ 'tools.a': () => 'a', 'tools.b': () => 'b' })
  assert.deepEqual(sandbox.jsGlobalNames(), ['tools.a', 'tools.b', 'whoami'])

  const granted = await sandbox.exec("js -e 'console.log(whoami(), tools.a(), tools.b())'")
  assert.equal(granted.exitCode, 0, granted.stderr)
  assert.equal(granted.stdout, 'agent-1 a b\n')

  // replace drops what it does not name, including constructor globals.
  sandbox.replaceJsGlobals({ 'tools.c': () => 'c' })
  assert.deepEqual(sandbox.jsGlobalNames(), ['tools.c'])
  const revoked = await sandbox.exec("js -e 'console.log(typeof whoami, tools.c())'")
  assert.equal(revoked.stdout, 'undefined c\n')

  sandbox.setJsGlobal('search', () => 'hit')
  assert.equal((await sandbox.exec("js -e 'console.log(search())'")).stdout, 'hit\n')
  assert.equal(sandbox.removeJsGlobal('search'), true)
  assert.equal(sandbox.removeJsGlobal('search'), false)

  // A rejected change leaves the live surface untouched.
  assert.throws(() => sandbox.setJsGlobal('console', () => null), /reserved name/)
  assert.throws(() => sandbox.extendJsGlobals({ tools: () => null }), /conflicts with/)
  assert.deepEqual(sandbox.jsGlobalNames(), ['tools.c'])
})

test('invalid global names throw during Sandbox construction', () => {
  // The core validates, so its rules reach JavaScript as errors rather than as
  // a builder panic crossing N-API.
  assert.throws(() => new Sandbox({ globals: { 'bad-name': () => null } }), /invalid name/)
  assert.throws(() => new Sandbox({ globals: { 'tools..a': () => null } }), /invalid name/)
  assert.throws(() => new Sandbox({ globals: { console: () => null } }), /reserved name/)
  assert.throws(() => new Sandbox({ globals: { 'process.exit': () => null } }), /reserved name/)
  assert.throws(
    () => new Sandbox({ globals: { tools: () => null, 'tools.search': () => null } }),
    /conflicts with/
  )
  assert.throws(() => new Sandbox({ globals: { ok: 'not a function' } }), /must be a function/)
})

test('direct VFS calls proceed while exec is in flight', async () => {
  // The command bridge must not monopolize the JS event loop while an exec awaits.
  const sandbox = new Sandbox({
    commands: {
      wait: async () => new Promise((resolve) => setTimeout(() => resolve({ stdout: Buffer.from('done\n') }), 50))
    }
  })
  const running = sandbox.exec('wait')
  await sandbox.fs.writeFile('/workspace/during.txt', Buffer.from('ok'))
  assert.equal(String(await sandbox.fs.readFile('/workspace/during.txt')), 'ok')
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
  const sandbox = new Sandbox({ mounts: { workspace: { type: 'custom', vfs } } })
  await sandbox.fs.writeFile('/workspace/x', Buffer.from('deadlock-free'))
  const result = await Promise.race([
    sandbox.exec('cat /workspace/x'),
    new Promise((_, reject) => setTimeout(() => reject(new Error('deadlocked')), 1000))
  ])
  assert.equal(result.stdout, 'deadlock-free')
})

test('unknown JS VFS error text collapses to EINVAL instead of substring matching', async () => {
  const vfs = createMemoryVfs()
  vfs.stat = async () => {
    throw new Error('file ENOENT-ish not really')
  }
  const sandbox = new Sandbox({ mounts: { workspace: { type: 'custom', vfs } } })
  await assert.rejects(
    () => sandbox.fs.stat('/workspace/x'),
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
  const sandbox = new Sandbox({ mounts: { workspace: { type: 'custom', vfs } } })
  await sandbox.fs.writeFile('/workspace/buffer.txt', Buffer.from('buffered'))
  assert.equal(String(await sandbox.fs.readFile('/workspace/buffer.txt')), 'buffered')
})

test('JS VFS adapters do not keep child processes alive', async () => {
  const script = `
    import { Sandbox } from './index.js'
    import { createMemoryVfs } from './__test__/helpers.mjs'
    const sandbox = new Sandbox({ mounts: { workspace: { type: 'custom', vfs: createMemoryVfs() } } })
    await sandbox.fs.writeFile('/workspace/x', Buffer.from('ok'))
    const result = await sandbox.exec('cat /workspace/x')
    if (result.stdout !== 'ok') process.exit(2)
  `

  const child = spawn(process.execPath, ['--import', 'tsx', '--input-type=module', '-e', script], {
    cwd: packageRoot,
    stdio: ['ignore', 'pipe', 'pipe']
  })
  const result = await waitForChild(child, 2000)
  assert.equal(result.code, 0, result.stderr)
})

test('the js runtime runs on machine code built ahead of time', async () => {
  // The native package embeds a precompiled artifact, so no process compiles
  // the QuickJS module on its first `js` command.
  const sandbox = new Sandbox()
  const result = await sandbox.exec("js -e 'console.log(1)'")
  assert.equal(result.exitCode, 0, result.stderr)
  assert.equal(jsRuntimeSource(), 'precompiled')
})

test('prompt chunks map to the matching native constants', async () => {
  // Spot-checks distinctive content per chunk so a crossed mapping in
  // index.js (e.g. prompts.shell pointing at the builtins text) is caught.
  const distinctive = {
    overview: 'virtual filesystem',
    shell: 'command substitution',
    builtins: 'GNU counterparts',
    jq: '--argjson',
    js: 'readFileSync',
    globals: 'bound as globals',
    fetch: 'WHATWG',
    sessionEphemeral: 'do not carry over',
    sessionPersistent: 'persist across'
  }
  assert.deepEqual(Object.keys(prompts).sort(), Object.keys(distinctive).sort())
  for (const [key, marker] of Object.entries(distinctive)) {
    const chunk = typeof prompts[key] === 'function' ? prompts[key](['search']) : prompts[key]
    assert.ok(chunk.includes(marker), `prompts.${key} should mention '${marker}'`)
  }
  assert.ok(prompts.globals(['kv.get']).includes('`kv.get(args)`'))

  // The builtins chunk must list exactly what `ls /bin` reports, minus `js`
  // which is introduced by the js chunk.
  const listing = (await new Sandbox().exec('ls /bin')).stdout
  const registered = listing.split('\n').filter((name) => name && name !== 'js')
  const commandLine = prompts.builtins.split('\n').find((line) => line.startsWith('cat '))
  assert.deepEqual(commandLine.split(/\s+/), registered)
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

test('host callbacks have bounded buffered input and output', async () => {
  let calls = 0
  const sandbox = new Sandbox({
    limits: { hostInputBytes: 4 },
    commands: {
      inspect: () => { calls++; return { stdout: 'ok' } },
      large: () => ({ stdout: '12345' })
    }
  })
  const oversized = await sandbox.exec('echo 12345 | inspect')
  assert.notEqual(oversized.exitCode, 0)
  assert.match(oversized.stderr, /input limit/)
  assert.equal(calls, 0)
  assert.equal((await sandbox.exec('echo 123 | inspect')).stdout, 'ok')
  assert.equal(calls, 1)
  assert.match((await sandbox.exec('large')).stderr, /output limit/)
})

test('disabled commands are absent from execution and bin', async () => {
  const sandbox = new Sandbox({ disabledCommands: ['jq', 'cd'] })
  assert.equal((await sandbox.exec("jq -n '1'")).exitCode, 127)
  assert.equal((await sandbox.exec('cd /')).exitCode, 127)
  assert.doesNotMatch((await sandbox.exec('ls /bin')).stdout, /\bjq\b|\bcd\b/)
})

test('host descriptors survive distinct fs facade calls and large reads fail before allocation', async () => {
  const sandbox = new Sandbox()
  await sandbox.fs.writeFile('/workspace/f', Buffer.from('abcdef'))
  const fd = await sandbox.fs.open('/workspace/f', { read: true })
  await sandbox.fs.rename('/workspace/f', '/workspace/g')
  await sandbox.fs.writeFile('/workspace/f', Buffer.alloc(0))
  assert.equal((await sandbox.fs.readAt(fd, 0, 6)).toString(), 'abcdef')
  await assert.rejects(() => sandbox.fs.readAt(fd, 0, 1024 ** 3), { code: 'EFBIG' })
  await sandbox.fs.close(fd)
  assert.throws(() => new Sandbox({ limits: { wallTimeMs: Number.MAX_VALUE } }), /wallTimeMs/)
})
