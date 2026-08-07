import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'
import vm from 'node:vm'

const loader = readFileSync(new URL('../native.cjs', import.meta.url), 'utf8')

const supportedTargets = [
  { platform: 'darwin', arch: 'arm64', target: 'darwin-arm64' },
  { platform: 'darwin', arch: 'x64', target: 'darwin-x64' },
  { platform: 'linux', arch: 'arm64', target: 'linux-arm64-gnu' },
  { platform: 'linux', arch: 'x64', target: 'linux-x64-gnu' }
]

function evaluateLoader({ platform, arch, musl = false, local = false, override }) {
  const requests = []
  const binding = { marker: `${platform}-${arch}` }
  const target = platform === 'darwin' ? `darwin-${arch}` : `linux-${arch}-gnu`
  const localId = `./tinysandbox-node.${target}.node`
  const packageId = `@tinysandbox/tinysandbox-${target}`

  function fakeRequire(id) {
    requests.push(id)
    if (id === 'node:fs') {
      return { readFileSync: () => musl ? 'musl' : 'glibc' }
    }
    if (override && id === override) return binding
    if (id === localId) {
      if (local) return binding
      throw new Error(`missing ${id}`)
    }
    if (id === packageId) return binding
    throw new Error(`unexpected require ${id}`)
  }

  const module = { exports: {} }
  vm.runInNewContext(loader, {
    Error,
    String,
    module,
    exports: module.exports,
    require: fakeRequire,
    process: {
      platform,
      arch,
      env: override ? { NAPI_RS_NATIVE_LIBRARY_PATH: override } : {},
      report: {
        getReport: () => ({
          header: musl ? {} : { glibcVersionRuntime: '2.39' },
          sharedObjects: musl ? ['/lib/ld-musl-aarch64.so.1'] : []
        })
      }
    }
  })

  return { binding, exported: module.exports, requests, localId, packageId }
}

test('the loader auto-selects exactly one supported native package', () => {
  for (const { platform, arch, target } of supportedTargets) {
    const result = evaluateLoader({ platform, arch })
    assert.equal(result.exported, result.binding)
    assert.deepEqual(result.requests, [
      'node:fs',
      `./tinysandbox-node.${target}.node`,
      `@tinysandbox/tinysandbox-${target}`
    ])
  }
})

test('the loader prefers a local development build', () => {
  const result = evaluateLoader({ platform: 'darwin', arch: 'arm64', local: true })
  assert.equal(result.exported, result.binding)
  assert.deepEqual(result.requests, ['node:fs', result.localId])
})

test('an explicit native library override bypasses platform selection', () => {
  const override = '/tmp/tinysandbox-custom.node'
  const result = evaluateLoader({ platform: 'plan9', arch: 'mips', override })
  assert.equal(result.exported, result.binding)
  assert.deepEqual(result.requests, ['node:fs', override])
})

test('unsupported platforms and musl fail clearly without probing packages', () => {
  assert.throws(
    () => evaluateLoader({ platform: 'win32', arch: 'x64' }),
    /Unsupported platform: win32-x64/
  )
  assert.throws(
    () => evaluateLoader({ platform: 'linux', arch: 'arm64', musl: true }),
    /requires glibc/
  )
})
