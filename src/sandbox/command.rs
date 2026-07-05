//! Custom command interfaces for sandbox execution.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};

use super::fs::Fs;
#[cfg(feature = "js")]
use super::syscall::Syscall;

/// Future returned by sandbox commands.
pub type CommandFuture = Pin<Box<dyn Future<Output = CommandResult> + Send>>;
/// Boxed asynchronous reader used for command stdin.
pub type BoxAsyncRead = Pin<Box<dyn AsyncRead + Send>>;
/// Boxed asynchronous writer used for command stdout and stderr.
pub type BoxAsyncWrite = Pin<Box<dyn AsyncWrite + Send>>;

/// Command implementation that can run inside a [`crate::sandbox::Sandbox`].
pub trait Command: Send + Sync {
    /// Runs the command with its execution context.
    fn run(&self, ctx: CommandContext) -> CommandFuture;
}

impl<F, Fut> Command for F
where
    F: Fn(CommandContext) -> Fut + Send + Sync,
    Fut: Future<Output = CommandResult> + Send + 'static,
{
    fn run(&self, ctx: CommandContext) -> CommandFuture {
        Box::pin(self(ctx))
    }
}

/// Inputs and capabilities passed to a command invocation.
pub struct CommandContext {
    /// Positional command arguments, excluding the command name.
    pub args: Vec<String>,
    /// Environment visible to this command.
    pub env: BTreeMap<String, String>,
    /// Current working directory.
    pub cwd: String,
    /// Command stdin.
    pub stdin: BoxAsyncRead,
    /// Command stdout.
    pub stdout: BoxAsyncWrite,
    /// Command stderr.
    pub stderr: BoxAsyncWrite,
    /// Filesystem facade rooted at the sandbox VFS and current directory.
    pub fs: Fs,
    /// Resource limits for this execution.
    pub limits: Limits,
    /// Names available through `/bin` and command lookup.
    pub commands: Arc<BTreeSet<String>>,
    /// JavaScript syscalls registered on the sandbox.
    #[cfg(feature = "js")]
    pub js_syscalls: Arc<BTreeMap<String, Arc<dyn Syscall>>>,
    /// JavaScript prelude evaluated before each user script.
    #[cfg(feature = "js")]
    pub js_prelude: Arc<str>,
}

/// Exit status and optional resource metrics returned by a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandResult {
    /// Process-like exit code.
    pub exit_code: i32,
    /// Peak WebAssembly memory observed by JS commands.
    pub peak_wasm_memory_bytes: Option<usize>,
}

impl CommandResult {
    /// Creates a result with an exit code and no extra metrics.
    pub const fn new(exit_code: i32) -> Self {
        Self {
            exit_code,
            peak_wasm_memory_bytes: None,
        }
    }

    /// Attaches peak WebAssembly memory usage to the result.
    pub const fn with_peak_wasm_memory(mut self, bytes: usize) -> Self {
        self.peak_wasm_memory_bytes = Some(bytes);
        self
    }

    /// Returns a successful zero-exit result.
    pub const fn success() -> Self {
        Self::new(0)
    }

    /// Returns a conventional failure result with exit code 1.
    pub const fn failure() -> Self {
        Self::new(1)
    }
}

/// Resource limits enforced for sandbox execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum wall-clock time for one `exec`.
    pub wall_time: Duration,
    /// Maximum captured stdout bytes before truncation.
    pub stdout_bytes: usize,
    /// Maximum captured stderr bytes before truncation.
    pub stderr_bytes: usize,
    /// Maximum simple commands that one parsed program may execute.
    pub max_commands: usize,
    /// Maximum bytes accepted by `sort` before it fails.
    pub sort_input_bytes: usize,
    /// Maximum WebAssembly memory for JS commands.
    pub wasm_memory_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_time: Duration::from_secs(30),
            stdout_bytes: 1024 * 1024,
            stderr_bytes: 1024 * 1024,
            max_commands: 1024,
            sort_input_bytes: 8 * 1024 * 1024,
            wasm_memory_bytes: 64 * 1024 * 1024,
        }
    }
}
