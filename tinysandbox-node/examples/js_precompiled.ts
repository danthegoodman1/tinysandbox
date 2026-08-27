import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { Sandbox, precompileJs, usePrecompiledJs } from '../index.js'

async function main() {
  const artifactPath = join(mkdtempSync(join(tmpdir(), 'tinysandbox-')), 'quickjs.cwasm')

  // Build step: Cranelift compiles the embedded QuickJS module once. Without
  // this, the first `js` command in every process pays for it (~400 ms).
  const compileStart = process.hrtime.bigint()
  const artifact = precompileJs()
  writeFileSync(artifactPath, artifact)
  console.log(
    `build: compiled quickjs in ${elapsedMs(compileStart)} ms, wrote ${artifact.length} bytes`
  )

  // Load step: normally this happens in a later process, before its first `js`
  // command. A stale or foreign artifact throws, and the normal compile path
  // still works, so falling back is safe.
  const loadStart = process.hrtime.bigint()
  try {
    usePrecompiledJs(readFileSync(artifactPath))
    console.log(`run: loaded artifact in ${elapsedMs(loadStart)} ms`)
  } catch (err) {
    console.log(`run: falling back to compiling quickjs: ${(err as Error).message}`)
  }

  const sandbox = new Sandbox()
  const execStart = process.hrtime.bigint()
  const result = await sandbox.exec("js -e 'console.log(6 * 7)'")
  console.log(`run: first js exec took ${elapsedMs(execStart)} ms -> ${result.stdout.trim()}`)
  console.assert(result.exitCode === 0, result.stderr)
  console.assert(result.stdout === '42\n')
}

function elapsedMs(start: bigint) {
  return Number(process.hrtime.bigint() - start) / 1_000_000
}

main().catch((err) => {
  console.error(err)
  process.exitCode = 1
})
