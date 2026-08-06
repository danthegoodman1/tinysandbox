# @tinysandbox/tinysandbox

Node.js bindings for [tinysandbox](https://github.com/danthegoodman1/tinysandbox), an in-process sandbox for AI agents with a Linux-like shell, coreutils, a virtual filesystem, and a Wasmtime-hosted JavaScript runtime.

```bash
npm install @tinysandbox/tinysandbox
```

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox()

await sandbox.exec('mkdir -p /workspace/data')
await sandbox.exec("echo 'alpha\nbeta\nalpha' > /workspace/data/words.txt")

const result = await sandbox.exec('sort -u /workspace/data/words.txt | wc -l')
console.log(result.stdout)
```

## What It Provides

- A bash-like shell subset with pipelines, redirects, variables, and session state.
- Built-in commands such as `cat`, `grep`, `head`, `tail`, `sort`, `uniq`, `wc`, `sed`, `jq`, `ls`, `cp`, `mv`, `rm`, and `mkdir`.
- A quota-enforced virtual filesystem with direct host APIs and JavaScript-backed VFS adapters.
- An optional local directory VFS (`localVfs: { root, quota? }`, Unix only) that persists the sandbox filesystem under a host directory with strict path containment (`..` clamped at the root, symlinks never followed). Quota usage can be rebaselined from the host via `sandbox.refreshLocalVfs()` (rescan) or `sandbox.setLocalVfsUsage({ usedBytes, fileCount })` (push).
- A built-in read-only S3 VFS that exposes one bucket/key prefix as `/` and performs bounded range reads instead of downloading whole objects.
- A sandboxed `js` command with a Node-compatible synchronous `fs` subset, `require`, `Buffer`, `process`, and `console`; expose JS-facing custom host functionality as syscalls.
- Limits and metrics for wall time, output size, command timings, pipe bytes, jq input bytes, and wasm memory. jq input bytes and JSON nesting are capped before evaluation, and the jq filter program text (not input data) is capped on size, nesting, and syntax complexity; jq filter evaluation has the limitations documented in the repository README.

## Read-only S3 filesystem

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({
  s3Vfs: {
    bucket: 'agent-inputs',
    prefix: 'tenant-42/current',
    region: 'us-east-1',
    // Optional for S3-compatible APIs:
    // endpointUrl: 'http://127.0.0.1:9000',
    // forcePathStyle: true,
    // credentials: { accessKeyId: '...', secretAccessKey: '...' }
  }
})

const result = await sandbox.exec('cat /large.log | head -n 1')
```

When region or credentials are omitted, the AWS SDK default provider chains
are used. The prefix is a strict root boundary; an empty prefix exposes the
bucket root, while a missing prefix is an empty filesystem. Implicit prefixes
and zero-byte markers become directories, and directories win object/prefix
name collisions.

Reads use ETag-pinned S3 ranges (64 KiB for normal sandbox streaming), so an
externally replaced object fails with `EIO` rather than mixing revisions.
Writes, redirects, and path mutations fail with `EACCES`; there is no object
cache, quota/stat accounting, snapshot support, or version browsing. Grant
only `s3:GetObject` for exposed keys and prefix-restricted `s3:ListBucket`.
See the repository README for Rust client configuration, S3-compatible endpoint
details, IAM JSON, and the complete semantics.

## Platform Support

Published packages include native bindings built by the release workflow. Linux and macOS artifacts are included in the npm package.

## More

See the [repository README](https://github.com/danthegoodman1/tinysandbox#readme) for the full Rust and TypeScript documentation.
