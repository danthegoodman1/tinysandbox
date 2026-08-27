import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { Sandbox, jsRuntimeSource, precompileJs, usePrecompiledJs } from '../index.js'

async function main() {
  // The native package embeds machine code for its platform, so the first `js`
  // command loads it instead of compiling the module (~400 ms of Cranelift).
  const sandbox = new Sandbox({
    globals: { whoami: () => 'agent-1', 'tools.answer': () => 42 }
  })
  const execStart = process.hrtime.bigint()
  const result = await sandbox.exec("js -e 'console.log(whoami(), tools.answer())'")
  console.log(
    `first js exec: ${elapsedMs(execStart)} ms from ${jsRuntimeSource()} machine code -> ${result.stdout.trim()}`
  )
  console.assert(result.exitCode === 0, result.stderr)
  console.assert(result.stdout === 'agent-1 42\n')

  // Producing an artifact by hand is for targeting another machine, or sharing
  // one across processes that build separately.
  const artifactPath = join(mkdtempSync(join(tmpdir(), 'tinysandbox-')), 'quickjs.cwasm')
  const compileStart = process.hrtime.bigint()
  const artifact = precompileJs()
  writeFileSync(artifactPath, artifact)
  console.log(`precompiled quickjs in ${elapsedMs(compileStart)} ms, ${artifact.length} bytes`)

  // Installing it only works before the first `js` command, which already ran
  // above, so this reports why it declined rather than throwing.
  try {
    usePrecompiledJs(readFileSync(artifactPath))
    console.log('installed the artifact')
  } catch (err) {
    console.log(`kept the embedded runtime: ${(err as Error).message}`)
  }
}

function elapsedMs(start: bigint) {
  return Number(process.hrtime.bigint() - start) / 1_000_000
}

main().catch((err) => {
  console.error(err)
  process.exitCode = 1
})
