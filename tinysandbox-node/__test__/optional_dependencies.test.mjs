import assert from 'node:assert/strict'
import { mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { spawnSync } from 'node:child_process'
import { test } from 'node:test'

const targets = ['darwin-arm64', 'darwin-x64', 'linux-arm64-gnu', 'linux-x64-gnu']

function currentTarget() {
  if (process.platform === 'darwin' && ['arm64', 'x64'].includes(process.arch)) {
    return `darwin-${process.arch}`
  }
  if (process.platform === 'linux' && ['arm64', 'x64'].includes(process.arch)) {
    return `linux-${process.arch}-gnu`
  }
  throw new Error(`unsupported test platform ${process.platform}-${process.arch}`)
}

test('npm installs only the compatible optional native package', (t) => {
  const tempDir = mkdtempSync(join(tmpdir(), 'tinysandbox-install-'))
  t.after(() => rmSync(tempDir, { recursive: true, force: true }))

  const packageJson = {
    name: 'tinysandbox-install-probe',
    version: '0.0.0',
    private: true,
    optionalDependencies: Object.fromEntries(
      targets.map((target) => [
        `@tinysandbox/tinysandbox-${target}`,
        new URL(`../npm/${target}`, import.meta.url).pathname
      ])
    )
  }
  writeFileSync(join(tempDir, 'package.json'), `${JSON.stringify(packageJson, null, 2)}\n`)

  const install = spawnSync('npm', ['install', '--ignore-scripts', '--package-lock=false'], {
    cwd: tempDir,
    encoding: 'utf8',
    env: {
      ...process.env,
      npm_config_cache: join(tempDir, '.npm-cache')
    }
  })
  assert.equal(install.status, 0, `${install.stdout}\n${install.stderr}`)

  const installed = readdirSync(join(tempDir, 'node_modules', '@tinysandbox')).sort()
  assert.deepEqual(installed, [`tinysandbox-${currentTarget()}`])

  const installedPackage = JSON.parse(
    readFileSync(join(tempDir, 'node_modules', '@tinysandbox', installed[0], 'package.json'), 'utf8')
  )
  assert.equal(installedPackage.name, `@tinysandbox/tinysandbox-${currentTarget()}`)
})
