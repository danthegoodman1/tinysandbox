# tinysandbox

[![crates.io](https://img.shields.io/crates/v/tinysandbox.svg)](https://crates.io/crates/tinysandbox)
[![npm](https://img.shields.io/npm/v/@tinysandbox/tinysandbox.svg)](https://www.npmjs.com/package/@tinysandbox/tinysandbox)
[![docs.rs](https://docs.rs/tinysandbox/badge.svg)](https://docs.rs/tinysandbox)
[![CI](https://github.com/danthegoodman1/tinysandbox/actions/workflows/ci.yml/badge.svg)](https://github.com/danthegoodman1/tinysandbox/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

An ultra-minimal, Linux-like sandbox for AI agents — a shell, coreutils, a
filesystem, and a secure JavaScript runtime in a single Rust crate, with no
containers, no VMs, and no access to the host.

#### Rust

```rust no_run
use tinysandbox::sandbox::Sandbox;

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder().build();

    sandbox
        .exec("echo 'hello from the sandbox' > /workspace/greeting.txt")
        .await;

    let result = sandbox
        .exec("cat /workspace/greeting.txt | grep -c sandbox")
        .await;
    assert_eq!(result.stdout, "1\n");
}
```

#### TypeScript

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox()

await sandbox.exec("echo 'hello from the sandbox' > /workspace/greeting.txt")

const result = await sandbox.exec('cat /workspace/greeting.txt | grep -c sandbox')
console.assert(result.stdout === '1\n')
```

## Table of contents

- [Why](#why)
- [Quickstart](#quickstart)
  - [Rust](#rust-1)
  - [TypeScript](#typescript-1)
  - [Rust filesystem access](#rust-2)
  - [TypeScript filesystem access](#typescript-2)
- [What's inside](#whats-inside)
  - [Shell](#shell)
  - [Builtins](#builtins)
  - [JavaScript runtime](#javascript-runtime)
- [Prompt chunks](#prompt-chunks)
- [Custom commands](#custom-commands)
  - [Rust](#rust-4)
  - [TypeScript](#typescript-4)
- [JavaScript host capabilities](#javascript-host-capabilities)
  - [Host globals](#host-globals)
  - [Changing globals between commands](#changing-globals-between-commands)
  - [JavaScript prelude](#javascript-prelude)
  - [Fetch](#fetch)
- [Filesystem backends](#filesystem-backends)
- [Filesystem mounts](#filesystem-mounts)
- [Local directory VFS](#local-directory-vfs)
  - [Rust](#rust-5)
  - [TypeScript](#typescript-5)
- [S3 VFS](#s3-vfs)
  - [Writes](#writes)
  - [Rust](#rust-6)
  - [TypeScript](#typescript-6)
- [Bring your own VFS](#bring-your-own-vfs)
  - [Rust](#rust-7)
  - [TypeScript](#typescript-7)
  - [Rust lower-level VFS](#rust-8)
  - [TypeScript lower-level VFS](#typescript-8)
- [Snapshots](#snapshots)
  - [Rust](#rust-9)
  - [TypeScript](#typescript-9)
  - [Rust diffing](#rust-10)
  - [TypeScript diffing](#typescript-10)
- [Limits and observability](#limits-and-observability)
  - [Rust](#rust-11)
  - [TypeScript](#typescript-11)
- [Security model](#security-model)
- [Comparison with just-bash](#comparison-with-just-bash)
- [Performance](#performance)
  - [JavaScript runtime startup](#javascript-runtime-startup)
- [Feature flags](#feature-flags)
- [Examples](#examples)
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
npm i @tinysandbox/tinysandbox
```

#### Rust

```rust no_run
use tinysandbox::sandbox::Sandbox;

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder().build();

    // By default, each exec starts from the builder's cwd/env. The VFS persists.
    sandbox.exec("mkdir -p /workspace/data").await;
    sandbox
        .exec("echo 'alpha\nbeta\nalpha' > /workspace/data/words.txt")
        .await;

    // GNU-faithful output shapes, down to wc padding stdin counts to width 7.
    let result = sandbox.exec("sort -u /workspace/data/words.txt | wc -l").await;
    assert_eq!(result.stdout, "      2\n");
    assert_eq!(result.exit_code, 0);

    // JavaScript with a Node-compatible fs API, sandboxed under Wasmtime.
    sandbox
        .exec(r#"echo 'const fs = require("fs"); console.log(fs.readFileSync("/workspace/data/words.txt", "utf8").length)' > /workspace/count.js"#)
        .await;
    let result = sandbox.exec("js /workspace/count.js").await;
    assert_eq!(result.stdout, "17\n");
}
```

#### TypeScript

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox()

// By default, each exec starts from the builder's cwd/env. The VFS persists.
await sandbox.exec('mkdir -p /workspace/data')
await sandbox.exec("echo 'alpha\nbeta\nalpha' > /workspace/data/words.txt")

// GNU-faithful output shapes, down to wc padding stdin counts to width 7.
const result = await sandbox.exec('sort -u /workspace/data/words.txt | wc -l')
console.assert(result.stdout === '      2\n')
console.assert(result.exitCode === 0)

// JavaScript with a Node-compatible fs API, sandboxed under Wasmtime.
await sandbox.exec(`echo 'const fs = require("fs"); console.log(fs.readFileSync("/workspace/data/words.txt", "utf8").length)' > /workspace/count.js`)
const counted = await sandbox.exec('js /workspace/count.js')
console.assert(counted.stdout === '17\n')
```

The host can also work with the filesystem directly — useful for seeding
input files or reading results without going through the shell:

#### Rust

```rust no_run
use tinysandbox::sandbox::Sandbox;
use tinysandbox::vfs::OpenMode;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = Sandbox::builder().build();
    let vfs = sandbox.vfs();
    let handle = vfs.open("/workspace/report.txt", OpenMode::write_only().create())?;
    vfs.write_at(handle, 0, b"direct host access")?;
    vfs.close(handle)?;
    Ok(())
}
```

#### TypeScript

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox()
await sandbox.fs.writeFile('/workspace/report.txt', Buffer.from('direct host access'))
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
  assignments, `export` / `unset`, and opt-in persistent cwd/env per `Sandbox`
- Loud, positioned errors for what's not supported: globs, `$(...)`,
  backticks, heredocs, `&`, subshells, tilde expansion

### Builtins

Native Rust implementations running directly against the VFS — no processes
spawned. GNU-faithful for supported flags, GNU-shaped error messages and
exit codes; golden tests pin the output shapes against the real tools.

```text
files:  cat ls cp mv rm mkdir touch stat which pwd cd
text:   grep head tail sort uniq wc sed echo
other:  true false export unset jq js
```

`/bin` is synthesized from the command registry, so `ls /bin` and
`which cat` work and writes to `/bin` fail with `EACCES`. One documented
deviation: `grep` and `sed` use Rust regex syntax (linear-time matching, so
hostile patterns can't burn CPU) rather than POSIX BRE.

#### jq

`jq filter [files...]` is powered by [jaq](https://github.com/01mf02/jaq)
and runs as a native builtin over the same VFS and pipes as the rest of the
sandbox.

**Flags.** The CLI surface is intentionally small: `-r`, `-j`, `-c`, `-e`,
`-n`, `-s`, `-S`, `--tab`, `--indent N`, `--arg name value`,
`--argjson name json`, and `--`, plus file operands and `-` for stdin.
Unsupported options fail loudly.

**Input.** Stdin when no files are passed, otherwise each file operand in
order (`-` reads stdin at that point in the list). Newline-delimited JSON is
accepted by default as a stream of JSON values; with `-s`, all values from
stdin and files are parsed first and passed to the filter as one array.

**Limits.** All enforced before evaluation starts:

- `Limits::jq_input_bytes` / `limits.jqInputBytes` caps the total bytes read
  across stdin and files (default 8 MiB).
- JSON input and `--argjson` values are rejected past 1024 levels of
  array/object nesting, before bytes reach jaq's recursive JSON parser.
- The filter program text (the expression argument, not input data) is
  rejected past 256 KiB of source, 512 levels of grouped/interpolation
  nesting, or 1024 significant syntax tokens — far beyond any hand-written
  filter, but enough to keep hostile programs out of jaq's recursive parser.

**Resource behavior.** Output is streamed: `jq` checks the sandbox wall-clock
limit between output values and inside the tinysandbox-provided `range`, and
stops promptly when a downstream pipe closes, so
`jq -n 'range(0;1000000000)' | head` does not buffer unbounded output. jaq
does not expose a fully preemptive evaluator or an allocator limit, so some
non-output-producing filters only time out at the command boundary while the
blocking worker runs until jaq yields again, and evaluation memory is bounded
by wall time plus host memory rather than a jq-specific heap cap. Hosts
running untrusted filters should set `wall_time` conservatively.

**Not included.** User-defined jq functions (`def ...`), external module
loading, color output, and CLI flags outside the listed subset. Diagnostics
use tinysandbox/jaq-shaped wording.

### JavaScript runtime

`js script.js [args...]` and `js -e 'code'` run agent scripts on
[quickjs-ng](https://github.com/quickjs-ng/quickjs) compiled to WebAssembly
and hosted by Wasmtime. The runtime targets Node fidelity for everything it
implements (the test suite runs the same scripts under real Node and pins
identical output):

| Area | Supported |
| --- | --- |
| `fs` (sync) | `readFileSync`, `readLinesSync` (UTF-8 line iterator, 64KB buffer), `writeFileSync`, `appendFileSync`, `mkdirSync`, `readdirSync` (incl. `withFileTypes`), `statSync`, `renameSync`, `rmSync`, `unlinkSync`, `rmdirSync`, `existsSync`, `copyFileSync`, `openSync`, `readSync`, `writeSync`, `ftruncateSync`, `closeSync` |
| `require` | Relative/absolute CommonJS: `./x`, `../x`, `/x`, extension inference (`.js`, `.json`), `dir/index.js`, module cache, Node cycle semantics, `module.exports`/`exports` aliasing, `require.main`, `MODULE_NOT_FOUND` shapes |
| Globals | `console.log/info/warn/error` (Node formatting incl. `%s %d %j`-style substitution), `process.argv/env/cwd()/exit()`, `__filename`, `__dirname`, `Buffer` (`from`, `alloc`, `isBuffer`, `toString('utf8'/'hex'/'base64')`) |
| Fetch | WHATWG-subset `fetch`, `Headers`, and `Response` backed only by an embedder-provided handler |
| Errors | Node-shaped: `.code` (`'ENOENT'`...), libuv-faithful `.errno`, `.syscall`, `.path`, messages like `ENOENT: no such file or directory, open '/x'` |
| Limits | Per-run memory cap (default 64 MB) with clean OOM errors, CPU deadline via epoch interruption (`while(true){}` exits 124), fetch response body cap, catchable `RangeError` on stack exhaustion |

Not there on purpose: timers, an event loop, direct networking,
`child_process`/process spawning, built-in shell command execution from
JavaScript, and `node_modules` resolution — bare `require('lodash')` tells you
plainly that there is no npm in the sandbox. Async is intentionally narrow:
already-settled microtasks drain before exit, and `fetch` is available only
through an embedder-granted handler. All file access goes through the same VFS
and quotas as the shell. Known deviations (fetch subset details,
stack-frame naming, line-1 column offsets) are documented in the `js` module
docs.

## Prompt chunks

Both packages export ready-made system-prompt text describing the sandbox to
an agent. Each chunk is a short, self-contained block covering one part of
the environment — they assume the model already knows bash, coreutils, jq,
and Node, and only state where this environment differs. Mix the chunks that
match your configuration and join them with blank lines:

#### Rust

```rust
let system_prompt = [
    tinysandbox::prompts::OVERVIEW,
    tinysandbox::prompts::SHELL,
    tinysandbox::prompts::BUILTINS,
    tinysandbox::prompts::SESSION_EPHEMERAL,
    tinysandbox::prompts::JS,
]
.join("\n\n");
```

#### TypeScript

```ts
import { prompts } from '@tinysandbox/tinysandbox'

const systemPrompt = [
  prompts.overview,
  prompts.shell,
  prompts.builtins,
  prompts.sessionEphemeral,
  prompts.js
].join('\n\n')
```

Available chunks: `OVERVIEW`/`overview`, `SHELL`/`shell`,
`BUILTINS`/`builtins`, `JQ`/`jq`, `JS`/`js`, `FETCH`/`fetch`, and
`SESSION_EPHEMERAL`/`sessionEphemeral` or
`SESSION_PERSISTENT`/`sessionPersistent` (pick the one matching
`persist_session`). Skip `JS` when the `js` feature is off and `FETCH` when no
fetch handler is set. `globals(names)`/`globals(names)` is a function rather
than a constant: pass the names you bound so the chunk lists what the model can
call. Tests pin the builtins chunk to the actual command registry so the text
cannot drift from the sandbox.

## Custom commands

Anything registered with the builder is indistinguishable from a builtin: it
shows up in `/bin`, resolves via `which`, and composes in pipelines. A
command is just an async function from `CommandContext` (args, env, cwd,
stdio streams, a VFS handle, limits) to an exit code:

#### Rust

```rust no_run
use tinysandbox::sandbox::{CommandContext, CommandResult, Sandbox};
use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder()
        .command("greet", |mut ctx: CommandContext| async move {
            let name = ctx.args.first().map_or("world", String::as_str);
            let _ = ctx.stdout.write_all(format!("hello {name}\n").as_bytes()).await;
            CommandResult::success()
        })
        .build();

    let result = sandbox.exec("greet agent | wc -w").await; // pipes like any builtin
    assert_eq!(result.stdout, "      2\n");
}
```

#### TypeScript

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({
  commands: {
    greet: async ({ args }) => {
      const name = args[0] ?? 'world'
      return { stdout: Buffer.from(`hello ${name}\n`) }
    }
  }
})

const result = await sandbox.exec('greet agent | wc -w')
console.assert(result.stdout === '      2\n')
```

This is the intended way to expose tools to an agent — file converters,
linters, API bridges — while the sandbox contains everything the agent's
own code does with the results.

## JavaScript host capabilities

The `js` runtime can receive a smaller capability surface than a whole shell
command. Bind host functions into the JavaScript global scope, add a prelude to
shape the guest API, and grant `fetch` only when you want agent JS to reach an
embedder-provided transport.

Custom shell commands registered with `SandboxBuilder::command` / `commands`
are available to the shell, not automatically to sandboxed JavaScript. If you
want JavaScript to call custom host functionality, bind it as a host global and
optionally wrap it with a JavaScript prelude.

### Host globals

Host globals are async host functions bound into the guest global scope by
name. The guest calls them synchronously, JSON round-tripping one value in and
one value out.

The name is a dotted path. A bare `search` becomes `globalThis.search`;
`tools.search` becomes `globalThis.tools.search` with the `tools` namespace
object created for you, so several names can share one namespace. Each segment
must match `[A-Za-z_][A-Za-z0-9_]*`. Names the runtime provides itself
(`console`, `process`, `require`, `Buffer`, `fetch`, ...) are rejected when the
sandbox is built, and a name that collides with any other existing global, such
as a JavaScript intrinsic, fails the run instead of shadowing it. A name cannot
be both a function and a namespace: `tools` and `tools.search` conflict.

**Rust**

```rust no_run
use serde_json::json;
use tinysandbox::sandbox::{HostError, Sandbox};

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder()
        // Bare name: scripts call `whoami()`.
        .js_global("whoami", |_args| async { Ok(json!({ "name": "agent-1" })) })
        // Dotted name: scripts call `kv.get({ key })`.
        .js_global("kv.get", |args| async move {
            let key = args["key"].as_str().ok_or_else(|| {
                HostError::new("key is required").with_code("E_KEY")
            })?;
            Ok(json!({ "value": format!("value-for-{key}") }))
        })
        .build();

    let result = sandbox
        .exec("js -e 'console.log(whoami().name); console.log(kv.get({ key: \"a\" }).value)'")
        .await;
    assert_eq!(result.stdout, "agent-1\nvalue-for-a\n");
}
```

**TypeScript**

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({
  globals: {
    whoami: () => ({ name: 'agent-1' }),
    'kv.get': async ({ key }) => ({ value: `value-for-${key}` })
  }
})

const result = await sandbox.exec(
  `js -e 'console.log(whoami().name); console.log(kv.get({ key: "a" }).value)'`
)
console.assert(result.stdout === 'agent-1\nvalue-for-a\n')
```

Scripts can enumerate a namespace with `Object.keys(tools)`, and
`prompts::globals` / `prompts.globals` builds the prompt chunk that names every
bound global, similar to how `/bin` is synthesized from the command registry.

If a handler throws or returns an error, sandboxed JS receives a normal
`Error`. A string `code` property on the thrown error is copied to
`err.code`. Returned values must be JSON values; non-JSON-serializable Node
returns fail the call.

### Changing globals between commands

`SandboxBuilder::js_global` fixes the surface when the sandbox is built. A live
sandbox can also change it, which is what an agent loop wants when each turn
grants a different set of tools:

```rust no_run
use serde_json::json;
use tinysandbox::sandbox::{JsGlobals, Sandbox};

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder().build();

    // One swap, validated as a whole.
    sandbox
        .replace_js_globals(
            JsGlobals::new()
                .with("tools.search", |_args| async { Ok(json!({ "hits": [] })) })
                .with("tools.read_doc", |_args| async { Ok(json!("doc body")) }),
        )
        .expect("grant this turn's tools");

    // Or add to what is already bound, leaving the rest in place.
    sandbox
        .extend_js_globals(JsGlobals::new().with("tools.trace", |_args| async { Ok(json!("ok")) }))
        .expect("add this turn's extra tool");

    // Or one name at a time.
    sandbox
        .set_js_global("whoami", |_args| async { Ok(json!("agent-1")) })
        .expect("bind whoami");
    assert!(sandbox.remove_js_global("whoami"));
    assert_eq!(sandbox.js_global_names().len(), 3);
}
```

Every `js` command snapshots the registry when it starts, which fixes what the
change is visible to:

- A command already running keeps the set it started with, so a script never
  sees a global appear or vanish mid-run, and a handler already executing is not
  cancelled by removing its name.
- The snapshot is taken per command, not per `exec`, so in `js a.js && js b.js`
  a change landing between the two is invisible to the first and visible to the
  second.
- `replace_js_globals` makes the surface exactly the set you pass, dropping
  everything else including globals registered on the builder. That is what a
  turn-scoped grant wants: a tool left out stops being callable. Re-include the
  names that should survive.
- `extend_js_globals` merges instead, replacing only names it repeats. Both
  validate the resulting surface before it lands, so a set that collides with a
  bound namespace leaves the live globals untouched.
- Each call is one swap. A `remove_js_global` followed by a `set_js_global` has
  a window in between where a command would see neither, so change a set that
  must move together with `extend_js_globals` or `replace_js_globals`.

**TypeScript**

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({ globals: { whoami: () => 'agent-1' } })

// One swap, validated as a whole.
sandbox.replaceJsGlobals({
  'tools.search': async ({ q }) => ({ hits: [] }),
  'tools.readDoc': () => 'doc body'
})

// Or add to what is already bound, leaving the rest in place.
sandbox.extendJsGlobals({ 'tools.trace': () => 'ok' })

// Or one name at a time.
sandbox.setJsGlobal('whoami', () => 'agent-1')
sandbox.removeJsGlobal('whoami')
console.log(sandbox.jsGlobalNames()) // ['tools.readDoc', 'tools.search', 'tools.trace']
```

### JavaScript prelude

`js_prelude` / `jsPrelude` is evaluated after tinysandbox installs its host
bindings and before the agent script. It runs before CommonJS globals exist,
so use it to define globals or wrap capabilities, not to `require()` modules.

**Rust**

```rust no_run
use serde_json::json;
use tinysandbox::sandbox::Sandbox;

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder()
        .js_global("secret_get", |_args| async { Ok(json!({ "value": "redacted" })) })
        .js_prelude(
            "const secretGet = globalThis.secret_get; \
             globalThis.readSecret = () => secretGet({}).value; \
             delete globalThis.secret_get",
        )
        .build();

    let result = sandbox
        .exec("js -e 'console.log(readSecret(), typeof secret_get)'")
        .await;
    assert_eq!(result.stdout, "redacted undefined\n");
}
```

**TypeScript**

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({
  globals: {
    secretGet: () => ({ value: 'redacted' })
  },
  jsPrelude:
    'const secretGet = globalThis.secretGet; globalThis.readSecret = () => secretGet({}).value; delete globalThis.secretGet'
})

const result = await sandbox.exec("js -e 'console.log(readSecret(), typeof secretGet)'")
console.assert(result.stdout === 'redacted undefined\n')
```

### Fetch

`fetch` is not an HTTP client built into the crate. It is a guest API backed
by a handler you provide, so the host decides whether URLs map to HTTP,
service calls, fixtures, object storage, or nothing at all. Without a handler,
`fetch()` rejects with a network-unavailable cause.

The guest receives a WHATWG-style subset: `fetch`, `Headers`, `Response`, and
body helpers such as `text()`, `json()`, and `arrayBuffer()`. Streams,
`AbortController`, redirects, and the full browser/undici surface are outside
the subset; see the `js` module docs for precise deviations.
`Limits::fetch_response_bytes` / `limits.fetchResponseBytes` caps the response
body accepted from the host before it reaches the guest.

**Rust**

```rust no_run
use tinysandbox::sandbox::{FetchResponse, HostError, Sandbox};

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder()
        .fetch(|request| async move {
            if request.url == "https://example.test/config" {
                Ok(FetchResponse {
                    status: 200,
                    headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                    body: br#"{"ok":true}"#.to_vec(),
                })
            } else {
                Err(HostError::new("no route").with_code("ENOENT"))
            }
        })
        .build();

    let result = sandbox
        .exec("js -e 'fetch(\"https://example.test/config\").then(r => r.json()).then(v => console.log(v.ok))'")
        .await;
    assert_eq!(result.stdout, "true\n");
}
```

**TypeScript**

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({
  limits: { fetchResponseBytes: 1024 * 1024 },
  fetch: async ({ url, body }) => ({
    status: 200,
    headers: [['content-type', 'text/plain']],
    body: `echo ${url} ${body?.toString('utf8') ?? ''}`
  })
})

const result = await sandbox.exec(
  `js -e 'fetch("https://example.test/echo", { method: "POST", body: Buffer.from("hi") }).then(r => r.text()).then(console.log)'`
)
console.assert(result.stdout === 'echo https://example.test/echo hi\n')
```

## Filesystem backends

| Backend | Host storage | Mutability | Availability |
| --- | --- | --- | --- |
| `InMemoryVfs` | Process memory | Read/write | Default |
| `LocalVfs` | Contained host directory | Read/write | Unix |
| `S3Vfs` | Bucket/key prefix | Read/write, or read-only by config | Cargo feature `s3`; included in Node |

## Filesystem mounts

The sandbox root is a read-only mount namespace. Backends are attached at
static, top-level names such as `/workspace`, `/input`, and `/output`. `/bin`
is reserved for the synthesized command registry. `ls /` lists both `bin` and
the configured mounts. Paths outside a mount return `ENOENT`, mount points
cannot be removed or renamed, and cross-mount renames fail with `EXDEV`.
Copying between mounts works normally.

By default, a sandbox has one in-memory `workspace` mount and starts with
`cwd=/workspace`. Rust callers can add or replace mounts through the builder:

```rust ignore
let sandbox = Sandbox::builder()
    .mount("workspace", LocalVfs::new("/srv/job/workspace")?)
    .mount("input", S3Vfs::new(client, "agent-inputs", Some("job-42"))?)
    .cwd("/workspace")
    .build();
```

`clear_mounts()` removes the default workspace and resets the builder cwd to
`/`. Mount names must be single path components and cannot be `.`, `..`, or
`bin`. Mounts are fixed after `build()`.

In TypeScript, providing `mounts` replaces the default workspace exactly:

```ts
const sandbox = new Sandbox({
  mounts: {
    workspace: { type: 'memory', quota: { maxBytes: 64 * 1024 * 1024 } },
    input: { type: 's3', bucket: 'agent-inputs', prefix: 'job-42' },
    output: { type: 'local', root: '/srv/job/output' }
  },
  cwd: '/workspace'
})
```

Each mount enforces its own quota. `Sandbox::stats()` reports the checked sum
of mounts that expose usage statistics. Mounts such as `S3Vfs` that do not
report usage are skipped.

## Local directory VFS

On Unix hosts, `LocalVfs` roots one mount in a directory on the host disk.
Files persist across sandboxes and process
restarts, existing content in the directory is visible inside, and host
tools can read what the agent writes.

Containment is strict: every sandbox path resolves beneath the root (`..` is
clamped by normalization before touching the OS), symbolic links are never
followed (they are invisible to lookups and rejected with `O_NOFOLLOW` at
open time), and special files are refused. The same quota knobs as
`InMemoryVfs` apply, seeded by scanning the existing tree at construction.
Dedicate the directory to the sandbox — containment holds regardless, but
quota accounting assumes no other process mutates the tree while the sandbox
is live. `LocalVfs` passes the same conformance suite as `InMemoryVfs`.

When the host does legitimately touch the tree — or tracks usage out of band
entirely — rebaseline the counters from outside the sandbox: `refresh()`
rescans the directory, and `set_usage()` pushes externally computed numbers.
Enforcement then just answers "should this write be blocked" against the
current baseline, with live operations applying their deltas on top. From
TypeScript: `await sandbox.refreshLocalVfs('workspace')` and
`sandbox.setLocalVfsUsage('workspace', { usedBytes, fileCount })`.

#### Rust

```rust ignore
use tinysandbox::sandbox::Sandbox;
use tinysandbox::vfs::{LocalVfs, VfsQuota};

let sandbox = Sandbox::builder()
    .mount("workspace", LocalVfs::new("/srv/agent-42-workspace")?)
    .build();

// Or with quota limits:
let quota = VfsQuota { max_bytes: 64 << 20, max_files: 4096, max_file_size: 16 << 20 };
let sandbox = Sandbox::builder()
    .mount("workspace", LocalVfs::with_quota("/srv/agent-42-workspace", quota)?)
    .build();
```

#### TypeScript

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({
  mounts: {
    workspace: {
      type: 'local',
      root: '/srv/agent-42-workspace',
      quota: { maxBytes: 64 * 1024 * 1024 }
    }
  }
})
```

## S3 VFS

The optional `s3` Cargo feature provides `S3Vfs`, a read-write view of one S3
bucket/key prefix. The Node package includes the same backend as the `s3` mount
type. The configured prefix becomes the backend root. An empty prefix exposes
the whole bucket, and a prefix with no matching keys is a valid empty virtual
root. Sandbox path normalization and a fixed prefix boundary prevent access to
adjacent keys.

Files are read with bounded S3 `Range` requests rather than whole-object
downloads. Shell and embedded-JavaScript streams use 64 KiB chunks, so an
early-exit pipeline such as `cat /large.log | head -n 1` only fetches the
needed prefix. Open handles pin the object's ETag with `If-Match`; replacement
during a read fails with `EIO` instead of combining revisions. Object bodies
and directory listings are not cached.

Directories are virtual: both implicit prefixes and zero-byte marker objects
ending in `/` appear as directories. If an object `a` and descendants under
`a/` both exist, directory identity wins and the colliding object is hidden.
`mkdir` writes a marker object, and emptying a directory rewrites its marker so
the directory does not disappear with its last child. VFS quotas/stats,
snapshots, version-history browsing, S3 Express, and access points are not
supported.

### Writes

S3 has no partial-object update, so a writable handle stages its contents and
lands them as one object operation when the handle closes. Writes become
visible to other handles and other readers at that point, not before, and
`close` is the call that reports a write failure. Two handles open on one path
stage independently, so the last close wins.

How a handle stages depends on whether it needs the object's existing bytes:

| Open mode | Staging | Size limit |
| --- | --- | --- |
| Write-only, creating or truncating (`>`, `w`, `wx`) | Forward-only writes flush through a multipart upload | None |
| Append to an existing object (`>>`, `a`) | Whole body in memory | `max_edit_bytes` |
| Read-write (`r+`, `w+`) | Whole body in memory | `max_edit_bytes` |

Modifying an object means reading it, applying the writes, and putting it back,
so its whole body is held in memory. `max_edit_bytes` caps that at 32 MiB by
default; a longer object fails with `EFBIG` and has to be rewritten rather than
edited. Setting it to zero removes the limit. A forward-only write is not
capped: it uploads 8 MiB parts and frees them as it goes, so a new object of
any size costs bounded memory. Seeking back below what a stream has already
uploaded also reports `EFBIG`, since those bytes are no longer in memory.

Handles that never write cost nothing extra: `touch` on an existing object and
a read-write open that is only read never download or replace it.

Writes carry preconditions so a concurrent replacement fails instead of being
silently overwritten: `If-None-Match: *` makes `create_new` a real exclusive
create, and `If-Match` on a read-modify-write turns a lost update into an
error. Set `conditional_writes` to false for a compatible service that rejects
them, which gives up both protections.

S3 has no atomic directory rename. Renaming a directory copies and then deletes
every key beneath it: two requests per key, and an interrupted rename leaves
keys under both prefixes. Set `directory_rename` to false to reject it with
`EXDEV` instead.

Read-only mounts stay available through `S3VfsConfig::read_only()`, which
refuses every write and path mutation with `EACCES` before issuing a request.
Credentials remain the enforcing boundary; the flag only stops the VFS from
issuing mutating requests with a client that would accept them.

#### Rust

Enable the backend and add the AWS SDK used to configure its client:

```bash
cargo add tinysandbox --features s3
cargo add aws-config aws-sdk-s3
cargo add tokio --features macros,rt-multi-thread
```

```rust no_run
use aws_sdk_s3::Client;
use tinysandbox::sandbox::Sandbox;
use tinysandbox::vfs::{S3Vfs, S3VfsConfig};

async fn run_with_client(client: Client) -> Result<(), Box<dyn std::error::Error>> {
    let vfs = S3Vfs::new(client, "agent-workspaces", Some("tenant-42/jobs/current"))?;
    let sandbox = Sandbox::builder()
        .clear_mounts()
        .mount("workspace", vfs)
        .cwd("/workspace")
        .build();

    let result = sandbox
        .exec("grep ERROR /workspace/logs/app.log | head > /workspace/errors.txt")
        .await;
    println!("{}", result.stdout);
    Ok(())
}

async fn run_read_only(client: Client) -> Result<(), Box<dyn std::error::Error>> {
    let vfs = S3Vfs::with_config(
        client,
        "agent-inputs",
        Some("tenant-42/jobs/current"),
        S3VfsConfig::read_only(),
    )?;
    let sandbox = Sandbox::builder().clear_mounts().mount("input", vfs).build();
    println!("{}", sandbox.exec("cat /input/spec.md").await.stdout);
    Ok(())
}
```

Tune the write policy with `S3Vfs::with_config`:

```rust no_run
use tinysandbox::vfs::S3VfsConfig;

let config = S3VfsConfig {
    // Stage up to 128 MiB in memory to modify an existing object.
    max_edit_bytes: 128 * 1024 * 1024,
    // Reject `mv` on a directory instead of copying every key beneath it.
    directory_rename: false,
    ..S3VfsConfig::default()
};
```

`S3Vfs` accepts an already configured `aws_sdk_s3::Client`; endpoint,
credentials, region, retry, timeout, TLS, and path-style policy remain owned by
that client. This also supports S3-compatible services without adding a second
configuration layer to the VFS.

#### TypeScript

The Node binding uses the AWS SDK's default region and credential provider
chains when overrides are omitted. Explicit credentials must include both key
fields; `endpointUrl` and `forcePathStyle` support compatible services.

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({
  mounts: {
    input: {
      type: 's3',
      bucket: 'agent-inputs',
      prefix: 'tenant-42/jobs/current',
      region: 'us-east-1',
      endpointUrl: 'http://127.0.0.1:9000',
      forcePathStyle: true,
      credentials: {
        accessKeyId: process.env.S3_ACCESS_KEY!,
        secretAccessKey: process.env.S3_SECRET_KEY!
      }
    },
    // Writes are on by default; opt a mount out explicitly.
    reference: {
      type: 's3',
      bucket: 'agent-reference',
      readOnly: true
    }
  },
  cwd: '/input'
})

const firstLine = await sandbox.exec('cat /input/logs/app.log | head -n 1')
await sandbox.exec('grep ERROR /input/logs/app.log > /input/errors.txt')
```

`maxEditBytes`, `directoryRename`, and `conditionalWrites` mirror the Rust
configuration fields.

A read-only mount needs only `s3:GetObject` on the exposed keys and
prefix-restricted `s3:ListBucket` on the bucket:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::agent-inputs",
      "Condition": { "StringLike": { "s3:prefix": ["tenant-42/jobs/current", "tenant-42/jobs/current/*"] } }
    },
    {
      "Effect": "Allow",
      "Action": "s3:GetObject",
      "Resource": "arn:aws:s3:::agent-inputs/tenant-42/jobs/current/*"
    }
  ]
}
```

A writable mount also needs `s3:PutObject`, `s3:DeleteObject`, and
`s3:AbortMultipartUpload` on the same key scope. Multipart uploads use
`s3:PutObject`; `s3:AbortMultipartUpload` lets a failed or abandoned write
clean up its parts instead of leaving them billable. Because IAM is the
enforcing boundary, granting only read permissions keeps a mount effectively
read-only even at the default configuration.

Compatibility tests use a loopback-only, pinned S3-compatible container with
throwaway credentials; the project test suite never contacts AWS or another
public object store.

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

#### Rust

```rust ignore
use std::sync::Arc;
use tinysandbox::sandbox::Sandbox;

let sandbox = Sandbox::builder()
    .mount("workspace", MyVfs::connect("s3://agent-42-workspace")?)
    .build();

// Or share one VFS between sandboxes / keep a handle for yourself:
let vfs = Arc::new(MyVfs::connect("s3://agent-42-workspace")?);
let sandbox = Sandbox::builder().mount_arc("workspace", Arc::clone(&vfs)).build();
```

#### TypeScript

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({
  mounts: {
    workspace: { type: 'custom', vfs: MyVfs.connect('s3://agent-42-workspace') }
  }
})
```

The crate ships the same conformance suite that validates `InMemoryVfs`, so
you can prove your implementation behaves like a POSIX filesystem —
open-mode enforcement, rename-over-existing, unlink-while-open handle
semantics, quota accounting, path containment, and more:

#### Rust

```rust ignore
#[test]
fn my_vfs_conforms() {
    tinysandbox::vfs::conformance::run(|quota| MyVfs::new(quota));
}
```

#### TypeScript

```ts
import { runConformance } from '@tinysandbox/tinysandbox'

await runConformance((quota) => new MyVfs(quota))
```

The JavaScript conformance runner covers the core VFS contract. Snapshot
conformance is Rust-only for now because `VfsSnapshot` uses an associated
snapshot type that does not map cleanly onto the callback-object adapter.

See the `tinysandbox::vfs` rustdoc for the full trait contract (errno
expectations per method, quota semantics, handle identity rules).

## Snapshots

`InMemoryVfs` supports cheap copy-on-write snapshots for rollback and
branching. A snapshot captures path-visible filesystem contents, not open file
handles; restoring one invalidates handles opened before the restore.

#### Rust

```rust
use tinysandbox::vfs::{InMemoryVfs, OpenMode, Vfs, VfsQuota, VfsSnapshot};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let vfs = InMemoryVfs::new(VfsQuota::unlimited());
    let handle = vfs.open("/draft.txt", OpenMode::write_only().create_new())?;
    vfs.write_at(handle, 0, b"before")?;
    vfs.close(handle)?;

    let snapshot = vfs.snapshot()?;
    let branch = vfs.branch(&snapshot)?;

    vfs.unlink("/draft.txt")?;
    assert!(vfs.stat("/draft.txt").is_err());
    assert!(branch.stat("/draft.txt")?.is_file());
    Ok(())
}
```

#### TypeScript

The Node binding exposes live VFS operations and JS-backed VFS adapters.
Snapshot capture/restore/branch is currently Rust-only.

`Sandbox::vfs()` returns the composite mount namespace as `Arc<dyn Vfs>`, so
snapshot-aware callers should keep their own concrete backend handle and pass a
clone into the builder:

#### Rust

```rust no_run
use std::sync::Arc;
use tinysandbox::sandbox::Sandbox;
use tinysandbox::vfs::{InMemoryVfs, VfsQuota, VfsSnapshot};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let vfs = Arc::new(InMemoryVfs::new(VfsQuota::unlimited()));
    let sandbox = Sandbox::builder().mount_arc("workspace", vfs.clone()).build();
    let before_turn = vfs.snapshot()?;
    let result = sandbox.exec("echo draft > /workspace/answer.txt").await;
    if result.exit_code != 0 {
        vfs.restore(&before_turn)?;
    }
    Ok(())
}
```

#### TypeScript

Snapshot-aware workflows should keep this part in Rust for now. TypeScript
VFS adapters can still validate the non-snapshot contract with
`runConformance(vfsFactory)`.

## Limits and observability

Every `Sandbox` enforces wall-clock timeouts (exit 124, like GNU `timeout`),
stdout/stderr caps with head+tail truncation, a per-exec command budget,
VFS byte/file quotas (surfacing as `ENOSPC`), a wasm memory cap for JS, and a
fetch response body cap for embedder-backed `fetch`. All configurable via
`Limits`:

#### Rust

```rust no_run
use std::time::Duration;
use tinysandbox::sandbox::{Limits, Sandbox};

fn main() {
    let sandbox = Sandbox::builder()
        .limits(Limits {
            wall_time: Duration::from_secs(5),
            wasm_memory_bytes: 32 * 1024 * 1024,
            fetch_response_bytes: 1024 * 1024,
            ..Limits::default()
        })
        .build();
}
```

#### TypeScript

```ts
import { Sandbox } from '@tinysandbox/tinysandbox'

const sandbox = new Sandbox({
  limits: {
    wallTimeMs: 5000,
    wasmMemoryBytes: 32 * 1024 * 1024,
    fetchResponseBytes: 1024 * 1024
  }
})
```

`ExecResult` carries per-run metrics (wall time, per-command timings, pipe
byte counts, truncation flags, peak wasm memory), and `Sandbox::stats()`
reports VFS usage and total commands run.

## Security model

- **Native code never runs agent input.** The shell and builtins only
  interpret command text against the VFS; the only thing that executes
  agent-authored *code* is the wasm guest.
- **The wasm guest is capability-free.** The vendored QuickJS module
  (see [assets/PROVENANCE.md](https://github.com/danthegoodman1/tinysandbox/blob/main/assets/PROVENANCE.md) for the reproducible build) imports no WASI
  filesystem functions — no preopens, no `path_open`. Its only window to
  the world is the audited hostcall ABI, which routes through the same
  VFS, quotas, and path containment as everything else.
- **Resources are bounded** per execution: memory (ResourceLimiter), CPU
  (epoch interruption), wall clock, output size, file quotas.
- `..` traversal is contained at the virtual root. `/bin` and mount points are
  read-only. With
  the local directory VFS, containment also refuses symlinks and special
  files, so the sandbox cannot reach outside its host directory.

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
  byte means materializing the entire file in memory. In tinysandbox, a VFS
  backed by object storage or a database can serve TB-scale files while the
  sandbox only touches the KBs actually read.
- **Streaming pipes and redirects.** Pipeline stages exchange data through
  bounded async streams, and redirects write through VFS handles while the
  command runs. just-bash buffers command output before it can be consumed or
  written back; tinysandbox can run `cat /huge | head -n 1` without
  materializing the full input or output.
- **Agent code always runs in WebAssembly.** In tinysandbox, the only thing
  that executes agent-authored code is the capability-free QuickJS wasm
  guest, with hard memory and CPU limits enforced by Wasmtime. just-bash
  interprets the shell and its commands in the host JavaScript engine and
  relies on language-level hardening against engine breakouts.
- **Host language.** tinysandbox is a Rust crate with Node.js bindings;
  just-bash is TypeScript and runs in Node or the browser.s

## Performance

The repository includes memory benchmarks for the Rust crate and TypeScript
binding. Each benchmark runs every sandbox count in a fresh child process,
keeps all `N` sandboxes alive, samples resident set size (RSS), then runs a
small VFS workload on up to 1,000 live sandboxes and extrapolates that sampled
RSS delta across `N`.

```bash
cargo run --release --example memory_benchmark -- --counts 1000,10000,100000,1000000 --task-sample 1000
npm --prefix tinysandbox-node run benchmark:memory -- --counts 1000,10000,100000,1000000 --task-sample 1000
```

Workload:

```sh
mkdir -p /bench && echo bench-payload > /bench/echo.txt && cat /bench/echo.txt
```

Measured on macOS 26.5.1 arm64 with `rustc 1.96.0` and Node.js `v24.15.0`.
RSS includes runtime and allocator overhead for that process.

#### Rust

| active sandboxes | active peak RSS | active delta / sandbox | create time | task sample | measured task peak | extrapolated task peak | task time |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1,000 | 8.84 MiB | 6.51 KiB | 49 ms | 1,000 | 12.08 MiB | 12.08 MiB | 60 ms |
| 10,000 | 60.78 MiB | 5.97 KiB | 75 ms | 1,000 | 64.09 MiB | 93.91 MiB | 55 ms |
| 100,000 | 580.58 MiB | 5.92 KiB | 312 ms | 1,000 | 583.86 MiB | 908.70 MiB | 55 ms |
| 1,000,000 | 5.64 GiB | 5.92 KiB | 2.56 s | 1,000 | 5.65 GiB | 8.92 GiB | 44 ms |

#### TypeScript

| active sandboxes | active peak RSS | active delta / sandbox | create time | task sample | measured task peak | extrapolated task peak | task time |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1,000 | 85.02 MiB | 6.61 KiB | 3 ms | 1,000 | 88.83 MiB | 88.83 MiB | 33 ms |
| 10,000 | 126.55 MiB | 6.43 KiB | 30 ms | 1,000 | 130.55 MiB | 166.55 MiB | 37 ms |
| 100,000 | 680.95 MiB | 6.32 KiB | 315 ms | 1,000 | 685.83 MiB | 1.14 GiB | 38 ms |
| 1,000,000 | 6.16 GiB | 6.38 KiB | 3.04 s | 1,000 | 6.16 GiB | 9.03 GiB | 29 ms |

### JavaScript runtime startup

Nothing compiles wasm at run time. The build script turns `quickjs.wasm` into
machine code for the crate's target and the binary embeds the result, so the
first `js` command loads an artifact that is already executable. Doing that work
during the build is worth about 425 ms and 29 MiB per process.

What a process pays, measured on Linux x86_64 with `wasmtime` 46:

| Step | When | Cost |
| --- | --- | --- |
| First `js` command in a process | loads the artifact | ~9 ms, ~9.5 MiB RSS |
| The same first command, on the fallback path | only when the artifact is refused | ~430 ms, ~31 MiB RSS |
| One in-flight `js` command | every run | ~3.4 ms, ~0.5 MiB RSS |
| Sandbox with no `js` command running | — | no runtime cost |

The artifact is pinned to the target triple with that architecture's baseline
CPU features, so it runs on any CPU of that architecture rather than only one as
new as the build machine. It is also tied to this Wasmtime version. When either
check fails — a target the build could not generate code for, an artifact from a
different Wasmtime — the runtime compiles the module itself and keeps working at
the cost of that first command; `js::runtime_source()` reports which happened.

Two costs come with this. Wasmtime's compiler is a build dependency as well as a
runtime one, so a cold `cargo build` compiles it twice; and the binary carries
the 2.7 MB artifact alongside the 0.6 MB wasm.

To produce an artifact yourself — for another machine, or to share one across
processes that build separately — `js::precompile` returns the machine code and
`js::use_precompiled` installs it before the first `js` command, replacing the
embedded one:

**Rust**

```rust no_run
// Build step, for example in a packaging job.
let artifact = tinysandbox::js::precompile().expect("precompile quickjs");
std::fs::write("target/quickjs.cwasm", &artifact).expect("write artifact");

// Later process, before the first `js` command runs.
if let Ok(artifact) = std::fs::read("target/quickjs.cwasm") {
    let _ = tinysandbox::js::use_precompiled(&artifact);
}
```

**TypeScript**

The native package already embeds an artifact for its platform. To install your
own:

```ts
import { readFileSync, writeFileSync } from 'node:fs'
import { jsRuntimeSource, precompileJs, usePrecompiledJs } from '@tinysandbox/tinysandbox'

writeFileSync('quickjs.cwasm', precompileJs())

try {
  usePrecompiledJs(readFileSync('quickjs.cwasm'))
} catch {
  // Stale or foreign artifact: the embedded runtime still serves.
}

console.log(jsRuntimeSource()) // 'precompiled'
```

## Feature flags

| Feature | Default | Effect |
| --- | --- | --- |
| `js` | on | The `js` command, Wasmtime, and the embedded QuickJS module (~600 KB). Disable with `default-features = false` for a shell-and-coreutils-only sandbox with a much smaller dependency tree. |
| `s3` | off | The prefix-rooted `S3Vfs` and AWS S3 SDK client adapter. The Node package enables this feature. |

## Examples

Runnable with `cargo run --example <name>`:

- [`quickstart`](https://github.com/danthegoodman1/tinysandbox/blob/main/examples/quickstart.rs) — sessions, pipelines, redirects,
  and reading results back from the host
- [`custom_command`](https://github.com/danthegoodman1/tinysandbox/blob/main/examples/custom_command.rs) — registering a host
  command and composing it with builtins
- [`js_scripts`](https://github.com/danthegoodman1/tinysandbox/blob/main/examples/js_scripts.rs) — multi-file JS with `require`,
  the `fs` API, and a look at limits and metrics
- [`js_dynamic_globals`](https://github.com/danthegoodman1/tinysandbox/blob/main/examples/js_dynamic_globals.rs) — changing the host global surface between commands
- [`js_precompiled`](https://github.com/danthegoodman1/tinysandbox/blob/main/examples/js_precompiled.rs) — precompiling the QuickJS module and loading it in a later process
- [`js_globals`](https://github.com/danthegoodman1/tinysandbox/blob/main/examples/js_globals.rs) — host globals,
  prelude wrappers, and embedder-backed fetch

Runnable with `npm --prefix tinysandbox-node run examples` after the package
dependencies are installed:

- `quickstart.ts` — sessions, pipelines, redirects, and host reads
- `custom_command.ts` — registering a TypeScript host command
- `js_scripts.ts` — multi-file sandboxed JS with limits and metrics
- `js_globals.ts` — TypeScript host globals, prelude wrappers, and fetch transport
- `js_dynamic_globals.ts` — changing the host global surface between commands
- `js_precompiled.ts` — precompiling the QuickJS module and loading the artifact
- `js_vfs.ts` — TypeScript-backed VFS callbacks plus `runConformance`

## License

Licensed under either of [MIT](https://github.com/danthegoodman1/tinysandbox/blob/main/LICENSE-MIT) or
[Apache-2.0](https://github.com/danthegoodman1/tinysandbox/blob/main/LICENSE-APACHE), at your option.
