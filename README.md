# tinysandbox

[![crates.io](https://img.shields.io/crates/v/tinysandbox.svg)](https://crates.io/crates/tinysandbox)
[![docs.rs](https://docs.rs/tinysandbox/badge.svg)](https://docs.rs/tinysandbox)
[![CI](https://github.com/danthegoodman1/tinysandbox/actions/workflows/ci.yml/badge.svg)](https://github.com/danthegoodman1/tinysandbox/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

An ultra-minimal, Linux-like sandbox for AI agents — a shell, coreutils, a
filesystem, and a JavaScript runtime in a single Rust crate, with no
containers, no VMs, and no access to the host.

- **Docs:** [docs.rs/tinysandbox](https://docs.rs/tinysandbox)
- **Crate:** [crates.io/crates/tinysandbox](https://crates.io/crates/tinysandbox)

```rust
let sandbox = Sandbox::builder().build();

sandbox.exec("mkdir /workspace && cd /workspace").await;
sandbox.exec("echo 'hello from the sandbox' > greeting.txt").await;

let result = sandbox.exec("cat greeting.txt | grep -c sandbox").await;
assert_eq!(result.stdout, "1\n");
```

## Table of contents

- [Why](#why)
- [Quickstart](#quickstart)
- [What's inside](#whats-inside)
  - [Shell](#shell)
  - [Builtins](#builtins)
  - [JavaScript runtime](#javascript-runtime)
- [Custom commands](#custom-commands)
- [Bring your own VFS](#bring-your-own-vfs)
- [Limits and observability](#limits-and-observability)
- [Security model](#security-model)
- [Comparison with just-bash](#comparison-with-just-bash)
- [Feature flags](#feature-flags)
- [Examples](#examples)
- [Roadmap](#roadmap)
- [License](#license)

## Why

Agents are good at bash and JavaScript because the training data is full of
both. But giving an agent a real shell means giving it your filesystem, your
network, and your process table — so the usual answer is a container or a
microVM, which costs seconds of startup, megabytes of memory per instance,
and an orchestration layer you now own.

tinysandbox takes a different trade. It executes a bash-compatible shell and
GNU-faithful coreutils *natively in your process* against a virtual
filesystem, and reserves heavyweight isolation (Wasmtime) for the one thing
that actually runs untrusted code: agent-authored JavaScript. The result:

- **Boot is instant and idle sandboxes cost kilobytes.** A `Sandbox` is a
  plain struct around an in-memory filesystem. `echo hello > out.txt` is
  microseconds — no VM, no fork/exec, no syscall filter.
- **The happy path feels exactly like Linux.** Supported commands, flags,
  and JS APIs match their GNU/bash/Node counterparts precisely (verified
  against the real tools in the test suite). You don't have to tell your
  agent it's in a special environment — probing with `ls /bin` or
  `which grep` behaves like it would anywhere else.
- **Everything outside the subset fails loudly.** Unsupported flags, shell
  constructs, and JS APIs produce clear errors, never silently different
  behavior.
- **The host stays unreachable.** Files live in a quota-enforced VFS. JS
  runs in a WebAssembly guest whose module has no filesystem imports at
  all — there is no code path from a script to your disk or network.

## Quickstart

```bash
cargo add tinysandbox tokio
```

```rust
use tinysandbox::sandbox::Sandbox;

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder().build();

    // Sessions persist cwd and env across execs, like a real shell.
    sandbox.exec("mkdir -p /workspace/data && cd /workspace").await;
    sandbox.exec("echo 'alpha\nbeta\nalpha' > data/words.txt").await;

    // GNU-faithful output shapes, down to wc padding stdin counts to width 7.
    let result = sandbox.exec("sort -u data/words.txt | wc -l").await;
    assert_eq!(result.stdout, "      2\n");
    assert_eq!(result.exit_code, 0);

    // JavaScript with a Node-compatible fs API, sandboxed under Wasmtime.
    sandbox
        .exec(r#"echo 'const fs = require("fs"); console.log(fs.readFileSync("data/words.txt", "utf8").length)' > count.js"#)
        .await;
    let result = sandbox.exec("js count.js").await;
    assert_eq!(result.stdout, "17\n");
}
```

The host can also work with the filesystem directly — useful for seeding
input files or reading results without going through the shell:

```rust
use tinysandbox::vfs::OpenMode;

let vfs = sandbox.vfs();
let handle = vfs.open("/workspace/report.txt", OpenMode::write_only().create())?;
vfs.write_at(handle, 0, b"direct host access")?;
vfs.close(handle)?;
```

## What's inside

### Shell

A hand-rolled, heavily tested parser and executor for the bash subset agents
actually use. Semantics inside the subset are verified against real bash:

- Pipelines (`cat log | grep err | wc -l`), lists (`&&`, `||`, `;`),
  newline separators, and line continuation after `&&`/`||`/`|`
- Redirects: `>`, `>>`, `<`, `2>`, `2>>`, `2>&1` — with bash-correct
  left-to-right fd resolution (`cmd 2>&1 > f` differs from `cmd > f 2>&1`,
  as it should)
- Quoting: single, double, backslash escapes, correct field splitting of
  unquoted `$VAR` expansions
- Variables: `$VAR`, `${VAR}`, `$?`, `VAR=x cmd` prefixes, bare
  assignments, `export` / `unset`, persistent cwd and env per `Sandbox`
- Loud, positioned errors for what's not supported: globs, `$(...)`,
  backticks, heredocs, `&`, subshells, tilde expansion

### Builtins

Native Rust implementations running directly against the VFS — no processes
spawned. GNU-faithful for supported flags, GNU-shaped error messages and
exit codes; golden tests pin the output shapes against the real tools.

```text
files:  cat ls cp mv rm mkdir touch stat which pwd cd
text:   grep head tail sort uniq wc sed echo
other:  true false export unset js
```

`/bin` is synthesized from the command registry, so `ls /bin` and
`which cat` work and writes to `/bin` fail with `EACCES`. One documented
deviation: `grep` and `sed` use Rust regex syntax (linear-time matching, so
hostile patterns can't burn CPU) rather than POSIX BRE.

### JavaScript runtime

`js script.js [args...]` and `js -e 'code'` run agent scripts on
[quickjs-ng](https://github.com/quickjs-ng/quickjs) compiled to WebAssembly
and hosted by Wasmtime. The runtime targets Node fidelity for everything it
implements (the test suite runs the same scripts under real Node and pins
identical output):

| Area | Supported |
| --- | --- |
| `fs` (sync) | `readFileSync`, `writeFileSync`, `appendFileSync`, `mkdirSync`, `readdirSync` (incl. `withFileTypes`), `statSync`, `renameSync`, `rmSync`, `unlinkSync`, `rmdirSync`, `existsSync`, `copyFileSync`, `openSync`, `readSync`, `writeSync`, `ftruncateSync`, `closeSync` |
| `require` | Relative/absolute CommonJS: `./x`, `../x`, `/x`, extension inference (`.js`, `.json`), `dir/index.js`, module cache, Node cycle semantics, `module.exports`/`exports` aliasing, `require.main`, `MODULE_NOT_FOUND` shapes |
| Globals | `console.log/info/warn/error` (Node formatting incl. `%s %d %j`-style substitution), `process.argv/env/cwd()/exit()`, `__filename`, `__dirname`, `Buffer` (`from`, `alloc`, `isBuffer`, `toString('utf8'/'hex'/'base64')`) |
| Errors | Node-shaped: `.code` (`'ENOENT'`...), libuv-faithful `.errno`, `.syscall`, `.path`, messages like `ENOENT: no such file or directory, open '/x'` |
| Limits | Per-run memory cap (default 64 MB) with clean OOM errors, CPU deadline via epoch interruption (`while(true){}` exits 124), catchable `RangeError` on stack exhaustion |

Not there on purpose: async APIs, event loop, timers, network, and
`node_modules` resolution — bare `require('lodash')` tells you plainly that
there is no npm in the sandbox. All file access goes through the same VFS
and quotas as the shell. Known cosmetic deviations (stack-frame naming,
line-1 column offsets) are documented in the `js` module docs.

## Custom commands

Anything registered with the builder is indistinguishable from a builtin: it
shows up in `/bin`, resolves via `which`, and composes in pipelines. A
command is just an async function from `CommandContext` (args, env, cwd,
stdio streams, a VFS handle, limits) to an exit code:

```rust
use tinysandbox::sandbox::{CommandContext, CommandResult, Sandbox};
use tokio::io::AsyncWriteExt;

let sandbox = Sandbox::builder()
    .command("greet", |mut ctx: CommandContext| async move {
        let name = ctx.args.first().map_or("world", String::as_str);
        let _ = ctx.stdout.write_all(format!("hello {name}\n").as_bytes()).await;
        CommandResult::success()
    })
    .build();

let result = sandbox.exec("greet agent | wc -w").await; // pipes like any builtin
assert_eq!(result.stdout, "      2\n");
```

This is the intended way to expose tools to an agent — file converters,
linters, API bridges — while the sandbox contains everything the agent's
own code does with the results.

## Bring your own VFS

The filesystem is a trait, and the in-memory implementation is just the
default. Back it with SQLite, object storage, or a network service by
implementing `tinysandbox::vfs::Vfs` — eleven synchronous, FUSE-style methods
(`stat`, `readdir`, `mkdir`, `rename`, `unlink`, `rmdir`, `open`,
`read_at`, `write_at`, `truncate`, `close`). Blocking implementations are
fine: the sandbox dispatches VFS calls to worker threads unless your
implementation opts into the in-memory fast path via `is_fast()`.

Attach it in the builder and the whole sandbox — shell, builtins, JS
scripts, and direct host access — runs against it:

```rust
use std::sync::Arc;
use tinysandbox::sandbox::Sandbox;

let sandbox = Sandbox::builder()
    .vfs(MyVfs::connect("s3://agent-42-workspace")?)
    .build();

// Or share one VFS between sandboxes / keep a handle for yourself:
let vfs = Arc::new(MyVfs::connect("s3://agent-42-workspace")?);
let sandbox = Sandbox::builder().vfs_arc(Arc::clone(&vfs)).build();
```

The crate ships the same conformance suite that validates `InMemoryVfs`, so
you can prove your implementation behaves like a POSIX filesystem —
open-mode enforcement, rename-over-existing, unlink-while-open handle
semantics, quota accounting, path containment, and more:

```rust
#[test]
fn my_vfs_conforms() {
    tinysandbox::vfs::conformance::run(|quota| MyVfs::new(quota));
}
```

See the `tinysandbox::vfs` rustdoc for the full trait contract (errno
expectations per method, quota semantics, handle identity rules).

## Limits and observability

Every `Sandbox` enforces wall-clock timeouts (exit 124, like GNU `timeout`),
stdout/stderr caps with head+tail truncation, a per-exec command budget,
VFS byte/file quotas (surfacing as `ENOSPC`), and a wasm memory cap for JS.
All configurable via `Limits`:

```rust
use std::time::Duration;
use tinysandbox::sandbox::{Limits, Sandbox};

let sandbox = Sandbox::builder()
    .limits(Limits {
        wall_time: Duration::from_secs(5),
        wasm_memory_bytes: 32 * 1024 * 1024,
        ..Limits::default()
    })
    .build();
```

`ExecResult` carries per-run metrics (wall time, per-command timings, pipe
byte counts, truncation flags, peak wasm memory), and `Sandbox::stats()`
reports VFS usage and total commands run.

## Security model

- **Native code never runs agent input.** The shell and builtins only
  interpret command text against the VFS; the only thing that executes
  agent-authored *code* is the wasm guest.
- **The wasm guest is capability-free.** The vendored QuickJS module
  (see `assets/PROVENANCE.md` for the reproducible build) imports no WASI
  filesystem functions — no preopens, no `path_open`. Its only window to
  the world is the audited hostcall ABI, which routes through the same
  VFS, quotas, and path containment as everything else.
- **Resources are bounded** per execution: memory (ResourceLimiter), CPU
  (epoch interruption), wall clock, output size, file quotas.
- `..` traversal is contained at the VFS root; `/bin` is read-only.

tinysandbox is one layer, not the whole story: for hostile multi-tenant
workloads you should still run your process under OS-level defense in depth
(non-root, seccomp/cgroups, or a microVM) appropriate to your threat model.

## Comparison with just-bash

[just-bash](https://github.com/vercel-labs/just-bash) is the closest
neighbor: a TypeScript simulated bash with a virtual filesystem, also built
for agents. Both give an agent a familiar shell without a container or VM,
but the designs differ in ways that matter:

- **Random file reads and writes.** The tinysandbox VFS is handle-and-offset
  based (`open`, `read_at`, `write_at`), and the JS runtime exposes the
  matching fd APIs (`fs.openSync` / `readSync` / `writeSync` with explicit
  positions). just-bash's filesystem interface is whole-file: reading one
  byte means materializing the entire file in memory, and a command's output
  is fully buffered before it can be written back. In tinysandbox, a VFS
  backed by object storage or a database can serve TB-scale files while the
  sandbox only touches the KBs actually read.
- **Agent code always runs in WebAssembly.** In tinysandbox, the only thing
  that executes agent-authored code is the capability-free QuickJS wasm
  guest, with hard memory and CPU limits enforced by Wasmtime. just-bash
  interprets the shell and its commands in the host JavaScript engine and
  relies on language-level hardening against engine breakouts.
- **Host language.** tinysandbox is a Rust crate (Node.js bindings are on
  the roadmap); just-bash is TypeScript and runs in Node or the browser. If
  your stack is JS-only today, just-bash is the natural pick; if you want
  native performance, a typed VFS trait, or to embed in a Rust service,
  that's tinysandbox.

## Feature flags

| Feature | Default | Effect |
| --- | --- | --- |
| `js` | on | The `js` command, Wasmtime, and the embedded QuickJS module (~600 KB). Disable with `default-features = false` for a shell-and-coreutils-only sandbox with a much smaller dependency tree. |

## Examples

Runnable with `cargo run --example <name>`:

- [`quickstart`](examples/quickstart.rs) — sessions, pipelines, redirects,
  and reading results back from the host
- [`custom_command`](examples/custom_command.rs) — registering a host
  command and composing it with builtins
- [`js_scripts`](examples/js_scripts.rs) — multi-file JS with `require`,
  the `fs` API, and a look at limits and metrics

## Roadmap

- Copy-on-write VFS snapshots for rollback and branching
- Node.js bindings (napi-rs): the whole sandbox — including VFS
  implementations written in JavaScript — usable from Node

## License

Licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.
