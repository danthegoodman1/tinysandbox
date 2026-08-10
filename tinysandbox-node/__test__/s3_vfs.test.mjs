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

function sandboxWithS3(options) {
  return new Sandbox({ mounts: { input: { type: 's3', ...options } } })
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

test('s3 mount validates option shapes synchronously', () => {
  assert.throws(() => sandboxWithS3({}), /S3 mount bucket is required/)
  assert.throws(() => sandboxWithS3({ bucket: '' }), /bucket must be a nonempty string/)
  assert.throws(() => sandboxWithS3({ bucket: '   ' }), /bucket must be a nonempty string/)
  assert.throws(() => sandboxWithS3({ bucket: 42 }), /string/i)
  assert.throws(() => sandboxWithS3({ bucket: 'bucket', prefix: 42 }), /string/i)
  assert.throws(() => sandboxWithS3({ bucket: 'bucket', region: '' }), /region must be a nonempty string/)
  assert.throws(() => sandboxWithS3({ bucket: 'bucket', region: 42 }), /string/i)
  assert.throws(() => sandboxWithS3({ bucket: 'bucket', endpointUrl: '' }), /endpointUrl must be a nonempty string/)
  assert.throws(
    () => sandboxWithS3({ bucket: 'bucket', credentials: { secretAccessKey: 'secret' } }),
    /accessKeyId is required/
  )
  assert.throws(
    () => sandboxWithS3({ bucket: 'bucket', credentials: { accessKeyId: 'key' } }),
    /secretAccessKey is required/
  )
  assert.throws(
    () => sandboxWithS3({
      bucket: 'bucket',
      credentials: { accessKeyId: 'key', secretAccessKey: 'secret', sessionToken: '' }
    }),
    /sessionToken must be a nonempty string/
  )

  const accessKeyId = 'shape-test-key'
  const secretAccessKey = 'shape-test-secret'
  assert.doesNotThrow(() => sandboxWithS3({
    bucket: 'bucket',
    prefix: null,
    region: 'us-east-1',
    endpointUrl: null,
    forcePathStyle: null,
    credentials: { accessKeyId, secretAccessKey, sessionToken: null }
  }))
  assert.doesNotThrow(() => sandboxWithS3({ bucket: 'bucket', region: 'us-east-1', credentials: null }))

  const originalRegion = process.env.AWS_REGION
  process.env.AWS_REGION = 'us-east-1'
  try {
    assert.doesNotThrow(() => sandboxWithS3({
      bucket: 'bucket', region: null, credentials: { accessKeyId, secretAccessKey }
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
  const sandbox = new Sandbox({ mounts: { remote: { type: 'custom', vfs } } })
  await assert.rejects(() => sandbox.fs.stat('/remote/x'), { code: 'EIO' })
})

test('built-in S3 mount request failures use the normal EIO filesystem shape', async () => {
  const sandbox = sandboxWithS3({
    bucket: 'bucket',
    region: 'us-east-1',
    endpointUrl: 'not a url',
    forcePathStyle: true,
    credentials: { accessKeyId: 'key', secretAccessKey: 'secret' }
  })
  await assert.rejects(
    () => sandbox.fs.stat('/input/x'),
    (error) => {
      assert.equal(error.code, 'EIO')
      assert.match(error.message, /^EIO: \/input\/x$/)
      return true
    }
  )
})

test('S3 mount reads a prefix through host, shell, and embedded JS APIs', { skip: !live }, async () => {
  const sandbox = sandboxWithS3(liveOptions())

  assert.deepEqual(await sandbox.fs.stat('/input/alpha.txt'), {
    fileType: 'file',
    len: 6,
    isFile: true,
    isDir: false
  })
  assert.deepEqual(
    (await sandbox.fs.readdir('/input')).map(({ name, fileType }) => [name, fileType]),
    [
      ['alpha.txt', 'file'],
      ['empty-marker', 'directory'],
      ['large.txt', 'file'],
      ['nested', 'directory']
    ]
  )
  assert.equal(String(await sandbox.fs.readFile('/input/nested/data.txt')), 'nested needle\nanother line\n')

  const handle = await sandbox.fs.open('/input/large.txt', { read: true })
  try {
    assert.equal(String(await sandbox.fs.readAt(handle, 13, 11)), 'record-0000')
  } finally {
    await sandbox.fs.close(handle)
  }

  const [alpha, nested] = await Promise.all([
    sandbox.fs.readFile('/input/alpha.txt'),
    sandbox.fs.readFile('/input/nested/data.txt')
  ])
  assert.equal(String(alpha), 'alpha\n')
  assert.equal(String(nested), 'nested needle\nanother line\n')

  const head = await sandbox.exec('cat /input/large.txt | head -n 1')
  assert.equal(head.exitCode, 0, head.stderr)
  assert.equal(head.stdout, 'first record\n')
  const grep = await sandbox.exec('grep needle /input/nested/data.txt')
  assert.equal(grep.exitCode, 0, grep.stderr)
  assert.equal(grep.stdout, 'nested needle\n')
  const embedded = await sandbox.exec(`js -e 'const fs=require("fs"); console.log(fs.readFileSync("/input/alpha.txt","utf8").trim())'`)
  assert.equal(embedded.exitCode, 0, embedded.stderr)
  assert.equal(embedded.stdout, 'alpha\n')
})

test('S3 mount rejects mutations and contains its configured prefix', { skip: !live }, async () => {
  const sandbox = sandboxWithS3(liveOptions())

  await assert.rejects(() => sandbox.fs.writeFile('/input/new.txt', Buffer.from('forbidden')), { code: 'EACCES' })
  await assert.rejects(() => sandbox.fs.mkdir('/input/new-dir'), { code: 'EACCES' })
  await assert.rejects(() => sandbox.fs.stat('/input/../root-secret.txt'), { code: 'ENOENT' })
  assert.ok((await sandbox.fs.readdir('/input')).every((entry) => !entry.name.includes('secret')))

  const redirect = await sandbox.exec('echo forbidden > /input/new.txt')
  assert.notEqual(redirect.exitCode, 0)
  assert.match(redirect.stderr, /Permission denied/)
})

test('S3 mount makes real reads from an explicitly empty bucket-root prefix', { skip: !live }, async () => {
  const sandbox = sandboxWithS3(liveOptions(''))
  const fullKey = `/input/${requiredEnv('TINYSANDBOX_S3_TEST_PREFIX')}/alpha.txt`
  assert.equal(String(await sandbox.fs.readFile(fullKey)), 'alpha\n')
})

test('S3 mount treats a missing configured prefix as an empty virtual root', { skip: !live }, async () => {
  const missingPrefix = `${requiredEnv('TINYSANDBOX_S3_TEST_PREFIX')}-missing`
  const sandbox = sandboxWithS3(liveOptions(missingPrefix))
  assert.deepEqual(await sandbox.fs.stat('/input'), {
    fileType: 'directory',
    len: 0,
    isFile: false,
    isDir: true
  })
  assert.deepEqual(await sandbox.fs.readdir('/input'), [])
  await assert.rejects(() => sandbox.fs.stat('/input/missing.txt'), { code: 'ENOENT' })
})

test('S3 mount default AWS region and credential chains can read the live fixture', { skip: !live }, async () => {
  const sandbox = sandboxWithS3({
    bucket: requiredEnv('TINYSANDBOX_S3_TEST_BUCKET'),
    prefix: requiredEnv('TINYSANDBOX_S3_TEST_PREFIX'),
    endpointUrl: requireLoopbackEndpoint(),
    forcePathStyle: true
  })
  assert.equal(String(await sandbox.fs.readFile('/input/alpha.txt')), 'alpha\n')
})
