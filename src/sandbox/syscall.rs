//! Embedder syscall interfaces for sandboxed JavaScript.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

/// Future returned by sandbox syscalls.
pub type SyscallFuture = Pin<Box<dyn Future<Output = Result<Value, SyscallError>> + Send>>;

/// Host syscall implementation callable from sandboxed JavaScript.
pub trait Syscall: Send + Sync {
    /// Runs the syscall with the guest-provided JSON argument.
    fn call(&self, args: Value) -> SyscallFuture;
}

impl<F, Fut> Syscall for F
where
    F: Fn(Value) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Value, SyscallError>> + Send + 'static,
{
    fn call(&self, args: Value) -> SyscallFuture {
        Box::pin(self(args))
    }
}

/// Error returned by an embedder syscall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyscallError {
    /// Human-readable error message exposed as the JavaScript `Error.message`.
    pub message: String,
    /// Optional machine-readable code exposed as the JavaScript `Error.code`.
    pub code: Option<String>,
}

impl SyscallError {
    /// Creates a syscall error with no code.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
        }
    }

    /// Attaches a machine-readable error code.
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

impl std::fmt::Display for SyscallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SyscallError {}
