'use strict'

const { readFileSync } = require('node:fs')

const packageVersion = '0.6.5'
const loadErrors = []

function isMusl() {
  if (process.platform !== 'linux') return false

  try {
    if (readFileSync('/usr/bin/ldd', 'utf8').includes('musl')) return true
  } catch {
    // Continue with Node's runtime report.
  }

  const report = typeof process.report?.getReport === 'function'
    ? process.report.getReport()
    : undefined
  if (report?.header?.glibcVersionRuntime) return false
  if (report?.sharedObjects?.some((file) => file.includes('libc.musl-') || file.includes('ld-musl-'))) {
    return true
  }

  return false
}

function targetFor(platform, arch) {
  if (platform === 'darwin' && (arch === 'arm64' || arch === 'x64')) {
    return `darwin-${arch}`
  }
  if (platform === 'linux' && (arch === 'arm64' || arch === 'x64')) {
    if (isMusl()) {
      throw new Error('Unsupported Linux libc: tinysandbox currently requires glibc')
    }
    return `linux-${arch}-gnu`
  }
  throw new Error(`Unsupported platform: ${platform}-${arch}`)
}

function tryRequire(id) {
  try {
    return require(id)
  } catch (error) {
    loadErrors.push(error)
    return undefined
  }
}

function loadNative() {
  if (process.env.NAPI_RS_NATIVE_LIBRARY_PATH) {
    const override = tryRequire(process.env.NAPI_RS_NATIVE_LIBRARY_PATH)
    if (override) return override
  }

  const target = targetFor(process.platform, process.arch)
  const localBinding = tryRequire(`./tinysandbox-node.${target}.node`)
  if (localBinding) return localBinding

  const nativePackage = `@tinysandbox/tinysandbox-${target}`
  const packagedBinding = tryRequire(nativePackage)
  if (packagedBinding) {
    if (process.env.NAPI_RS_ENFORCE_VERSION_CHECK && process.env.NAPI_RS_ENFORCE_VERSION_CHECK !== '0') {
      const actualVersion = require(`${nativePackage}/package.json`).version
      if (actualVersion !== packageVersion) {
        throw new Error(
          `Native binding version mismatch: expected ${packageVersion}, got ${actualVersion}`
        )
      }
    }
    return packagedBinding
  }

  const reasons = loadErrors
    .map((error) => error instanceof Error ? error.message : String(error))
    .join('\n- ')
  throw new Error(
    `Cannot load the native binding for ${target}. Reinstall @tinysandbox/tinysandbox ` +
    `with optional dependencies enabled.${reasons ? `\n- ${reasons}` : ''}`
  )
}

module.exports = loadNative()
