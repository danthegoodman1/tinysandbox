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
- A sandboxed `js` command with a Node-compatible synchronous `fs` subset, `require`, `Buffer`, `process`, and `console`.
- Limits and metrics for wall time, output size, command timings, pipe bytes, jq input bytes, and wasm memory. jq input bytes and JSON nesting are capped before evaluation, and the jq filter program text (not input data) is capped on size, nesting, and syntax complexity; jq filter evaluation has the limitations documented in the repository README.

## Platform Support

Published packages include native bindings built by the release workflow. Linux and macOS artifacts are included in the npm package.

## More

See the [repository README](https://github.com/danthegoodman1/tinysandbox#readme) for the full Rust and TypeScript documentation.
