//! Embedder syscall interfaces for sandboxed JavaScript.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

/// Future returned by sandbox syscalls.
pub type SyscallFuture = Pin<Box<dyn Future<Output = Result<Value, SyscallError>> + Send>>;
/// Future returned by sandbox fetch handlers.
pub type FetchFuture = Pin<Box<dyn Future<Output = Result<FetchResponse, SyscallError>> + Send>>;

/// Request passed to an embedder-provided JavaScript `fetch` handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    /// Absolute or relative URL string supplied by the guest.
    pub url: String,
    /// HTTP method normalized by the guest fetch glue.
    pub method: String,
    /// Request headers as normalized `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Optional request body bytes.
    pub body: Option<Vec<u8>>,
}

/// Response returned by an embedder-provided JavaScript `fetch` handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

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

/// Host transport implementation backing sandboxed JavaScript `fetch`.
pub trait Fetch: Send + Sync {
    /// Runs the fetch handler with the guest-provided request.
    fn fetch(&self, request: FetchRequest) -> FetchFuture;
}

impl<F, Fut> Fetch for F
where
    F: Fn(FetchRequest) -> Fut + Send + Sync,
    Fut: Future<Output = Result<FetchResponse, SyscallError>> + Send + 'static,
{
    fn fetch(&self, request: FetchRequest) -> FetchFuture {
        Box::pin(self(request))
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
