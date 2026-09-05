import { Sandbox, type HostContext, type JsFetch, type JsGlobal } from '../index.js'

const legacyGlobal: JsGlobal = (value) => ({ value: String(value) })
const legacyFetch: JsFetch = (request) => ({ status: 200, body: request.url })
const contextualGlobal: JsGlobal = (_value, context: HostContext) => {
  const signal: AbortSignal = context.signal
  const deadline: number | null = context.deadlineMs
  const remaining: number | null = context.remainingTimeMs()
  return { aborted: signal.aborted, cancelled: context.isCancelled(), deadline, remaining }
}

const sandbox = new Sandbox({
  limits: { jqMemoryBytes: 64 * 1024 * 1024 },
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
const read: Promise<Buffer> = sandbox.fs.readFile('/workspace/hello')
void execution
void read
