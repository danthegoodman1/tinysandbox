import { Sandbox, type JsonValue } from '../index.js'

async function main() {
  const store = new Map<string, JsonValue>()

  const sandbox = new Sandbox({
    limits: { fetchResponseBytes: 1024 },
    globals: {
      // A dotted name creates the namespace: scripts call `kv.get(...)`.
      'kv.get': (args) => {
        const { key } = args as { key?: string }
        if (!key) {
          const err = new Error('key is required') as Error & { code?: string }
          err.code = 'E_KEY'
          throw err
        }
        return { value: store.get(key) ?? null }
      },
      'kv.put': (args) => {
        const { key, value } = args as { key: string; value: unknown }
        store.set(key, value as JsonValue)
        return { ok: true }
      },
      // A bare name binds one top-level global: scripts call `whoami()`.
      whoami: () => ({ name: 'agent-1' })
    },
    // The prelude still runs before the script, so host globals can be wrapped
    // in friendlier JavaScript.
    jsPrelude: 'globalThis.kvGet = key => kv.get({ key }).value',
    fetch: async (request) => {
      if (request.url !== 'https://example.test/echo') {
        const err = new Error(`no canned response for ${request.url}`) as Error & { code?: string }
        err.code = 'ENOENT'
        throw err
      }
      return {
        status: 200,
        headers: [['content-type', 'text/plain']],
        body: `echo:${request.body?.toString('utf8') ?? ''}`
      }
    }
  })

  const script = `
kv.put({ key: 'answer', value: 42 })
console.log(\`answer=\${kvGet('answer')} user=\${whoami().name}\`)

try {
  kv.get({})
} catch (err) {
  console.log(\`\${err.code}:\${err.message}\`)
}

(async () => {
  const response = await fetch('https://example.test/echo', {
    method: 'POST',
    body: Buffer.from('ping')
  })
  console.log(\`\${response.status}:\${await response.text()}\`)
})()
`
  await sandbox.fs.writeFile('/workspace/main.js', Buffer.from(script))

  const result = await sandbox.exec('js /workspace/main.js')
  process.stdout.write(result.stdout)
  console.assert(result.exitCode === 0, result.stderr)
  console.assert(result.stdout === 'answer=42 user=agent-1\nE_KEY:key is required\n200:echo:ping\n')
}

main().catch((err) => {
  console.error(err)
  process.exitCode = 1
})
