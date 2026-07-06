//! Prompt chunks for agent system prompts.
//!
//! Each constant is a short, self-contained block of prompt text describing
//! one part of the sandbox. They assume the model already knows bash, GNU
//! coreutils, jq, and Node, and only state where this environment differs.
//! Pick the chunks that match your sandbox configuration and join them with
//! blank lines:
//!
//! ```
//! let system_prompt = [
//!     tinysandbox::prompts::OVERVIEW,
//!     tinysandbox::prompts::SHELL,
//!     tinysandbox::prompts::BUILTINS,
//!     tinysandbox::prompts::SESSION_EPHEMERAL,
//!     tinysandbox::prompts::JS,
//! ]
//! .join("\n\n");
//! # assert!(system_prompt.contains("bash"));
//! ```
//!
//! Skip [`JS`] when the `js` feature is disabled, [`SYSCALLS`] when no
//! syscalls are registered, and [`FETCH`] when no fetch handler is set.
//! Include exactly one of [`SESSION_EPHEMERAL`] or [`SESSION_PERSISTENT`]
//! depending on the builder's `persist_session` setting.

/// What the environment is and its hard boundaries.
pub const OVERVIEW: &str = "You are working in a minimal Linux-like sandbox: a bash-compatible shell over a virtual filesystem. Files persist across commands. There is no host filesystem, no other processes, no package manager, and no network access beyond capabilities described here.";

/// The supported shell subset and what fails to parse.
pub const SHELL: &str = "The shell supports the common bash subset: pipelines, `&&`/`||`/`;`, redirects (`>`, `>>`, `<`, `2>`, `2>>`, `2>&1`), single/double quotes, backslash escapes, and variables (`$VAR`, `${VAR}`, `$?`, `VAR=x cmd`, `export`, `unset`). Behavior inside this subset matches bash. Globs, command substitution (`$(...)`, backticks), heredocs, `&`, subshells, and brace/tilde expansion are not supported and fail with a clear parse error.";

/// The available commands. The `js` command is introduced by [`JS`] instead
/// so this chunk stays accurate for sandboxes built without the `js` feature.
pub const BUILTINS: &str = "Available commands, matching their GNU counterparts for supported flags:
cat cd cp echo export false grep head jq ls mkdir mv pwd rm sed sort stat tail touch true uniq unset wc which
Unsupported flags fail with a clear error rather than being silently ignored. `grep` and `sed` use Rust regex syntax instead of POSIX BRE/ERE (notably: no backreferences). `ls /bin` lists every command; `/bin` is read-only.";

/// The supported jq CLI subset.
pub const JQ: &str = "`jq` supports a common subset: `-r`, `-j`, `-c`, `-e`, `-n`, `-s`, `-S`, `--tab`, `--indent N`, `--arg`, `--argjson`, file operands, and `-` for stdin. `def`, modules, and flags outside this list are not supported and fail loudly.";

/// The `js` command and its Node-compatible runtime, including the `fs` API.
pub const JS: &str = "`js script.js [args...]` and `js -e 'code'` run JavaScript with Node-compatible semantics for what it implements:
- `require()` for relative/absolute CommonJS modules (`./x`, `/x`, `.js`/`.json` inference, `dir/index.js`). There is no npm and no `node_modules`.
- The sync `fs` API over the same filesystem as the shell: readFileSync, readLinesSync (UTF-8 line iterator, 64KB buffer), writeFileSync, appendFileSync, mkdirSync, readdirSync, statSync, renameSync, rmSync, unlinkSync, rmdirSync, existsSync, copyFileSync, openSync, readSync, writeSync, ftruncateSync, closeSync. Errors are Node-shaped (`err.code === 'ENOENT'` etc.).
- `console`, `process.argv/env/cwd()/exit()`, `Buffer`, `__filename`, `__dirname`.
There are no timers and no event loop: only already-settled promise callbacks run before exit. Scripts run under memory and CPU limits; an infinite loop exits with code 124.";

/// Host syscalls exposed to sandboxed JavaScript. Include only when syscalls
/// are registered.
pub const SYSCALLS: &str = "Inside `js`, host-provided functions are available as synchronous `sandbox.<name>(args)` calls that take and return one JSON value. `Object.keys(sandbox)` lists them. Failures throw a normal `Error`, with `err.code` set when the host provides one.";

/// The `fetch` capability inside sandboxed JavaScript. Include only when a
/// fetch handler is registered.
pub const FETCH: &str = "Inside `js`, `fetch(url, options)` is available with a WHATWG subset: `Headers`, `Response`, and body helpers `text()`/`json()`/`arrayBuffer()`. Streams, `AbortController`, and automatic redirects are not supported. Requests are served by a host-defined handler, which decides what URLs are reachable.";

/// Session behavior with the default per-exec cwd/env reset.
pub const SESSION_EPHEMERAL: &str = "Each command you run starts from the same initial working directory and environment: `cd` and `export` do not carry over to later commands, so chain them on one line (`cd /workspace && ls`) or use absolute paths. Filesystem changes do persist.";

/// Session behavior when the builder enables `persist_session`.
pub const SESSION_PERSISTENT: &str = "The working directory and environment persist across the commands you run: `cd` and `export` carry over to later commands.";
