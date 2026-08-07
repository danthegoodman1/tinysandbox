#!/usr/bin/env node
import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { dirname, join, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import { fileURLToPath } from 'node:url'

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const nodePackageRoot = join(repoRoot, 'tinysandbox-node')

export const nativeTargets = [
  {
    triple: 'aarch64-apple-darwin',
    target: 'darwin-arm64',
    os: ['darwin'],
    cpu: ['arm64']
  },
  {
    triple: 'x86_64-apple-darwin',
    target: 'darwin-x64',
    os: ['darwin'],
    cpu: ['x64']
  },
  {
    triple: 'aarch64-unknown-linux-gnu',
    target: 'linux-arm64-gnu',
    os: ['linux'],
    cpu: ['arm64'],
    libc: ['glibc']
  },
  {
    triple: 'x86_64-unknown-linux-gnu',
    target: 'linux-x64-gnu',
    os: ['linux'],
    cpu: ['x64'],
    libc: ['glibc']
  }
]

export function verifyNativePackages({ requireBinaries = false } = {}) {
  const mainPackage = readJson(join(nodePackageRoot, 'package.json'))
  assert.equal(mainPackage.name, '@tinysandbox/tinysandbox')
  assert.equal(mainPackage.files.includes('*.node'), false, 'main package must not include native binaries')
  assert.deepEqual(mainPackage.napi?.targets, nativeTargets.map(({ triple }) => triple))

  const expectedOptionalDependencies = Object.fromEntries(
    nativeTargets.map(({ target }) => [`@tinysandbox/tinysandbox-${target}`, mainPackage.version])
  )
  assert.deepEqual(mainPackage.optionalDependencies, expectedOptionalDependencies)

  const mainPackFiles = packFiles(nodePackageRoot)
  assert.equal(
    mainPackFiles.some((file) => file.endsWith('.node')),
    false,
    'main npm tarball must contain no native binaries'
  )

  for (const metadata of nativeTargets) {
    const packageDir = join(nodePackageRoot, 'npm', metadata.target)
    const packageJson = readJson(join(packageDir, 'package.json'))
    const binary = `tinysandbox-node.${metadata.target}.node`

    assert.equal(packageJson.name, `@tinysandbox/tinysandbox-${metadata.target}`)
    assert.equal(packageJson.version, mainPackage.version)
    assert.equal(packageJson.main, binary)
    assert.deepEqual(packageJson.files, [binary])
    assert.deepEqual(packageJson.os, metadata.os)
    assert.deepEqual(packageJson.cpu, metadata.cpu)
    assert.deepEqual(packageJson.libc, metadata.libc)

    if (requireBinaries) {
      const binaries = readdirSync(packageDir).filter((file) => file.endsWith('.node'))
      assert.deepEqual(binaries, [binary])
      assert.deepEqual(
        packFiles(packageDir).filter((file) => file.endsWith('.node')),
        [binary]
      )
    }
  }
}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function packFiles(packageDir) {
  const result = spawnSync('npm', ['pack', '--dry-run', '--json'], {
    cwd: packageDir,
    encoding: 'utf8',
    env: {
      ...process.env,
      npm_config_cache: process.env.npm_config_cache ?? join(tmpdir(), 'tinysandbox-npm-cache')
    }
  })
  assert.equal(result.status, 0, result.stderr)
  return JSON.parse(result.stdout)[0].files.map(({ path }) => path).sort()
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  verifyNativePackages({ requireBinaries: process.argv.includes('--require-binaries') })
  console.log('native npm packages verified')
}
