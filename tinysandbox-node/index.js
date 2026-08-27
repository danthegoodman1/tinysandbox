const native = require('./native.cjs')

class Sandbox extends native.NativeSandbox {
  constructor(options = undefined) {
    super(normalizeOptions(options))
    this._fs = undefined
  }

  get fs() {
    this._fs ??= new SandboxFs(super.fs)
    return this._fs
  }
}

async function runConformance(vfsFactory) {
  return native.runConformance(async (quota) => native.createJsVfs(wrapVfs(await vfsFactory(firstArgument(quota)))))
}

function normalizeOptions(options) {
  if (!options) return options
  const normalized = { ...options }
  if (options.commands) normalized.commands = wrapCommands(options.commands)
  else delete normalized.commands
  if (options.globals) normalized.globals = wrapGlobals(options.globals)
  else delete normalized.globals
  if (options.fetch) normalized.fetch = wrapFetch(options.fetch)
  else delete normalized.fetch
  if (options.mounts) normalized.mounts = wrapMounts(options.mounts)
  else delete normalized.mounts
  return normalized
}

function wrapMounts(mounts) {
  return Object.fromEntries(
    Object.entries(mounts).map(([name, mount]) => [
      name,
      mount?.type === 'custom' ? { ...mount, vfs: wrapVfs(mount.vfs) } : mount
    ])
  )
}

function wrapCommands(commands) {
  return Object.fromEntries(
    Object.entries(commands).map(([name, command]) => [
      name,
      async (call) => {
        try {
          const payload = firstArgument(call)
          return normalizeCommandOutput(await command(payload))
        } catch (err) {
          return {
            exitCode: 1,
            stderr: Buffer.from(`${err?.message ?? err}\n`)
          }
        }
      }
    ])
  )
}

function wrapGlobals(globals) {
  return Object.fromEntries(
    Object.entries(globals).map(([name, global]) => {
      // Names are validated by the native layer, which owns the rules.
      if (typeof global !== 'function') throw new TypeError(`global '${name}' must be a function`)
      return [
        name,
        async (args) => {
          try {
            return { value: normalizeJsonValue(await global(firstArgument(args))) }
          } catch (err) {
            return { error: callbackErrorPayload(err) }
          }
        }
      ]
    })
  )
}

function wrapFetch(fetch) {
  if (typeof fetch !== 'function') throw new TypeError('fetch must be a function')
  return async (request) => {
    try {
      return { response: normalizeFetchResponse(await fetch(normalizeFetchRequest(firstArgument(request)))) }
    } catch (err) {
      return { error: callbackErrorPayload(err) }
    }
  }
}

function wrapVfs(vfs) {
  return Object.fromEntries(
    vfsOperations.map((name) => [
      name,
      async (request) => {
        try {
          return normalizeVfsResponse(name, await vfs[name](normalizeVfsRequest(request)))
        } catch (err) {
          return { error: errorPayload(err) }
        }
      }
    ]).concat(
      typeof vfs.stats === 'function'
        ? [[
            'stats',
            async (request) => {
              try {
                return normalizeVfsResponse('stats', await vfs.stats(normalizeVfsRequest(request)))
              } catch (err) {
                return { error: errorPayload(err) }
              }
            }
          ]]
        : []
    )
  )
}

function normalizeVfsRequest(request) {
  return firstArgument(request)
}

function firstArgument(value) {
  // napi-rs TSFN callbacks marshal tuple arguments as an object with numeric
  // keys, while direct wrapper calls already pass the request object.
  return value && typeof value === 'object' && Object.hasOwn(value, '0') ? value[0] : value
}

function normalizeJsonValue(value) {
  validateJsonValue(value, new Set())
  return JSON.parse(JSON.stringify(value))
}

function validateJsonValue(value, seen) {
  if (value === null) return
  if (typeof value === 'string' || typeof value === 'boolean') return
  if (typeof value === 'number') {
    if (Number.isFinite(value)) return
    throw new TypeError('host global return value must contain only finite numbers')
  }
  if (Array.isArray(value)) {
    if (seen.has(value)) throw new TypeError('host global return value must not contain cycles')
    seen.add(value)
    for (const item of value) validateJsonValue(item, seen)
    seen.delete(value)
    return
  }
  if (typeof value === 'object') {
    const proto = Object.getPrototypeOf(value)
    if (proto !== Object.prototype && proto !== null) {
      throw new TypeError('host global return value must contain only JSON objects, arrays, and scalars')
    }
    if (seen.has(value)) throw new TypeError('host global return value must not contain cycles')
    seen.add(value)
    for (const item of Object.values(value)) validateJsonValue(item, seen)
    seen.delete(value)
    return
  }
  throw new TypeError('host global return value must be JSON-serializable')
}

function normalizeFetchRequest(request) {
  return {
    url: request.url,
    method: request.method,
    headers: normalizeHeaderPairs(request.headers ?? []),
    body: request.body == null ? null : Buffer.from(request.body)
  }
}

function normalizeFetchResponse(response) {
  if (!response || typeof response !== 'object') {
    throw new TypeError('fetch handler must return a response object')
  }
  return {
    status: response.status,
    headers: response.headers === undefined ? undefined : normalizeHeaderPairs(response.headers),
    body: normalizeFetchBody(response.body)
  }
}

function normalizeHeaderPairs(headers) {
  if (!Array.isArray(headers)) throw new TypeError('headers must be an array of [name, value] pairs')
  return headers.map((pair) => {
    if (!Array.isArray(pair) || pair.length !== 2) {
      throw new TypeError('headers must be an array of [name, value] pairs')
    }
    const [name, value] = pair
    if (typeof name !== 'string' || typeof value !== 'string') {
      throw new TypeError('header names and values must be strings')
    }
    return [name, value]
  })
}

function normalizeFetchBody(body) {
  if (body === undefined || body === null) return undefined
  if (typeof body === 'string') return Buffer.from(body, 'utf8')
  return Buffer.from(body)
}

function normalizeCommandOutput(output = {}) {
  return {
    exitCode: output.exitCode ?? 0,
    stdout: output.stdout ? Buffer.from(output.stdout) : undefined,
    stderr: output.stderr ? Buffer.from(output.stderr) : undefined
  }
}

function normalizeVfsResponse(name, response) {
  if (response === undefined || response === null) return {}
  if (response.error) return response
  if (Buffer.isBuffer(response)) return { data: response, bytesRead: response.length }
  if (Array.isArray(response)) return { entries: response }
  if (typeof response === 'number') {
    if (name === 'open') return { handle: response }
    if (name === 'writeAt') return { bytesWritten: response }
  }
  return response
}

function errorPayload(err) {
  return {
    code: normalizeErrnoCode(err?.code),
    message: err?.message ?? String(err)
  }
}

function callbackErrorPayload(err) {
  return {
    code: typeof err?.code === 'string' ? err.code : undefined,
    message: err?.message ?? String(err)
  }
}

function normalizeErrnoCode(code) {
  return errnos.includes(code) ? code : 'EINVAL'
}

const vfsOperations = [
  'stat',
  'readdir',
  'mkdir',
  'rename',
  'unlink',
  'rmdir',
  'open',
  'readAt',
  'writeAt',
  'truncate',
  'close'
]

const errnos = [
  'EBADF',
  'EBUSY',
  'EXDEV',
  'EACCES',
  'EEXIST',
  'EFBIG',
  'EIO',
  'EINVAL',
  'EISDIR',
  'ENOENT',
  'ENOSPC',
  'ENOTDIR',
  'ENOTEMPTY'
]

class SandboxFs {
  constructor(inner) {
    this.inner = inner
  }

  stat(path) {
    return decorateFsPromise(this.inner.stat(path))
  }

  readdir(path) {
    return decorateFsPromise(this.inner.readdir(path))
  }

  mkdir(path) {
    return decorateFsPromise(this.inner.mkdir(path))
  }

  rename(from, to) {
    return decorateFsPromise(this.inner.rename(from, to))
  }

  unlink(path) {
    return decorateFsPromise(this.inner.unlink(path))
  }

  rmdir(path) {
    return decorateFsPromise(this.inner.rmdir(path))
  }

  readFile(path) {
    return decorateFsPromise(this.inner.readFile(path))
  }

  writeFile(path, data) {
    return decorateFsPromise(this.inner.writeFile(path, data))
  }

  appendFile(path, data) {
    return decorateFsPromise(this.inner.appendFile(path, data))
  }

  open(path, mode) {
    return decorateFsPromise(this.inner.open(path, mode))
  }

  readAt(handle, offset, len) {
    return decorateFsPromise(this.inner.readAt(handle, offset, len))
  }

  writeAt(handle, offset, data) {
    return decorateFsPromise(this.inner.writeAt(handle, offset, data))
  }

  truncate(handle, len) {
    return decorateFsPromise(this.inner.truncate(handle, len))
  }

  close(handle) {
    return decorateFsPromise(this.inner.close(handle))
  }
}

async function decorateFsPromise(promise) {
  try {
    return await promise
  } catch (err) {
    if (!err || typeof err !== 'object') throw err
    err.code = normalizeNativeErrno(err)
    throw err
  }
}

function normalizeNativeErrno(err) {
  if (errnos.includes(err.code)) return err.code
  const [prefix] = String(err.message ?? '').split(':', 1)
  return errnos.includes(prefix) ? prefix : 'EINVAL'
}

const prompts = Object.freeze({
  overview: native.PROMPT_OVERVIEW,
  shell: native.PROMPT_SHELL,
  builtins: native.PROMPT_BUILTINS,
  jq: native.PROMPT_JQ,
  js: native.PROMPT_JS,
  globals: (names = []) => native.promptGlobals([...names]),
  fetch: native.PROMPT_FETCH,
  sessionEphemeral: native.PROMPT_SESSION_EPHEMERAL,
  sessionPersistent: native.PROMPT_SESSION_PERSISTENT
})

exports.precompileJs = () => native.precompileJs()
exports.usePrecompiledJs = (artifact) => native.usePrecompiledJs(artifact)
exports.jsRuntimeSource = () => native.jsRuntimeSource()
exports.NativeSandbox = native.NativeSandbox
exports.SandboxFs = SandboxFs
exports.Sandbox = Sandbox
exports.runConformance = runConformance
exports.prompts = prompts
