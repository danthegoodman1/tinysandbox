# Architecture and correctness improvement plan

Baseline: `985fba5` (0.6.6), reviewed and implemented 2026-09-04. These stable
phase identifiers continue [PLAN.md](PLAN.md). The historical ledgers there
record earlier validation; this document tracks the additional findings.

## Overarching goal

Preserve explicit capabilities, independent runs, a small shell/Node subset,
and efficient embedded execution while repairing containment, ownership,
streaming, admission, and verification boundaries. Keep the parser/VFS/builtin/
QuickJS decomposition. Change APIs where the actual contract requires it.

## Implementation principles

- Anchor local paths to opened directories and preserve open-file identity.
- Carry one deadline and cancellation scope through execution. Distinguish a
  returned timeout from completion of already-running trusted host work.
- Own handles across asynchronous handoffs; finish successful staged output and
  abort canceled or failed transfers. Custom backends retain the synchronous VFS
  interface and can override the new default `abort` method.
- Apply capture truncation only to returned captures. Share real redirect
  descriptions and bounded streams; preserve byte order and backpressure.
- Admit source, expanded pipelines, host reads, retained buffers, handles, and
  workers before allocating or dispatching the corresponding work.
- Prefer a measured local change over a broad rewrite. Do not equate Wasm
  limits with total-process heap limits or preemption of trusted host code.

## Testing strategy

Permanent adversarial tests use counters, gated backend operations, independent
Bash/Node references, exact bytes, and fake-S3 request/state observations.
Run core-only no-feature/default tests separately from the all-feature workspace
(including doctests), strict Clippy/rustdoc, native Node tests, strict portable
TypeScript plus generated declarations, browser/Convex/package checks, supported
Linux/macOS builds, and real S3 compatibility in CI. Benchmarks have no flaky
CI timing thresholds; [measurements](benchmarks/RUNTIME.md) state their limits.

## Findings and implemented outcomes

The review distinguished reproduced defects from worthwhile improvements.
F01 required external host namespace mutation; F02 was verified against the
Wasmtime safety contract without executing corrupted native artifacts. F11's
scheduling overhead was measured. Native jq's limitations were already known;
they motivated an explicit policy, not an invented new escape.

| ID | Finding | Outcome |
| --- | --- | --- |
| F01 | Replacing the local root could expose a sibling sentinel. | Descriptor-relative traversal, no symlinks/special files, race regressions, precise host-mutation contract. |
| F02 | Safe Rust accepted bytes passed to unsafe native deserialization; initializer could publish the wrong ticker. | Unsafe artifact API, documented Node trust boundary, serialized initialization and epoch regression. |
| F03 | A JS command could start a new write after exec timed out; synchronous preparation could outlive its budget and report success. | Shared cancellation/deadline, worker admission, source/expansion gates, cancellation checkpoints and late-write tests. |
| F04 | Unclosed JS descriptors pinned unlinked data; canceled opens/sinks lost ownership. | Scoped registries, RAII handoff, success close/failure abort, retained-capability and dropped-future tests. |
| F05 | Both JS hosts clamped fd reads by a reused pathname and returned zero instead of six bytes. | Handle-based reads with independent real-Node comparison. |
| F06 | A 128-byte capture cap changed 4097 pipeline/file bytes to 129, including NULs. | Bounded acknowledged JS transport; capture only at the destination; exact-byte/early-close tests. |
| F07 | Redirect preflight/reopen produced ACB for ABC and broke S3 replacement/merged output. | Open once, share offset and one finish, preserve replacement mode, own queued bytes across write cancellation. |
| F08 | Sort read before enforcing its cap; whole reads, expansion, null stages, and worker admission had gaps. | Incremental/cumulative budgets, bounded adapters/serialization, all-stage admission, isolated jq evaluator (Phase 22). |
| F09 | Null redirects were skipped; `$?` was stale within and/or lists; null stages drained input. | Shared redirect preparation/finish, immediate status propagation and input close, effect-aware Bash tests. |
| F10 | Recursive copy into itself grew until quota; tree/copy paths were recursive or whole-file. | Normalized topology rejection, streamed copy, iterative traversal, depth 256 snapshot/tree policy. |
| F11 | An unused slow mount forced memory operations through blocking dispatch; buffers repeatedly moved/scanned prefixes. | Resolved-backend dispatch, direct bounded JS workers, cursor/ring buffers, reproducible release measurements. |
| F12 | Workspace feature unification hid broken doctests; both JS hosts shared bugs; portable types were unchecked. | Package-isolated gates, independent oracles, strict implementation checking/generated declarations, deterministic release tests. |

## Decisions and compatibility

- Rust `js::use_precompiled` is now unsafe. Node's host-only loader explicitly
  requires authentic, compatible machine-code artifacts. Examples were migrated.
- New `Limits` fields and Node options bound shell source, host inputs, open
  files, path depth, and retained tail bytes. A pipeline's cumulative expansion
  budget is the larger of shell-source and host-input caps. Whole-file host
  facade calls use default caps; handles support streaming larger files.
- Timeout124 discards partial captures/session changes and cancels capabilities.
  Already-running trusted callbacks/VFS calls may finish; effects are not rolled
  back. Cleanup can outlive the result. No public "all workers finished" API is
  promised. JS/jq workers each have 16 slots, VFS dispatch 128, fallback cleanup 4;
  open handles have a 16,384 process ceiling and 1,024 default per-exec ceiling.
- Phase 22 replaces native jaq execution with an embedded jaq Wasm guest: parser,
  compiler, evaluator, and serializer share one capped linear memory and an
  engine-interrupted deadline. Wasmtime is now required even without `js`;
  `without_command("jq")` remains available. Input buffering precedes jq admission
  to avoid pipeline deadlock and is capped per command, not process-wide.
- Trusted host callbacks receive cooperative deadline/cancellation context.
  Arbitrary host allocations and synchronous blocking code require application
  limits or external process isolation; no new hard host-code guarantee.
- Local roots are stable capabilities. A host rename does not revoke an opened
  directory. Host-created hard links and mounted outside content are unsupported
  isolation inputs; external mutations can invalidate quota accounting.
- Retain a dedicated single-command path for parent-session shell builtins.
  Share redirect preparation and admission, and pin intentional session behavior
  with differential tests. A wholesale executor rewrite adds no needed guarantee.

## Phase 14: Repair containment and artifact trust boundaries

Goal: Keep local traversal anchored and make native artifact trust explicit.

Scope: 14A, 14B, 14C.

Completion gate: Local/conformance, initialization and doctest suites pass; Linux containment also runs in CI.

Testing plan: Maintain the named regressions below and run the shared configuration matrix.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 14A: Descriptor-anchored local backend (F01) | `src/vfs/local.rs`, safe Unix rustix dependency; `tests/vfs_local.rs` covers root/ancestor replacement, sockets/FIFOs, depth and quota. |
| Complete | Work | 14B: Explicit artifact trust API (F02) | `src/js/mod.rs`, `tinysandbox-node/src/lib.rs`, migrated Rust/README examples; compile-fail doctest rejects safe calls. |
| Complete | Work | 14C: Atomic runtime/ticker initialization | Serialized initialization; controlled epoch-interruption unit test plus precompiled integration test. |
| Complete | Test / Gate | Phase 14 validation | Local/conformance, initialization and doctest suites pass; Linux containment also runs in CI. |

## Phase 15: Own executions and open resources through teardown

Goal: End guest capabilities at completion/cancellation and release open resources even when a result is abandoned.

Scope: 15A, 15B, 15C, 15D.

Completion gate: Gated late-open, dropped-host-open, retained-Fs, redirect cancellation/flush, JS teardown and fake-S3 tests pass.

Testing plan: Maintain the named regressions below and run the shared configuration matrix.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 15A: Execution scope and deadline propagation (F03) | `src/sandbox/control.rs`; `js_does_not_start_a_filesystem_mutation_after_the_exec_deadline`; canceled Fs calls reject new work. |
| Complete | Decision | 15B: Timeout, pending work and cleanup contract | README Limits/security sections distinguish result delivery, trusted in-flight effects, fixed admission and eventual cleanup. |
| Complete | Work | 15C: Owned handle and finish/abort lifecycle (F04) | `src/sandbox/fs.rs` registry/OpenHandoff; eight `tests/resource_lifecycle.rs` regressions; S3 abort/cancellation tests. |
| Complete | Work | 15D: Handle-based reads in both JS hosts (F05) | Rust JS identity test and portable real-Node oracle cover rename/path reuse; teardown tests cover success/error/timeout. |
| Complete | Test / Gate | Phase 15 validation | Gated late-open, dropped-host-open, retained-Fs, redirect cancellation/flush, JS teardown and fake-S3 tests pass. |

## Phase 16: Use real byte streams and shared redirect destinations

Goal: Preserve bytes, ordering, backpressure and storage mode through composition.

Scope: 16A, 16B, 16C.

Completion gate: Existing streaming/e2e and 45 fake-S3 tests pass. Real S3 compatibility is a CI gate.

Testing plan: Maintain the named regressions below and run the shared configuration matrix.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 16A: Streaming JS output with terminal-only capture (F06) | Capacity-one acknowledged transport with 16 KiB chunks; 4,097-byte file/pipe regression, early-reader exit and capture tests. |
| Complete | Work | 16B: Shared open redirect descriptions (F07) | `RedirectFile` shares one handle/offset and one queued chunk; gated canceled-write/flush tests prevent byte misattribution; duplicated pipes fan out readiness and preserve independent shutdown. |
| Complete | Work | 16C: Preserve S3 write mode and single finish (F07) | Fake-S3 tests prove ABC merging, 31-byte replacement above 16-byte edit cap, multipart, null/superseded redirects and abort. |
| Complete | Test / Gate | Phase 16 validation | Existing streaming/e2e and 45 fake-S3 tests pass. Real S3 compatibility is a CI gate. |

## Phase 17: Bound work before allocation and evaluation

Goal: Reject oversized work during admission or consumption and state the limits of native evaluation.

Scope: 17A, 17B, 17C, 17D.

Completion gate: Adversarial generated input, expansion, retention, serialization and admission regressions pass; Phase 22 extends these admission bounds to the evaluator itself.

Testing plan: Maintain the named regressions below and run the shared configuration matrix.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 17A: Parser, expansion, stage and loop budgets (F03/F08) | `execution_boundaries.rs`: all-stage/no-pipe rejection, cumulative pipeline expansion, variable doubling/field amplification, assignment-aware redirect admission and no redirect mutations. |
| Complete | Work | 17B: Bounded host reads, retention and serialization (F08) | Incremental sort, retained tail byte ring, bounded whole-file helpers, Rust/portable host transfer tests, streamed sed and jq serialization. |
| Complete | Decision | 17C: Bounded worker and jq policy | Independent 16-slot JS/jq pools retain permits through worker exit; jq admission test covers full-pool timeout and a one-slot pipeline. README states admission limits and exclusion API; Phase 22 replaces native evaluation. |
| Complete | Work | 17D: Capped Node command adapter | `hostInputBytes` limits input before callback and output before transport; Node tests verify rejection, exact cap, no callback on oversized input, and pre-allocation raw read checks. |
| Complete | Test / Gate | Phase 17 validation | Adversarial generated input, expansion, retention, serialization and admission regressions pass; Phase 22 extends these admission bounds to the evaluator itself. |

## Phase 18: Unify supported simple-command semantics

Goal: Match Bash for supported null commands, assignments, redirects, status and parent-session effects.

Scope: 18A, 18B.

Completion gate: Twelve independent Bash scripts compare status, stdout/stderr, file contents and persistent session effects; existing shell/e2e/streaming suites pass.

Testing plan: Maintain the named regressions below and run the shared configuration matrix.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 18A: Shared preparation and session policy (F09) | Single and pipeline paths share redirect preparation; null assignments persist on redirect failure as Bash does; parent-session builtin path intentionally retained. |
| Complete | Work | 18B: Status propagation and null-stage closure (F09) | Status updates after each pipeline; null stages drop stdin immediately; command budget includes null/assignment stages. |
| Complete | Test / Gate | Phase 18 validation | Twelve independent Bash scripts compare status, stdout/stderr, file contents and persistent session effects; existing shell/e2e/streaming suites pass. |

## Phase 19: Make tree operations safe and bounded

Goal: Reject invalid copy topology and bound file/traversal resources.

Scope: 19A, 19B, 19C.

Completion gate: Builtin limit/topology and VFS conformance/property/depth suites pass, including failure cleanup.

Testing plan: Maintain the named regressions below and run the shared configuration matrix.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 19A: Reject invalid copy topology (F10) | `copy_rejects_normalized_self_and_descendant_targets_without_mutation` includes normalized same/descendant destinations. |
| Complete | Work | 19B: Streamed copy and bounded traversal | 64KiB copy with short-write/close/abort paths; iterative copy/remove/grep/local scan; large generated-copy and tree tests. |
| Complete | Work | 19C: Snapshot/tree depth policy | Shared normalized-path hard ceiling256; near-ceiling snapshot/clone/branch/restore/drop test plus configurable exec-depth test. No unsupported unlimited-depth promise. |
| Complete | Test / Gate | Phase 19 validation | Builtin limit/topology and VFS conformance/property/depth suites pass, including failure cleanup. |

## Phase 20: Remove measured scheduling and buffering overhead

Goal: Remove verified overhead while preserving the synchronous backend API and byte semantics.

Scope: 20A, 20B, 20C.

Completion gate: Reproducible optimized measurements, deterministic dispatch assertions, and streaming/builtin regressions support the changes.

Testing plan: Maintain the named regressions below and run the shared configuration matrix.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 20A: Dispatch by resolved backend (F11) | Per-path/per-handle defaults and MountedVfs forwarding; thread-ID test and release 10k stats 7.13 ms vs 7.11 ms with unused local mount. |
| Complete | Work | 20B: One bounded JS execution facility | Direct stack-sized worker plus oneshot/admission replaces nested blocking-thread join; JS output/deadline/early-close suites pass. |
| Complete | Work | 20C: Cursor/ring I/O buffers | Line/stream reader cursors, retained tail/capture rings and incremental newline scans; `examples/runtime_benchmark.rs` and `benchmarks/RUNTIME.md` record isolated results without throughput claims. |
| Complete | Test / Gate | Phase 20 validation | Reproducible optimized measurements, deterministic dispatch assertions, and streaming/builtin regressions support the changes. |

## Phase 21: Make verification prove the supported product

Goal: Exercise real feature configurations, independently compare semantics, and ship checked public types.

Scope: 21A, 21B, 21C, 21D, 21E.

Completion gate: All local configuration checks must pass; PR CI must cover Linux/macOS, browser and real S3 before merge.

Testing plan: Maintain the named regressions below and run the shared configuration matrix.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 21A: Genuine feature-isolation gates (F12) | CI selects core-only no-default/default features; hidden feature gates fix seven README doctests; macOS runs local/conformance tests. |
| Complete | Work | 21B: Composition plus independent reference corpus | Bash effect corpus, portable real-Node fd oracle, S3 composition and gated lifecycle regressions. |
| Complete | Work | 21C: Checked implementation and public type contract | Removed blanket ts-nocheck/manual portable declarations; strict tsc emits JS/declarations from implementation/interfaces.26 portable tests, Convex and package smoke pass. |
| Complete | Doc | 21D: Precise guarantees and current plan evidence | README documents cancellation, staging, isolated jq, local capability assumptions, all new limits and artifact trust; this ledger and benchmark evidence replace prospective claims. |
| Complete | Work | 21E: Deterministic release-test boundary | Offline fresh-cache placeholder test; live registry smoke separately opt-in and bounded. 31 deterministic release tests pass; live smoke also verified. |
| Complete | Test / Gate | Phase 21 validation | [CI run 33929163678](https://github.com/danthegoodman1/tinysandbox/actions/runs/33929163678) passed all seven jobs for implementation commit `e4c1bfd`: Rust release gates, Linux x64/arm64, macOS arm64/Intel, portable Chrome/Convex, and live S3. [PR #24](https://github.com/danthegoodman1/tinysandbox/pull/24) tracks subsequent checks. |


## Phase 22: Isolate jq and expose cooperative host cancellation

Goal: Enforce jq memory and computation boundaries while giving trusted host
callbacks the deadline and cancellation context needed to stop their own work.

Scope: 22A guest/runtime, 22B callback APIs, 22C packaging and compatibility.

Completion gate: Hostile intermediate allocations fail within the guest cap;
engine cancellation terminates an entered no-output evaluation; existing jq
semantics and legacy callback signatures pass; PR CI is green.

Testing plan: Guest evaluator tests, end-to-end CLI/resource cases, actual worker
exit after cancellation, callback cancellation/drop races, native/portable tests,
locked artifact rebuild, optimized before/after measurement, and full CI matrix.

Status ledger:

| Status | Type | Item | Evidence / Gap |
| --- | --- | --- | --- |
| Complete | Work | 22A: Capped, preemptible jq guest | `guests/jq`, `jq_protocol.rs`, `jq_runtime.rs`, `build.rs`; 15 guest tests, 5 real-Wasm ABI tests, CLI/e2e/streaming suites, 32MiB allocation/recovery test, entered-worker cancellation and unpolled-output deadline tests pass. |
| Complete | Work | 22B: Trusted host callback context | `tests/host_context.rs` (6 tests), cancellation race unit tests, native callback tests and 29 portable tests verify deadlines, execution drop, settlement, legacy signatures, and no effects from expired queued callbacks. |
| Complete | Work | 22C: Reproducible artifact, API and resource documentation | The canonical Linux x86_64 build and pinned Docker reproduction are documented in `guests/jq/README.md`; `benchmarks/JQ_ISOLATION.md` records measured 1.25–1.65× latency and guest memory; README documents required Wasmtime, UTC/empty-env semantics, caps and cooperative host limits. |
| Complete | Test / Gate | Phase 22 validation | [CI run 33935000244](https://github.com/danthegoodman1/tinysandbox/actions/runs/33935000244) passed all eight jobs on implementation commit `3c3feed`: Rust feature matrix/Clippy/rustdoc/package, native Linux x64/arm64 and macOS arm64/Intel, live S3, portable Chrome/Convex, and the canonical byte-for-byte jq rebuild. Local checks include 56 native and 29 portable tests, 6 Rust context tests, guest memory/worker/backpressure regressions and both package smoke checks. [PR #24](https://github.com/danthegoodman1/tinysandbox/pull/24) tracks checks on subsequent documentation revisions. |
