import assert from 'node:assert/strict'
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { spawnSync } from 'node:child_process'
import { test } from 'node:test'

import { build } from 'esbuild'

const packageRoot = new URL('../', import.meta.url)

function currentTarget() {
  if (process.platform === 'darwin' && ['arm64', 'x64'].includes(process.arch)) {
    return `darwin-${process.arch}`
  }
  if (process.platform === 'linux' && ['arm64', 'x64'].includes(process.arch)) {
    return `linux-${process.arch}-gnu`
  }
  throw new Error(`unsupported test platform ${process.platform}-${process.arch}`)
}

test('the main npm tarball contains no native binaries', () => {
  const result = spawnSync('npm', ['pack', '--dry-run', '--json'], {
    cwd: packageRoot,
    encoding: 'utf8'
  })
  assert.equal(result.status, 0, result.stderr)

  const [pack] = JSON.parse(result.stdout)
  assert.equal(pack.files.some(({ path }) => path.endsWith('.node')), false)
  assert.deepEqual(
    pack.files.map(({ path }) => path).sort(),
    ['README.md', 'index.d.ts', 'index.js', 'native.cjs', 'native.d.ts', 'package.json']
  )
})

test('a bundled facade loads only the auto-detected native package', async (t) => {
  const target = currentTarget()
  const tempDir = mkdtempSync(join(tmpdir(), 'tinysandbox-bundle-'))
  t.after(() => rmSync(tempDir, { recursive: true, force: true }))

  const bundlePath = join(tempDir, 'bundle.cjs')
  await build({
    entryPoints: [new URL('../index.js', import.meta.url).pathname],
    bundle: true,
    format: 'cjs',
    platform: 'node',
    outfile: bundlePath
  })

  const packageName = `@tinysandbox/tinysandbox-${target}`
  const nativeFilename = `tinysandbox-node.${target}.node`
  const nativePackageDir = join(tempDir, 'node_modules', '@tinysandbox', `tinysandbox-${target}`)
  mkdirSync(nativePackageDir, { recursive: true })
  copyFileSync(new URL(`../${nativeFilename}`, import.meta.url), join(nativePackageDir, nativeFilename))
  writeFileSync(
    join(nativePackageDir, 'package.json'),
    `${JSON.stringify({ name: packageName, version: '0.0.0-test', main: nativeFilename }, null, 2)}\n`
  )

  const probe = spawnSync(
    process.execPath,
    ['-e', `const api = require(${JSON.stringify(bundlePath)}); if (typeof api.Sandbox !== 'function') process.exit(2)`],
    { cwd: tempDir, encoding: 'utf8' }
  )
  assert.equal(probe.status, 0, `${probe.stdout}\n${probe.stderr}`)

  const bundle = readFileSync(bundlePath, 'utf8')
  for (const unsupportedTarget of [
    'darwin-arm64',
    'darwin-x64',
    'linux-arm64-gnu',
    'linux-x64-gnu'
  ].filter((candidate) => candidate !== target)) {
    assert.doesNotMatch(bundle, new RegExp(`tinysandbox-${unsupportedTarget}`))
  }
})
