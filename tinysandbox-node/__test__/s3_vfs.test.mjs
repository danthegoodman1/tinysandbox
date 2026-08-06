import test from 'node:test'
import assert from 'node:assert/strict'
import { Sandbox } from '../index.js'

const endpoint = process.env.TINYSANDBOX_S3_TEST_ENDPOINT
const live = endpoint !== undefined

function requiredEnv(name) {
  const value = process.env[name]
  assert.ok(value, `${name} must be set by scripts/test-s3-compat.sh`)
  return value
}

function isAllowedLoopbackEndpoint(value) {
  if (typeof value !== 'string' || !/^http:\/\/(127\.0\.0\.1|localhost):[0-9]+$/.test(value)) return false
  const url = new URL(value)
  const port = Number(url.port)
  return url.protocol === 'http:' &&
    (url.hostname === '127.0.0.1' || url.hostname === 'localhost') &&
    Number.isInteger(port) && port >= 1 && port <= 65535
}

function requireLoopbackEndpoint() {
  assert.ok(isAllowedLoopbackEndpoint(endpoint), 'S3 compatibility endpoint must be exactly http://127.0.0.1:<port> or http://localhost:<port>')
  return endpoint
}

function liveOptions(prefix = requiredEnv('TINYSANDBOX_S3_TEST_PREFIX')) {
  return {
    bucket: requiredEnv('TINYSANDBOX_S3_TEST_BUCKET'),
    prefix,
    region: requiredEnv('TINYSANDBOX_S3_TEST_REGION'),
    endpointUrl: requireLoopbackEndpoint(),
    forcePathStyle: true,
    credentials: {
      accessKeyId: requiredEnv('TINYSANDBOX_S3_TEST_ACCESS_KEY'),
      secretAccessKey: requiredEnv('TINYSANDBOX_S3_TEST_SECRET_KEY')
    }
  }
}

test('S3 compatibility endpoint guard is strictly loopback-only', () => {
  for (const value of ['http://127.0.0.1:9000', 'http://localhost:1']) {
    assert.equal(isAllowedLoopbackEndpoint(value), true, value)
  }
  for (const value of [
    'https://127.0.0.1:9000',
    'http://127.0.0.1',
    'http://127.0.0.1:0',
    'http://127.0.0.1:9000/',
    'http://localhost:+80',
    'http://localhost.evil:9000',
    'http://s3.amazonaws.com:80'
  ]) {
    assert.equal(isAllowedLoopbackEndpoint(value), false, value)
  }
})

test('s3Vfs validates option shapes synchronously', () => {
  assert.doesNotThrow(() => new Sandbox({ s3Vfs: null }))
  assert.doesNotThrow(() => new Sandbox({ s3Vfs: undefined }))
  assert.throws(() => new Sandbox({ s3Vfs: {} }), /s3Vfs bucket is required/)
  assert.throws(() => new Sandbox({ s3Vfs: 42 }), /object/i)
  assert.throws(() => new Sandbox({ s3Vfs: { bucket: '' } }), /bucket must be a nonempty string/)
  assert.throws(() => new Sandbox({ s3Vfs: { bucket: '   ' } }), /bucket must be a nonempty string/)
  assert.throws(() => new Sandbox({ s3Vfs: { bucket: 42 } }), /string/i)
  assert.throws(() => new Sandbox({ s3Vfs: { bucket: 'bucket', prefix: 42 } }), /string/i)
  assert.throws(() => new Sandbox({ s3Vfs: { bucket: 'bucket', region: '' } }), /region must be a nonempty string/)
  assert.throws(() => new Sandbox({ s3Vfs: { bucket: 'bucket', region: 42 } }), /string/i)
  assert.throws(() => new Sandbox({ s3Vfs: { bucket: 'bucket', endpointUrl: '' } }), /endpointUrl must be a nonempty string/)
  assert.throws(
    () => new Sandbox({ s3Vfs: { bucket: 'bucket', credentials: { secretAccessKey: 'secret' } } }),
    /accessKeyId is required/
  )
  assert.throws(
    () => new Sandbox({ s3Vfs: { bucket: 'bucket', credentials: { accessKeyId: 'key' } } }),
    /secretAccessKey is required/
  )
  assert.throws(
    () => new Sandbox({
      s3Vfs: {
        bucket: 'bucket',
        credentials: { accessKeyId: 'key', secretAccessKey: 'secret', sessionToken: '' }
      }
    }),
    /sessionToken must be a nonempty string/
  )
  assert.throws(
    () => new Sandbox({ vfs: {}, s3Vfs: {} }),
    /either vfs or localVfs or s3Vfs/
  )
  assert.throws(
    () => new Sandbox({ localVfs: {}, s3Vfs: {} }),
    /either vfs or localVfs or s3Vfs/
  )

  const accessKeyId = 'shape-test-key'
  const secretAccessKey = 'shape-test-secret'
  assert.doesNotThrow(() => new Sandbox({
    vfs: null,
    localVfs: undefined,
    s3Vfs: {
      bucket: 'bucket',
      prefix: null,
      region: 'us-east-1',
      endpointUrl: null,
      forcePathStyle: null,
      credentials: { accessKeyId, secretAccessKey, sessionToken: null }
    }
  }))
  assert.doesNotThrow(() => new Sandbox({
    s3Vfs: { bucket: 'bucket', region: 'us-east-1', credentials: null }
  }))

  const originalRegion = process.env.AWS_REGION
  process.env.AWS_REGION = 'us-east-1'
  try {
    assert.doesNotThrow(() => new Sandbox({
      s3Vfs: { bucket: 'bucket', region: null, credentials: { accessKeyId, secretAccessKey } }
    }))
  } finally {
    if (originalRegion === undefined) delete process.env.AWS_REGION
    else process.env.AWS_REGION = originalRegion
  }
})

test('custom VFS EIO errors retain their code', async () => {
  const vfs = Object.fromEntries(
    ['stat', 'readdir', 'mkdir', 'rename', 'unlink', 'rmdir', 'open', 'readAt', 'writeAt', 'truncate', 'close']
      .map((name) => [name, async () => { const error = new Error('remote I/O failed'); error.code = 'EIO'; throw error }])
  )
  const sandbox = new Sandbox({ vfs })
  await assert.rejects(() => sandbox.fs.stat('/x'), { code: 'EIO' })
})

test('built-in s3Vfs request failures use the normal EIO filesystem shape', async () => {
  const sandbox = new Sandbox({
    s3Vfs: {
      bucket: 'bucket',
      region: 'us-east-1',
      endpointUrl: 'not a url',
      forcePathStyle: true,
      credentials: { accessKeyId: 'key', secretAccessKey: 'secret' }
    }
  })
  await assert.rejects(
    () => sandbox.fs.stat('/x'),
    (error) => {
      assert.equal(error.code, 'EIO')
      assert.match(error.message, /^EIO: \/x$/)
      return true
    }
  )
})

test('s3Vfs reads a prefix through host, shell, and embedded JS APIs', { skip: !live }, async () => {
  const sandbox = new Sandbox({ s3Vfs: liveOptions() })

  assert.deepEqual(await sandbox.fs.stat('/alpha.txt'), {
    fileType: 'file',
    len: 6,
    isFile: true,
    isDir: false
  })
  assert.deepEqual(
    (await sandbox.fs.readdir('/')).map(({ name, fileType }) => [name, fileType]),
    [
      ['alpha.txt', 'file'],
      ['empty-marker', 'directory'],
      ['large.txt', 'file'],
      ['nested', 'directory']
    ]
  )
  assert.equal(String(await sandbox.fs.readFile('/nested/data.txt')), 'nested needle\nanother line\n')

  const handle = await sandbox.fs.open('/large.txt', { read: true })
  try {
    assert.equal(String(await sandbox.fs.readAt(handle, 13, 11)), 'record-0000')
  } finally {
    await sandbox.fs.close(handle)
  }

  const [alpha, nested] = await Promise.all([
    sandbox.fs.readFile('/alpha.txt'),
    sandbox.fs.readFile('/nested/data.txt')
  ])
  assert.equal(String(alpha), 'alpha\n')
  assert.equal(String(nested), 'nested needle\nanother line\n')

  const head = await sandbox.exec('cat /large.txt | head -n 1')
  assert.equal(head.exitCode, 0, head.stderr)
  assert.equal(head.stdout, 'first record\n')
  const grep = await sandbox.exec('grep needle /nested/data.txt')
  assert.equal(grep.exitCode, 0, grep.stderr)
  assert.equal(grep.stdout, 'nested needle\n')
  const embedded = await sandbox.exec(`js -e 'const fs=require("fs"); console.log(fs.readFileSync("/alpha.txt","utf8").trim())'`)
  assert.equal(embedded.exitCode, 0, embedded.stderr)
  assert.equal(embedded.stdout, 'alpha\n')
})

test('s3Vfs rejects mutations and contains its configured prefix', { skip: !live }, async () => {
  const sandbox = new Sandbox({ s3Vfs: liveOptions() })

  await assert.rejects(() => sandbox.fs.writeFile('/new.txt', Buffer.from('forbidden')), { code: 'EACCES' })
  await assert.rejects(() => sandbox.fs.mkdir('/new-dir'), { code: 'EACCES' })
  await assert.rejects(() => sandbox.fs.stat('/../root-secret.txt'), { code: 'ENOENT' })
  assert.ok((await sandbox.fs.readdir('/')).every((entry) => !entry.name.includes('secret')))

  const redirect = await sandbox.exec('echo forbidden > /new.txt')
  assert.notEqual(redirect.exitCode, 0)
  assert.match(redirect.stderr, /Permission denied/)
})

test('s3Vfs makes real reads from an explicitly empty bucket-root prefix', { skip: !live }, async () => {
  const sandbox = new Sandbox({ s3Vfs: liveOptions('') })
  const fullKey = `/${requiredEnv('TINYSANDBOX_S3_TEST_PREFIX')}/alpha.txt`
  assert.equal(String(await sandbox.fs.readFile(fullKey)), 'alpha\n')
})

test('s3Vfs treats a missing configured prefix as an empty virtual root', { skip: !live }, async () => {
  const missingPrefix = `${requiredEnv('TINYSANDBOX_S3_TEST_PREFIX')}-missing`
  const sandbox = new Sandbox({ s3Vfs: liveOptions(missingPrefix) })
  assert.deepEqual(await sandbox.fs.stat('/'), {
    fileType: 'directory',
    len: 0,
    isFile: false,
    isDir: true
  })
  assert.deepEqual(await sandbox.fs.readdir('/'), [])
  await assert.rejects(() => sandbox.fs.stat('/missing.txt'), { code: 'ENOENT' })
})

test('s3Vfs default AWS region and credential chains can read the live fixture', { skip: !live }, async () => {
  const sandbox = new Sandbox({
    s3Vfs: {
      bucket: requiredEnv('TINYSANDBOX_S3_TEST_BUCKET'),
      prefix: requiredEnv('TINYSANDBOX_S3_TEST_PREFIX'),
      endpointUrl: requireLoopbackEndpoint(),
      forcePathStyle: true
    }
  })
  assert.equal(String(await sandbox.fs.readFile('/alpha.txt')), 'alpha\n')
})
