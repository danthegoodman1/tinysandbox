import { Sandbox } from '../index.js'

// A base capability every turn keeps, defined once so the same handler can be
// re-granted after a revoking swap.
const whoami = () => 'agent-1'

async function main() {
  const sandbox = new Sandbox({ globals: { whoami } })
  console.log('base:   ', sandbox.jsGlobalNames())

  // Turn one adds tools and keeps everything already bound.
  sandbox.extendJsGlobals({
    'tools.search': (args) => {
      const { q } = args as { q: string }
      return { hits: [`hit for ${q}`] }
    },
    'tools.read_doc': () => 'doc body'
  })
  console.log('turn 1: ', sandbox.jsGlobalNames())
  const result = await sandbox.exec(
    `js -e 'console.log(whoami(), tools.search({ q: "vfs" }).hits[0])'`
  )
  process.stdout.write(`call:    ${result.stdout}`)

  // Turn two revokes turn one. replaceJsGlobals swaps the whole surface, so the
  // base capability is re-granted alongside the new tool.
  sandbox.replaceJsGlobals({
    whoami,
    'tools.write_note': (args) => {
      const { text } = args as { text?: string }
      return { written: (text ?? '').length }
    }
  })
  console.log('turn 2: ', sandbox.jsGlobalNames())
  const revoked = await sandbox.exec("js -e 'console.log(typeof tools.search)'")
  process.stdout.write(`revoked: ${revoked.stdout}`)

  // Single names can be added and dropped without touching the rest.
  sandbox.setJsGlobal('tools.trace', () => 'traced')
  console.log('added:  ', sandbox.jsGlobalNames())
  console.log('removed:', sandbox.removeJsGlobal('tools.trace'))
  console.log('again:  ', sandbox.removeJsGlobal('tools.trace'))

  // A rejected change leaves the surface exactly as it was.
  for (const attempt of [
    () => sandbox.setJsGlobal('console', () => null),
    () => sandbox.extendJsGlobals({ tools: () => null })
  ]) {
    try {
      attempt()
    } catch (err) {
      console.log('refused:', (err as Error).message)
    }
  }
  console.log('bound:  ', sandbox.jsGlobalNames())
}

main().catch((err) => {
  console.error(err)
  process.exitCode = 1
})
