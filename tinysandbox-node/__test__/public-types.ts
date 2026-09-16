import { Pools, Sandbox, type HostContext, type JsFetch, type JsGlobal, type SandboxFs } from '../index.js'

const legacyGlobal: JsGlobal = (value) => ({ value: String(value) })
const legacyFetch: JsFetch = (request) => ({ status: 200, body: request.url })
const contextualGlobal: JsGlobal = (_value, context: HostContext) => {
  const signal: AbortSignal = context.signal
  const deadline: number | null = context.deadlineMs
  const remaining: number | null = context.remainingTimeMs()
  return { aborted: signal.aborted, cancelled: context.isCancelled(), deadline, remaining }
}

const tenant = new Pools({ jqWorkers: 2, openFiles: 64 })

const sandbox = new Sandbox({
  limits: { jqMemoryBytes: 64 * 1024 * 1024 },
  pools: tenant,
  globals: { legacyGlobal, contextualGlobal },
  fetch: legacyFetch,
  commands: {
    legacy: ({ args }) => ({ stdout: args.join(' ') }),
    contextual: ({ signal, remainingTimeMs }) => ({
      stdout: `${signal.aborted} ${remainingTimeMs()}`
    })
  }
})
sandbox.setJsGlobal('legacyGlobal', legacyGlobal)
sandbox.extendJsGlobals({ contextualGlobal })
sandbox.replaceJsGlobals({ legacyGlobal, contextualGlobal })
const execution: Promise<number> = sandbox.exec('echo hello').then((result) => result.exitCode)
const fs: SandboxFs = sandbox.fs
const read: Promise<Buffer> = fs.readFile('/workspace/hello')
void execution
void read
const abort: Promise<void> = fs.abort(1)
void abort
