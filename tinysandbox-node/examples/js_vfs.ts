import { Sandbox, runConformance } from '../index.js'
import { createMemoryVfs } from './memory_vfs.ts'

async function main() {
  const conformance = await runConformance((quota) => createMemoryVfs(quota))
  console.log('conformance:', conformance)

  const sandbox = new Sandbox({
    mounts: { workspace: { type: 'custom', vfs: createMemoryVfs() } }
  })
  await sandbox.fs.writeFile('/workspace/input.txt', Buffer.from('one\ntwo\nthree\n'))

  const result = await sandbox.exec('cat /workspace/input.txt | wc -l')
  process.stdout.write(result.stdout)
}

main().catch((err) => {
  console.error(err)
  process.exitCode = 1
})
