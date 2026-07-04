## Final shape: ultra-minimal agent sandbox

```text
Host supervisor (Rust)
  ├── per-agent VFS (behind a Rust trait)
  ├── tiny shell dispatcher (bash-compatible subset)
  ├── native builtin Unix tools (Rust functions over the VFS)
  └── Wasmtime
        └── quickjs-ng.wasm for agent-written JS
```

### Guiding principle: invisible in the happy path

The sandbox is a Linux-like environment that agents already know how to use.
For basic operations — coreutils, shell pipelines, filesystem access, plain
JS scripts — behavior is identical to a real system: same commands, same
flags, same output format, same Node `fs` API. No special instructions or
re-learning required unless the agent leaves the happy path.

Fidelity to familiar interfaces is therefore a hard requirement, not a
nice-to-have: every supported command and API must match its GNU/POSIX/Node
counterpart for the subset it implements, and unsupported features must fail
with clear errors rather than silently diverging.

### Core idea

Do **not** boot Linux, do **not** spawn a VM, and do **not** run full Bash.

Give each agent a tiny OS-like environment:

```text
/workspace   agent files
/tmp         ephemeral scratch space
/bin         read-only virtual dir synthesized from the command registry
stdin/stdout/stderr
cwd/env
quotas
```

Commands are in-process functions, not files, but `/bin` still exists so the
environment survives probing: `ls /bin` lists every registered command as an
executable-mode entry, `which grep` resolves, and writes to `/bin` fail with
a permission error like they would anywhere else.

Trust boundary: the coreutils and shell are our code — only the *arguments*
and *file contents* are untrusted, and they can't escape a VFS-backed
implementation by construction. Wasm isolation is reserved for the one thing
that runs agent-authored code: the JS interpreter.

### Execution model

Agent runs:

```bash
cat notes.txt | grep TODO | js transform.js > out.txt
```

The shell dispatcher parses it and runs:

```text
cat    (native builtin, in-process)
  -> in-memory pipe
grep   (native builtin, in-process)
  -> in-memory pipe
quickjs-ng.wasm transform.js   (Wasmtime, epoch-limited)
  -> VFS write out.txt
```

The shell is a hand-rolled bash-compatible subset (~small lexer/parser):

```text
commands + args
single/double quoting, backslash escapes
pipes: |
redirects: > >> <
sequencing: && || ;
env vars: $VAR, VAR=x cmd
cwd
```

Globbing and `$(...)` are deferred. Semantics for the supported grammar match
bash exactly; anything outside it is a clear parse error.

### Tools

Builtins are plain Rust functions over the VFS trait — no wasm coreutils.
(uutils on wasm32-wasi is patchy, BusyBox needs fork/exec which WASI lacks,
and sandboxing trusted tool code buys nothing.) Each builtin matches GNU
behavior for its common flags.

```text
core:   cat, ls, cp, mv, rm, mkdir, touch, echo, pwd
text:   grep, head, tail, sort, uniq, wc, sed (s/// subset)
```

`grep` uses the Rust `regex` crate: linear-time matching, so hostile patterns
can't blow up the CPU. `awk` and `tar` are omitted — JS covers awk's need,
and archives don't matter until files enter or leave the sandbox.

### JavaScript support

`/bin/js` runs quickjs-ng (maintained QuickJS fork) compiled to wasm,
executed under Wasmtime. Native embedding is off the table: wasm isolation
is the whole point for agent-authored code.

```bash
js script.js input.json > output.json
```

Expose a Node-compatible `fs` API (major operations, backed by the VFS):

```js
// whole-file and directory operations
fs.readFileSync("/workspace/input.txt", "utf8")
fs.writeFileSync("/workspace/out.txt", "data")
fs.appendFileSync("/workspace/log.txt", "line\n")
fs.mkdirSync("/workspace/dir", { recursive: true })
fs.readdirSync("/workspace")
fs.statSync("/workspace/input.txt")
fs.renameSync("/workspace/a", "/workspace/b")
fs.rmSync("/workspace/tmp", { recursive: true })

// random-access reads and writes via file handles
const fd = fs.openSync("/workspace/data.bin", "r+")
fs.readSync(fd, buf, 0, 4096, 1024)   // read 4KB at offset 1024
fs.writeSync(fd, buf, 0, 512, 8192)   // write 512B at offset 8192
fs.ftruncateSync(fd, 16384)
fs.closeSync(fd)
```

Node compatibility keeps agent-written scripts portable; the VFS-backed
implementation means no host filesystem access regardless of API surface.
Sync-only avoids needing an event loop story in QuickJS.

Do **not** expose:

```text
child_process
net
host fs
raw process.env
native modules
```

### Filesystem

The VFS is the source of truth, defined as a Rust trait so backends are
swappable and every consumer (builtins, shell, JS hostcalls) shares one
implementation:

```rust
trait Vfs {
    // paths: stat, readdir, mkdir, rename, unlink, ...
    // handles: open(mode) -> Fd, read_at, write_at, truncate, close
}
```

Backend roadmap:

```text
now:    in-memory tree
next:   copy-on-write snapshots (agent rollback/branching)
later:  SQLite/object-storage persistence, only if multi-node demands it
```

Quotas are enforced inside the trait implementation so all consumers get
them for free:

```text
read/write quota
file count quota
max file size
/tmp cleanup
no host path access
```

Exposure to wasm is via custom host calls implementing the Node `fs` surface
(~a dozen calls: open/close/read_at/write_at/stat/readdir/rename/unlink/
truncate/mkdir) — not `wasi:filesystem`, which is far more work for no
benefit. The interpreter gets WASI stdio/clock/random only; it physically
cannot touch anything without a hostcall we defined.

### Security controls

Per agent:

```text
memory limit (Wasmtime Store limits)
CPU timeout via epoch interruption (near-zero overhead; fuel only if
  deterministic metering is ever needed)
wall-clock timeout
stdout/stderr size limit, truncated head+tail with a marker
filesystem byte limit
max command count
no ambient network
explicit fetch allowlist if needed
deterministic env/cwd
```

Host defense-in-depth (deployment config, do from the start):

```text
run supervisor as non-root
seccomp
cgroups
AppArmor/SELinux if available
no broad host filesystem mounts
no ambient outbound network
```

### Runtime tiers

```text
Tier 1: default
  native builtins + VFS + tiny shell + Wasmtime-hosted QuickJS

Tier 2: compatibility fallback
  rootless Linux namespace sandbox + BusyBox/Toybox

Tier 3: high-risk hostile workloads
  Firecracker microVM
```

Tiers 2–3 are documented escape hatches only; no code is built for them now.

### Wasmtime notes

Shared `Engine`, module precompiled once, instantiation via `InstancePre`
(microseconds per run). Async wasmtime + epoch ticks integrate directly with
tokio — no worker pool needed; spawn a task per execution.

### Recommended default

```text
Rust host process
  + VFS trait (in-memory impl first)
  + tiny shell dispatcher (bash-compatible subset)
  + ~15 native builtin tools
  + Wasmtime + quickjs-ng.wasm with Node-fs hostcalls
```

This gives an ultra-light "agent OS" without paying VM or Linux-container
overhead per sandbox, while keeping capabilities explicit and bounded — and
without agents ever needing to be told they're in a special system.
