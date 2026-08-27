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
- Static top-level filesystem mounts backed by memory, local directories, S3, or JavaScript VFS adapters. The default is an in-memory `/workspace`. `ls /` lists every mount plus `/bin`.
- Local directory mounts (Unix only) with strict path containment and per-mount quotas. Usage can be rebaselined with `sandbox.refreshLocalVfs(mount)` or `sandbox.setLocalVfsUsage(mount, usage)`.
- S3 mounts that expose bucket/key prefixes, perform bounded range reads instead of downloading whole objects, and write back through staged object operations.
- A sandboxed `js` command with a Node-compatible synchronous `fs` subset, `require`, `Buffer`, `process`, and `console`; expose JS-facing custom host functionality as host globals.
- Limits and metrics for wall time, output size, command timings, pipe bytes, jq input bytes, and wasm memory. jq input bytes and JSON nesting are capped before evaluation, and the jq filter program text (not input data) is capped on size, nesting, and syntax complexity; jq filter evaluation has the limitations documented in the repository README.

## S3 filesystem

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({
  mounts: {
    workspace: {
      type: 's3',
      bucket: 'agent-workspaces',
      prefix: 'tenant-42/current',
      region: 'us-east-1',
      // Optional for S3-compatible APIs:
      // endpointUrl: 'http://127.0.0.1:9000',
      // forcePathStyle: true,
      // credentials: { accessKeyId: '...', secretAccessKey: '...' }
    },
    // Writes are on by default; opt a mount out explicitly.
    reference: { type: 's3', bucket: 'agent-reference', readOnly: true }
  },
  cwd: '/workspace'
})

const result = await sandbox.exec('cat /workspace/large.log | head -n 1 > /workspace/first.txt')
```

When region or credentials are omitted, the AWS SDK default provider chains
are used. The prefix is a strict root boundary; an empty prefix exposes the
bucket root, while a missing prefix is an empty filesystem. Implicit prefixes
and zero-byte markers become directories, and directories win object/prefix
name collisions.

Reads use ETag-pinned S3 ranges (64 KiB for normal sandbox streaming), so an
externally replaced object fails with `EIO` rather than mixing revisions.

S3 has no partial-object update, so a writable handle stages its contents and
lands them as one object operation when the handle closes; writes become
visible then, not before. A forward-only write streams through a multipart
upload at any size, while modifying an existing object holds its whole body in
memory, capped by `maxEditBytes` (32 MiB by default, zero for no limit).
Exceeding it fails with `EFBIG`, meaning the object has to be rewritten rather
than edited. `readOnly` refuses every mutation with `EACCES`, `directoryRename`
governs the copy-and-delete walk that renames a directory, and
`conditionalWrites` guards writes against concurrent replacement.

There is no object cache, quota/stat accounting, snapshot support, or version
browsing. Grant `s3:GetObject` plus prefix-restricted `s3:ListBucket` for
reads, adding `s3:PutObject`, `s3:DeleteObject`, and `s3:AbortMultipartUpload`
for writes. See the repository README for Rust client configuration,
S3-compatible endpoint details, IAM JSON, and the complete semantics.

## Platform Support

Installing `@tinysandbox/tinysandbox` automatically selects one optional native
package for the current operating system, CPU, and Linux libc. The public import
stays the same, while the main package contains only JavaScript and TypeScript
definitions and the selected native package contains exactly one binding.
Installations must leave optional dependencies enabled; using
`--omit=optional` removes the native binding required at runtime.

Prebuilt bindings cover both 64-bit processor families on the supported
desktop/server platforms:

| Platform | x64 | arm64 |
| --- | --- | --- |
| Linux (glibc) | yes | yes |
| macOS | yes | yes |

Linux prebuilt bindings require glibc 2.34 or newer. CI tests each
OS/architecture pair on a native runner, with Linux builds and tests running in
pinned glibc 2.34 containers. It also bundles the main entry point with esbuild
and verifies that the resulting application loads only the auto-detected native
package. The release workflow refuses to publish unless all four
architecture-specific packages are complete, but the main npm tarball is
rejected if it contains any `.node` file. Alpine and other musl-based Linux
distributions are not currently included.

## More

See the [repository README](https://github.com/danthegoodman1/tinysandbox#readme) for the full Rust and TypeScript documentation.
