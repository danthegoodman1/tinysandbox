import test from 'node:test'
import assert from 'node:assert/strict'
import { mkdir, mkdtemp, readFile, rm, symlink, writeFile } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { Sandbox } from '../index.js'

const isUnix = process.platform !== 'win32'

async function withScratchDir(fn) {
  const parent = await mkdtemp(join(tmpdir(), 'tinysandbox-localvfs-'))
  const root = join(parent, 'root')
  await mkdir(root)
  try {
    return await fn(root, parent)
  } finally {
    await rm(parent, { recursive: true, force: true })
  }
}

test('localVfs persists sandbox writes to the host directory', { skip: !isUnix }, async () => {
  await withScratchDir(async (root) => {
    await writeFile(join(root, 'seeded.txt'), 'from host')
    const sandbox = new Sandbox({ localVfs: { root } })

    const seeded = await sandbox.exec('cat /seeded.txt')
    assert.equal(seeded.exitCode, 0, seeded.stderr)
    assert.equal(seeded.stdout, 'from host')

    const result = await sandbox.exec('mkdir /work && echo hello > /work/out.txt')
    assert.equal(result.exitCode, 0, result.stderr)
    assert.equal(await readFile(join(root, 'work', 'out.txt'), 'utf8'), 'hello\n')
  })
})

test('localVfs clamps traversal at the root and refuses symlinks', { skip: !isUnix }, async () => {
  await withScratchDir(async (root, parent) => {
    await writeFile(join(parent, 'secret.txt'), 'top secret')
    await symlink(join(parent, 'secret.txt'), join(root, 'link'))
    const sandbox = new Sandbox({ localVfs: { root } })

    // "/../secret.txt" resolves inside the root, not to the sibling file.
    const traversal = await sandbox.exec('cat /../secret.txt')
    assert.notEqual(traversal.exitCode, 0)

    const escape = await sandbox.exec('echo contained > /../escape.txt')
    assert.equal(escape.exitCode, 0, escape.stderr)
    assert.equal(await readFile(join(root, 'escape.txt'), 'utf8'), 'contained\n')
    assert.ok(!existsSync(join(parent, 'escape.txt')))

    // Symlinks are invisible: unreadable, and writes never follow them.
    const readLink = await sandbox.exec('cat /link')
    assert.notEqual(readLink.exitCode, 0)
    const writeLink = await sandbox.exec('echo clobber > /link')
    assert.notEqual(writeLink.exitCode, 0)
    assert.equal(await readFile(join(parent, 'secret.txt'), 'utf8'), 'top secret')
  })
})

test('localVfs enforces quota and reports stats', { skip: !isUnix }, async () => {
  await withScratchDir(async (root) => {
    const sandbox = new Sandbox({ localVfs: { root, quota: { maxBytes: 8 } } })

    await sandbox.fs.writeFile('/data.bin', Buffer.alloc(8, 1))
    await assert.rejects(
      () => sandbox.fs.appendFile('/data.bin', Buffer.from('x')),
      (err) => {
        assert.equal(err.code, 'ENOSPC')
        return true
      }
    )

    const stats = await sandbox.stats()
    assert.equal(stats.vfs.usedBytes, 8)
    assert.equal(stats.vfs.fileCount, 1)
  })
})

test('localVfs usage can be refreshed and pushed from the host', { skip: !isUnix }, async () => {
  await withScratchDir(async (root) => {
    const sandbox = new Sandbox({ localVfs: { root, quota: { maxBytes: 16 } } })
    await sandbox.fs.writeFile('/a.bin', Buffer.alloc(4, 1))

    // The host mutates the tree behind the sandbox's back...
    await writeFile(join(root, 'external.bin'), Buffer.alloc(6, 2))
    let stats = await sandbox.stats()
    assert.equal(stats.vfs.usedBytes, 4)

    // ...and reconciles with a rescan.
    const refreshed = await sandbox.refreshLocalVfs()
    assert.equal(refreshed.usedBytes, 10)
    assert.equal(refreshed.fileCount, 2)
    stats = await sandbox.stats()
    assert.equal(stats.vfs.usedBytes, 10)

    // Pushed usage becomes the enforcement baseline.
    sandbox.setLocalVfsUsage({ usedBytes: 16, fileCount: 2 })
    await assert.rejects(
      () => sandbox.fs.appendFile('/a.bin', Buffer.from('x')),
      (err) => {
        assert.equal(err.code, 'ENOSPC')
        return true
      }
    )
    sandbox.setLocalVfsUsage({ usedBytes: 10, fileCount: 2 })
    await sandbox.fs.appendFile('/a.bin', Buffer.from('x'))

    assert.throws(() => sandbox.setLocalVfsUsage({ usedBytes: -1, fileCount: 0 }), /usedBytes/)

    const plain = new Sandbox()
    await assert.rejects(() => plain.refreshLocalVfs(), /localVfs/)
    assert.throws(() => plain.setLocalVfsUsage({ usedBytes: 0, fileCount: 0 }), /localVfs/)
  })
})

test('localVfs rejects invalid configuration', { skip: !isUnix }, async () => {
  await withScratchDir(async (root) => {
    assert.throws(() => new Sandbox({ localVfs: { root: join(root, 'missing') } }), /localVfs root/)
    assert.throws(
      () => new Sandbox({ localVfs: { root, quota: { maxBytes: -1 } } }),
      /non-negative safe integer/
    )
    const stubVfs = Object.fromEntries(
      ['stat', 'readdir', 'mkdir', 'rename', 'unlink', 'rmdir', 'open', 'readAt', 'writeAt', 'truncate', 'close'].map(
        (name) => [name, async () => ({})]
      )
    )
    assert.throws(() => new Sandbox({ vfs: stubVfs, localVfs: { root } }), /either vfs or localVfs/)
  })
})
