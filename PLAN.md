# Development Plan

## Overarching Goal

Build `tinysandbox`: a Rust crate providing an ultra-minimal, Linux-like agent
sandbox — a VFS behind a trait, a bash-compatible shell subset, native
coreutils builtins, and a Wasmtime-hosted QuickJS runtime — that other Rust
projects embed via `Sandbox::builder().vfs(...).command(...).build()`. The
final phase adds Node.js bindings (napi-rs) so the whole sandbox, including
custom JS-implemented VFS backends, is usable from Node.

Non-goals: no real network access from the sandbox, no Python runtime, no
container/microVM tiers, no persistence backends beyond what snapshots need.

## Implementation Principles

- Happy-path fidelity: every supported command, flag, and API matches its
  GNU/POSIX/Node counterpart; unsupported features fail with clear errors,
  never silently diverge. Errors use errno-style codes (ENOENT, EACCES...).
- Smallest production-quality implementation; simple and correct over clever.
- `Vfs` is a synchronous, FUSE-style trait. Blocking impls (network-backed)
  are supported by dispatching calls to worker threads; async exists only at
  the `Sandbox` API boundary.
- Quotas enforce inside the `Vfs` implementation so shell, JS hostcalls, and
  direct host calls share one enforcement point.
- Interfaces stream, implementations may buffer: the `Command` trait takes
  reader/writer handles from day one; the executor may run stages
  sequentially with buffered pipes until streaming is needed.
- Wasm isolation only for agent-authored code (JS). Builtins are trusted
  native functions over the VFS.
- No test touches the network or the host filesystem outside temp dirs.

## Testing Strategy

- Unit tests alongside every module; doc tests on the public API.
- Public VFS conformance suite (`tinysandbox::vfs::conformance`) that any
  implementation — first-party or third-party — runs against itself.
- Golden tests for shell parsing and builtin output, with expected values
  matching GNU/bash behavior (captured manually, committed as fixtures).
- Property tests (proptest) for VFS operation sequences and shell lexing;
  fuzz target for the shell parser (no panics on arbitrary input).
- End-to-end pipeline tests through the public `Sandbox` API.
- CI gate per phase: `cargo test --all-features` green, `cargo clippy` clean.

## Phase 1: VFS trait, in-memory backend, conformance suite

Goal:
A stable `Vfs` trait with errno-style errors, a quota-enforcing in-memory
implementation, and a reusable conformance suite proving both.

Scope:
- `Vfs` trait: path ops (stat, readdir, mkdir, rename, unlink, symlink-free
  v1) and handle ops (open with modes, read_at, write_at, truncate, close).
- `VfsError` mapping to POSIX errno codes; `Metadata`, `OpenMode` types.
- `InMemoryVfs` with quotas: total bytes, file count, max file size.
- Conformance suite as a public module taking a `Vfs` factory, covering
  semantics (open modes, offsets past EOF, rename-over-existing, rmdir on
  non-empty, quota exhaustion errors, path normalization, `..` containment).
- Crate scaffolding: modules, lints, CI workflow.

Out of scope:
- Snapshots, slow-backend thread dispatch, permissions/ownership model.

Completion gate:
Conformance suite passes against `InMemoryVfs`; suite is public API usable
by external implementations; property tests over random op sequences pass.

Testing plan:
- Conformance suite run on `InMemoryVfs` in CI.
- proptest: random valid op sequences never panic; quotas never exceeded.
- Unit tests for path normalization and errno mapping.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 1A: `Vfs` trait + error/metadata types | `src/vfs/mod.rs`: trait, errno enum, `Metadata`, `OpenMode`, public `FileHandle::new`. |
| Complete | Work | 1B: `InMemoryVfs` with quota enforcement | `src/vfs/mem.rs`: inode-based handles (POSIX unlink-while-open semantics), dirs count against max_files. |
| Complete | Work | 1C: public conformance suite | `src/vfs/conformance.rs`: 14 cases incl. quota accounting, handle identity, cross-handle visibility; factory takes explicit `VfsQuota`. |
| Complete | Work | 1D: crate scaffolding, lints, CI | `.github/workflows/ci.yml`: test --all-features + clippy -D warnings. |
| Complete | Test | proptest op-sequence and quota invariants | `tests/vfs_proptest.rs`: model-based (HashMap mirror), exact quota accounting asserted per op. |
| Complete | Gate | conformance suite green on `InMemoryVfs` | Reviewer approved after 3 rounds; local `cargo test --all-features` + clippy green. |

## Phase 2: Shell lexer and parser

Goal:
A pure, heavily tested parser for the bash-compatible subset producing an
AST, with bash-identical semantics for the subset and clear errors outside it.

Scope:
- Lexer/parser for: commands + args, single/double quotes, backslash
  escapes, `|`, `> >> <`, `&& || ;`, `$VAR` expansion, `VAR=x cmd` prefixes.
- AST types consumed by the Phase 3 executor.
- Precise parse errors for unsupported constructs (globs, `$(...)`, `&`).

Out of scope:
- Execution, globbing, command substitution, heredocs, job control.

Completion gate:
Golden test corpus (including quoting edge cases verified against real bash
behavior) passes; fuzz target runs without panics.

Testing plan:
- Golden tests: input line -> expected AST/word-split, fixtures documented
  as matching bash.
- Fuzz target (cargo-fuzz or proptest string strategy): no panics.
- Error-message tests for each rejected construct.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 2A: lexer with quoting/escape rules | `src/shell/lex.rs`: bash-rule escapes, newline separators, quoted-literal tracking + unit tests. |
| Complete | Work | 2B: parser to AST (pipes, redirects, lists) | `src/shell/parse.rs`: `Program`/`AndOrList`/`Pipeline`/`Simple`, fd redirects, newline continuation after `&& \|\| \|` + unit tests. |
| Complete | Work | 2C: `$VAR` expansion and assignment prefixes | Segmented words (`Literal{quoted}`/`Expansion`), unquoted-`NAME=` assignment detection, `$?` supported. |
| Complete | Work | 2D: clear errors for unsupported grammar | Positioned errors for globs, `$(...)`, backticks, `&`, subshells, braces, heredocs, tilde, `$'...'`, special params, `+=`, `!`. |
| Complete | Test | golden corpus vs bash behavior | `tests/fixtures/shell_golden.txt` (30+ bash-verified cases) + `tests/shell_golden.rs`. |
| Complete | Test | fuzz/proptest no-panic | `tests/shell_proptest.rs`: metacharacter-biased no-panic + round-trip property. |
| Complete | Gate | corpus + fuzz green | Reviewer approved after 3 rounds; local test + clippy green. |

## Phase 3: Sandbox, executor, builtins

Goal:
The public `Sandbox` API executing real pipelines over the VFS with
session state, limits, and GNU-faithful builtins.

Scope:
- `Command` trait (async `run(ctx)`, stream-shaped stdio handles) and
  registry; custom commands register identically to builtins.
- `Sandbox` builder + session semantics: persistent cwd/env across `exec`,
  `Arc<dyn Vfs>` attachment, direct host access to the VFS.
- Executor: sequential stages with buffered pipes, redirects, exit codes,
  `&& || ;` semantics, `$?`.
- Limits: wall-clock timeout (exit 124), stdout/stderr caps with head+tail
  truncation marker, max command count.
- Builtins: cat, ls, cp, mv, rm, mkdir, touch, echo, pwd, grep, head, tail,
  sort (input-size ceiling), uniq, wc, sed (`s///` subset); shell-level cd,
  which; `/bin` synthesized read-only from the registry.
- Thread dispatch policy for blocking VFS backends (`spawn_blocking` path)
  with a fast inline path for in-memory.
- Observability: `Sandbox::stats()` (VFS bytes/files via optional `Vfs`
  stats method, commands run) and per-exec metrics on `ExecResult` (wall
  time per command/pipeline, pipe byte counts).

Out of scope:
- JS runtime, snapshots, concurrent streaming executor.

Completion gate:
End-to-end tests through `Sandbox::exec` pass, including multi-command
pipelines, session state, limit enforcement, and a custom user command; a
slow fake VFS (sleeping impl) works correctly via the thread-dispatch path.

Testing plan:
- Golden output tests per builtin against GNU-verified fixtures.
- End-to-end pipeline tests (`cat | grep | wc`, redirects, `&&` chains).
- Limit tests: timeout, output truncation, quota errors surfacing as ENOSPC.
- Slow-VFS integration test exercising thread dispatch.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 3A: `Command` trait + registry + `/bin` synthesis | `src/sandbox/command.rs` + `fs.rs` facade; builder panics on reserved names (cd/export/unset). |
| Complete | Work | 3B: `Sandbox` builder/session API | `src/sandbox/mod.rs`: persistent cwd/env, `$PWD`/`OLDPWD`, executing doc test. |
| Complete | Work | 3C: executor (pipes, redirects, lists, exit codes) | Left-to-right fd resolution (bash-correct `2>&1` ordering), preflighted redirect targets, field splitting, `$?`. |
| Complete | Work | 3D: limits (wall clock, output caps, command count) | Timeout → 124, head+tail truncation marker, max command count → 125; all tested. |
| Complete | Work | 3E: builtins with GNU-faithful flags | 18 builtins; grep/sed regex dialect documented as deliberate deviation; GNU exit codes (grep 0/1/2, sed 1/2). |
| Complete | Work | 3F: blocking-VFS thread dispatch | `Vfs::is_fast()` inline path, `spawn_blocking` otherwise; 4-way concurrency test discriminates serial vs parallel. |
| Complete | Work | 3G: `Sandbox::stats()` + `ExecResult` metrics | Stats (VFS bytes/files, commands run) + wall time, per-command timings, pipe bytes; tested. |
| Complete | Test | end-to-end pipeline suite via public API | `tests/sandbox_e2e.rs` + `tests/fixtures/sandbox_builtins_golden.txt` (20+ GNU-verified cases). |
| Complete | Gate | e2e + golden + limit tests green | Reviewer approved after 3 rounds; local test + clippy green. |

## Phase 4: JS runtime (feature `js`, default on)

Goal:
`/bin/js` runs agent scripts under Wasmtime-hosted quickjs-ng with the
Node-compatible `fs` surface, memory/CPU limits, and clean failure modes.

Scope:
- Vendored quickjs-ng `wasm32-wasip1` artifact, `include_bytes!`, compiled
  once into the shared `Engine`, instantiated via `InstancePre`.
- Hostcalls implementing the Node `fs` sync surface (open/close/read_at/
  write_at/stat/readdir/mkdir/rename/rm/truncate/append) over `Arc<dyn Vfs>`.
- JS globals: `console`, `process.argv`, `process.env` (sandbox env),
  stdin/stdout/stderr wiring, exit codes.
- CommonJS `require` subset for built-in `fs` plus relative/absolute `.js`,
  `.json`, and directory `index.js` modules over the VFS.
- Store memory limits (`ResourceLimiter`), epoch-based CPU deadline wired to
  the sandbox wall-clock budget; OOM and timeout surface as script errors /
  exit 124.
- Cargo feature `js` gating wasmtime and the wasm blob.
- Peak wasm memory per execution (observed via `ResourceLimiter`) reported
  in `ExecResult` metrics.

Out of scope:
- Async JS APIs, event loop, `node` alias decision, ESM/import, node_modules,
  package.json, and broader Node module resolution.

Completion gate:
Node-compat script corpus passes (same scripts produce identical output
under real `node` — verified manually, committed as fixtures); OOM, infinite
loop, and quota tests pass; `--no-default-features` build stays green.

Testing plan:
- Script corpus: fs round-trips, random-access read/write at offsets,
  ftruncate, readdir/stat shapes, error codes (ENOENT etc.).
- Adversarial tests: `while(true){}` (epoch trap -> 124), allocation bomb
  (Store limit -> OOM error), quota exhaustion from JS.
- Pipeline integration: `cat data | js transform.js > out`.
- Feature matrix build in CI (`--no-default-features`, `--all-features`).

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 4A: quickjs-ng wasm artifact vendored + engine cache | `assets/quickjs.wasm` built from quickjs-ng `v0.15.1`; `assets/PROVENANCE.md` + `scripts/build-quickjs-wasm.sh`; `src/js/mod.rs` caches `Engine` + `InstancePre`. |
| Complete | Work | 4B: Node `fs` hostcalls over the VFS | `src/js/mod.rs`: JSON hostcall ABI over `Fs`, descriptor table, recursive mkdir/rm, Node-shaped errno errors. |
| Complete | Work | 4C: console/process/stdio wiring, exit codes | `src/js/quickjs_shim.c`: console stdout/stderr, `process.argv/env/cwd/exit`, `require('fs')`; uncaught errors flow to stderr. |
| Complete | Work | 4D: memory + epoch limits, peak-memory metric | `Limits::wasm_memory_bytes`, Wasmtime `ResourceLimiter`, epoch deadline setup, `ExecMetrics::peak_wasm_memory_bytes`; adversarial tests cover timeout and memory cap. |
| Complete | Work | 4E: `js` cargo feature gating | `Cargo.toml`: default `js` feature gates serde/wasmtime/runtime; CI includes no-default build/test. |
| Complete | Work | 4F: CommonJS `require` subset | `src/js/quickjs_shim.c`: VFS-backed path resolution/cache/cycles/module globals; `tests/js_runtime.rs`: Node-verified path, JSON, cache, cycle, error, stack, and depth cases. |
| Complete | Test | Node-compat script corpus | `tests/js_runtime.rs`: console/process Node-verified shape plus VFS-backed fs round trips, descriptor offsets, error codes, quota, pipeline, metrics, timeout/OOM. |
| Complete | Gate | corpus + adversarial + feature matrix green | Reviewer approved after 3 rounds; test --all-features / --no-default-features + clippy green locally. |

## Phase 5: Snapshots, hardening, release readiness

Goal:
Copy-on-write snapshots for rollback/branching, a comprehensive edge-case
coverage sweep across the whole codebase, fuzz/property hardening, and a
documented crate ready for crates.io.

Scope:
- `VfsSnapshot` extension trait: snapshot/restore/branch on `InMemoryVfs`
  (structural sharing so snapshots are cheap).
- Conformance suite extension for snapshot semantics.
- Coverage sweep: audit every module for untested edge cases and close the
  gaps. Systematic sources: `cargo llvm-cov` uncovered branches; error paths
  (every errno arm, every builtin failure message); boundary values (empty
  input, zero-length files, offset/count extremes, quota exactly-at-limit);
  interaction cases (redirects x pipes x limits, expansions in every word
  position, unlink/rename races with open handles, session state across
  failing execs); adversarial input (deep nesting, huge words, invalid
  UTF-8 where accepted). Divergences found against real bash/GNU become
  fixes plus pinned golden cases.
- Fuzzing pass over shell parser and a VFS op fuzzer; fix all findings.
- API polish: rustdoc on all public items, README with embedding example,
  examples/ dir, semver review, `cargo publish --dry-run`.

Out of scope:
- Persistent (disk/SQLite) snapshot storage.
- Coverage-percentage targets for their own sake; the sweep chases real
  edge-case bugs, not a number.

Completion gate:
Snapshot conformance tests pass; coverage sweep documented (gaps found,
bugs fixed, cases added) with `cargo llvm-cov` run as evidence; fuzzers run
clean for a fixed budget; `cargo publish --dry-run` succeeds; README
example compiles as a doc test.

Testing plan:
- Snapshot semantics: mutate-after-snapshot isolation, restore fidelity,
  branch independence, quota accounting across snapshots.
- Coverage sweep cases land in the existing suites (conformance, golden
  corpora, e2e, proptests) so they keep running in CI.
- Fuzz budget run (e.g. 30 min each target) with zero crashes.
- Docs build with `-D warnings`; example compiles in CI.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 5A: `VfsSnapshot` + CoW on `InMemoryVfs` | `snapshot`/`restore`/`branch` with `Arc`-shared content, CoW via `Arc::make_mut` (shrink copies prefix only). Reviewer probe: 256 MiB snapshot 33 us, EBADF on pre-restore handles, quota recomputed. Structural sharing pinned via `Arc::ptr_eq` tests. |
| Complete | Work | 5B: conformance suite snapshot extension | Opt-in `run_snapshots` (non-snapshot impls unaffected): isolation, restore fidelity, branch independence, quota-at-limit. |
| Complete | Work | 5C: comprehensive edge-case coverage sweep | llvm-cov 85.0% -> 87.2% lines; shell/mod.rs 38% -> 100%, builtins 80% -> 86%. Closed: basename paths, echo -e escapes, sed/head/tail mid-stream errors, stat operand, all ParseError displays. Phase 7 follow-ups fixed (handle leaks + regression tests, builtin dedupe, dead fd arms). Deviations documented in builtins module docs (sed exit 2 vs GNU 4, trailing-slash normalization, no Try-help lines). |
| Complete | Work | 5D: fuzz targets + findings fixed | Deviation: cargo-fuzz unavailable (no interactive installs); fallback PROPTEST_CASES=4096 runs of shell + model-based VFS op-sequence proptests, green. Committed fuzz/ dir deferred. |
| Complete | Work | 5E: rustdoc, README, examples, publish dry-run | `#![warn(missing_docs)]` + doc build under `-D warnings`; README included as crate docs, 8 doctests; docs.rs-safe links; CI gains doc + publish dry-run gates; dry-run packages 39 files at 0.3.0. |
| Complete | Work | 5F (added): `persist_session` builder option | Session persistence now off by default (base session per exec, no store-back, race-free concurrent execs); `persist_session(true)` restores prior semantics. Breaking default change -> 0.3.0. |
| Complete | Gate | coverage sweep + fuzz clean + dry-run + docs green | All six gates verified by reviewer; approved after 1 fix round (3 majors + minors). Remaining nits documented as deviations. |

## Phase 6: Node.js bindings

Goal:
An npm package exposing `Sandbox` (exec, direct VFS tool calls, limits) and
supporting VFS implementations written in JavaScript, validated by the same
conformance suite.

Scope:
- Workspace split: `tinysandbox` (core, unchanged public API) and
  `tinysandbox-node` (napi-rs cdylib) + npm package scaffolding.
- Bindings: build sandbox, `exec` (async), direct VFS ops (read/write/
  readdir/stat), limits config, custom JS commands.
- `JsVfs` adapter: implements the sync `Vfs` trait by calling JS callbacks
  through napi threadsafe functions; Rust worker thread blocks on a channel
  until the JS promise resolves. VFS calls never run on the JS main thread.
- Conformance runner exported to JS so third-party JS VFS implementations
  can self-certify.
- Node test suite (node:test) covering exec, tool calls, JS VFS, JS custom
  command, and limit behavior.
- Docs/examples parity: every Rust code example in the README gains an
  equivalent JS version, shown after the Rust one under lower-tier
  sub-headers (e.g. "Rust" / "JavaScript") so both audiences can skim
  their language; every on-disk example in `examples/` gains an equivalent
  runnable JS example (e.g. `tinysandbox-node/examples/quickstart.mjs`,
  `custom_command.mjs`, `js_scripts.mjs`, plus a JS-VFS example), verified
  runnable against the locally built binding. README install/quickstart
  covers both `cargo add` and `npm install`.

Out of scope:
- Prebuilt binary distribution matrix / npm publish automation (document
  the napi build command; publishing pipeline is follow-on work).

Completion gate:
`npm test` green against a locally built binding, including the conformance
suite running against a JS-implemented VFS and an end-to-end
`exec("cat x | js t.js")` from Node; README shows Rust + JS variants for
every example and all on-disk JS examples run against the built binding.

Testing plan:
- node:test suite: sandbox lifecycle, exec output/exit codes, direct VFS
  calls, JS VFS conformance run, wall-clock/memory limit behavior.
- Concurrency test: exec in flight + tool calls serialize correctly.
- Rust-side integration test for `JsVfs` thread-bridge deadlock-freedom.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Incomplete | Work | 6A: workspace split, napi-rs crate, npm scaffold | Missing: `tinysandbox-node/` building locally. |
| Incomplete | Work | 6B: Sandbox/exec/VFS/limits bindings | Missing: binding impl + node:test coverage. |
| Incomplete | Work | 6C: `JsVfs` threadsafe-function adapter | Missing: adapter + deadlock-freedom test. |
| Incomplete | Work | 6D: conformance runner exported to JS | Missing: JS-VFS conformance run green. |
| Incomplete | Work | 6E: README Rust/JS example parity + on-disk JS examples | Missing: JS variants in README + runnable `.mjs` examples. |
| Incomplete | Test | node:test suite incl. e2e pipeline from Node | Missing: `tinysandbox-node/__test__/`. |
| Incomplete | Gate | `npm test` green with JS VFS conformance | Missing: passing run. |

## Phase 7: Streaming pipeline and redirect I/O

Prioritized ahead of Phases 5 and 6: the README advertises bounded-memory
operation over huge files, and this phase makes that true end to end.

Goal:
Pipelines, redirects, and builtins operate in fixed-size chunks with
backpressure, so `cat huge | grep x > out` holds O(chunk) memory per stage
regardless of file size, and early-exiting readers (`head`) stop upstream
producers promptly.

Scope:
- Executor: pipeline stages run as concurrent tasks connected by bounded
  in-memory byte pipes (backpressure via the bounded buffer). `CommandContext`
  keeps its existing `BoxAsyncRead`/`BoxAsyncWrite` shape; only the wiring
  behind it changes.
- Redirects: `< file` streams from the VFS via chunked `read_at`; `>` and
  `>>` stream to the VFS via chunked `write_at` as output is produced
  (preflight create/truncate behavior preserved).
- Closed-pipe semantics: when a downstream stage exits early and closes its
  read end, upstream writes fail and the stage terminates promptly with
  exit 141 (SIGPIPE convention); the pipeline status remains the last
  stage's status. No deadlocks, no stray error output.
- Builtins stream where semantics allow: `cat`/`grep`/`head`/`wc`/`sed`/
  `uniq` process input incrementally (line- or chunk-at-a-time); `tail`
  keeps only its window; `sort` necessarily buffers its full input
  (documented exception).
- Captured stdout/stderr respect `Limits::stdout_bytes`/`stderr_bytes`
  while streaming: accumulation stops at the cap (truncated flag set),
  excess is drained and counted, never buffered.
- Metrics keep working: `pipe_bytes` counts bytes through each pipe;
  per-command timings may now overlap (documented).
- Shell builtins that mutate session state (`cd`, `export`, `unset`) match
  bash: standalone (single-stage) invocations mutate the session inline;
  inside a multi-stage pipeline they behave like bash subshell members (no
  session mutation), locked in by tests. Assignment-only pipeline stages
  likewise don't persist, but must still consume their stdin and close
  their stdout so pipe topology is preserved.
- `js` keeps its semantics; its stdout hostcalls flow through the streaming
  writer.

Out of scope:
- Changing the `Vfs` trait or the public `Command` trait.
- Concurrent execution across `&&`/`||`/`;` list items.
- bash `pipefail` or job control.

Correctness invariants:
- All existing e2e/golden/conformance tests pass unchanged (tests asserting
  internal buffering details may be updated with justification).
- Every pipe writer is closed when its stage finishes; every reader is
  drained or closed: no deadlock for any pipeline shape times limit
  combination.
- Bounded memory: bytes buffered per pipe never exceed the fixed pipe
  capacity; builtin working memory is O(chunk) or O(window), except sort.

Completion gate:
A counting/generating VFS test proves streaming: a virtual multi-GiB file
(content generated in `read_at`, never stored) piped through
`cat huge | head -n 1` completes with the VFS serving only a small prefix
(assert bytes-served counter), and `wc -c < huge > out`-style full scans
complete with bounded pipe capacity. Full suite + clippy green.

Testing plan:
- Generating-VFS tests: early-exit byte-count assertion (`head` stops
  `cat`); full-scan correctness (`wc -c`, `grep -c`) over data far larger
  than any buffer; streamed redirect write producing correct file content.
- Deadlock regressions under a test timeout: slow consumer + fast producer,
  producer exceeding pipe capacity, stage failing mid-stream, limit_hit
  mid-pipeline.
- Closed-pipe: exit-141 stage status, pipeline status from last stage,
  clean stderr.
- Output-cap streaming: stdout beyond `stdout_bytes` sets truncated without
  unbounded buffering.
- Existing suites re-run green (behavioral compatibility).

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 7A: concurrent pipeline stages over bounded pipes | 64 KiB duplex pipes, JoinSet abort-on-drop for wall-time teardown. |
| Complete | Work | 7B: chunked streaming redirects (read + write) | Custom `AsyncRead` over `read_at` (surfaces mid-stream errors); chunked `write_at` sinks. |
| Complete | Work | 7C: incremental builtins + closed-pipe semantics | cat/grep/head/tail/wc/sed/uniq stream (bounded line cap, documented); sort documented exception; exit 141 with clean stderr. |
| Complete | Work | 7D: streaming-aware output caps + metrics | `CappedOutput` byte-identical to old truncation; pipe_bytes via pipe counters. |
| Complete | Test | generating-VFS byte-count + deadlock + 141 suites | `tests/streaming.rs`: 14 tests incl. 4 GiB virtual file served <= 2 MiB for `head -n 1`, timeout-abort, mid-stream read errors, pipeline-subshell semantics. |
| Complete | Gate | full suite + clippy green with streaming proofs | 84 tests green all-features + no-default-features, clippy `-D warnings` clean. Reviewer approved after 1 fix round (1 blocker + 7 majors fixed); minor follow-ups noted: error-path handle leaks, duplicated shell-builtin fn. |

## Phase 8: Release automation for crates.io and npm

Runs after Phase 6 (the npm side publishes the `tinysandbox-node` package it
creates). Mirrors the release pipeline in the sibling `durust` repo.

Goal:
Merges to `main` automatically version and publish the crate to crates.io
and the Node package to npm, in lockstep, with no manual release steps.

Scope:
- `scripts/release-version.mjs` (adapted from durust): `next` computes the
  next version from the current lockstep version and the bump signal
  (`#major`/`#minor` in the commit message, else patch; `workflow_dispatch`
  input can override); `apply` writes it to every manifest (root
  `Cargo.toml`, `tinysandbox-node/Cargo.toml`, npm `package.json`s);
  `check` verifies lockstep agreement.
- `.github/workflows/release.yml` (adapted from durust): triggers on CI
  success on `main` plus manual `workflow_dispatch` with a bump choice;
  eligibility guard skips `chore(release):` commits and `[skip release]`
  markers; commits the version bump back to `main` as
  `chore(release): X.Y.Z [skip release]`; publishes crates with
  already-published idempotency checks (curl the crates.io API before
  `cargo publish --locked`), waiting for dependency crates to become
  visible before publishing dependents; publishes npm packages via OIDC
  trusted publishing with `npm view` idempotency checks.
- CI workflow gains any missing release-blocking gates so "CI green on
  main" is a trustworthy release trigger (doc build and publish dry-run
  land in Phase 5).
- Security posture, enforced by construction: pull requests run the test
  workflow only — no secrets, no publish steps, no version commits (the
  publish dry-run needs no token). Versioning and deployment happen
  exclusively on push to `main` (direct or merged PR) via the
  `workflow_run`-on-CI-success trigger, which never fires for PR runs.
  Release-workflow permissions stay minimal (`contents: write`,
  `id-token: write`); the test workflow keeps default read-only
  permissions.
- Secrets/settings documented in the workflow header comment:
  `CARGO_REGISTRY_TOKEN`, npm trusted-publisher configuration for the
  package, branch-protection interaction with the release commit, and the
  Actions approval policy (already set: workflow runs from all external
  contributors require approval from a maintainer).

Out of scope:
- Prebuilt native binary matrices for the Node package beyond what Phase 6
  established; changelog generation; GitHub Releases/tags beyond what the
  version commit provides.

Completion gate:
A merge to `main` with CI green produces a version-bump commit and both
registries showing the new version (or idempotently skips when nothing
changed); a `workflow_dispatch` with an explicit bump works; a
`[skip release]` commit does not release; a PR run executes tests only,
with read-only permissions and no access to publish secrets.

Testing plan:
- `release-version.mjs` unit-tested (node:test) for bump parsing, lockstep
  application, and disagreement detection.
- Dry-run exercise of the workflow steps locally (script `next`/`apply`/
  `check` roundtrip, `cargo publish --dry-run`, `npm publish --dry-run`).
- First real release observed end to end on both registries.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Incomplete | Work | 8A: `scripts/release-version.mjs` lockstep versioning | Missing: script + unit tests. |
| Incomplete | Work | 8B: `release.yml` auto-release workflow | Missing: workflow. |
| Incomplete | Work | 8C: CI gates sufficient as release trigger | Missing: audit vs release requirements. |
| Incomplete | Test | script unit tests + dry-run roundtrip | Missing: tests + logs. |
| Incomplete | Gate | end-to-end auto-release on both registries | Missing: observed release. |
