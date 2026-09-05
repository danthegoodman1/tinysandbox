//! Embedder host interfaces for sandboxed JavaScript.
//!
//! Existing one-argument closures remain supported. Context-aware registrations
//! receive a [`HostContext`] that shares the execution cancellation signal and
//! exposes the individual call's monotonic deadline. The context is cancelled
//! when that callback settles. Propagate it to downstream work; the runtime can
//! drop an unfinished callback future, but it
//! cannot preempt synchronous blocking work or bound the host allocator.

use std::future::Future;
use std::pin::Pin;

use super::HostContext;
use serde_json::Value;

/// Future returned by host globals.
pub type JsGlobalFuture = Pin<Box<dyn Future<Output = Result<Value, HostError>> + Send>>;
/// Future returned by sandbox fetch handlers.
pub type FetchFuture = Pin<Box<dyn Future<Output = Result<FetchResponse, HostError>> + Send>>;

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

/// Host function bound into the sandboxed JavaScript global scope.
pub trait JsGlobal: Send + Sync {
    /// Runs the global with the guest-provided JSON argument.
    fn call(&self, args: Value) -> JsGlobalFuture;

    /// Runs with cooperative cancellation and the remaining host-call deadline.
    /// Existing implementations continue to receive calls through [`Self::call`].
    fn call_with_context(&self, args: Value, _context: HostContext) -> JsGlobalFuture {
        self.call(args)
    }
}

impl<F, Fut> JsGlobal for F
where
    F: Fn(Value) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Value, HostError>> + Send + 'static,
{
    fn call(&self, args: Value) -> JsGlobalFuture {
        Box::pin(self(args))
    }
}

/// Host transport implementation backing sandboxed JavaScript `fetch`.
pub trait Fetch: Send + Sync {
    /// Runs the fetch handler with the guest-provided request.
    fn fetch(&self, request: FetchRequest) -> FetchFuture;

    /// Runs with cooperative cancellation and the remaining host-call deadline.
    /// Existing implementations continue through [`Self::fetch`].
    fn fetch_with_context(&self, request: FetchRequest, _context: HostContext) -> FetchFuture {
        self.fetch(request)
    }
}

impl<F, Fut> Fetch for F
where
    F: Fn(FetchRequest) -> Fut + Send + Sync,
    Fut: Future<Output = Result<FetchResponse, HostError>> + Send + 'static,
{
    fn fetch(&self, request: FetchRequest) -> FetchFuture {
        Box::pin(self(request))
    }
}

pub(crate) struct ContextualGlobal<F>(pub(crate) F);
impl<F, Fut> JsGlobal for ContextualGlobal<F>
where
    F: Fn(Value, HostContext) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Value, HostError>> + Send + 'static,
{
    fn call(&self, args: Value) -> JsGlobalFuture {
        self.call_with_context(args, HostContext::unscoped())
    }
    fn call_with_context(&self, args: Value, context: HostContext) -> JsGlobalFuture {
        Box::pin((self.0)(args, context))
    }
}

pub(crate) struct ContextualFetch<F>(pub(crate) F);
impl<F, Fut> Fetch for ContextualFetch<F>
where
    F: Fn(FetchRequest, HostContext) -> Fut + Send + Sync,
    Fut: Future<Output = Result<FetchResponse, HostError>> + Send + 'static,
{
    fn fetch(&self, request: FetchRequest) -> FetchFuture {
        self.fetch_with_context(request, HostContext::unscoped())
    }
    fn fetch_with_context(&self, request: FetchRequest, context: HostContext) -> FetchFuture {
        Box::pin((self.0)(request, context))
    }
}

/// Error from registering host globals on a live sandbox.
///
/// The builder validates the same rules eagerly and panics instead, because a
/// statically declared global surface should fail at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsGlobalError {
    message: String,
}

impl JsGlobalError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for JsGlobalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JsGlobalError {}

/// A set of host globals to bind in one swap.
///
/// Build the set for a turn, then hand it to
/// [`Sandbox::replace_js_globals`](crate::sandbox::Sandbox::replace_js_globals);
/// the whole set is validated before it replaces the live one, so a rejected
/// set never lands halfway.
#[derive(Default)]
pub struct JsGlobals {
    pub(crate) entries: Vec<(String, std::sync::Arc<dyn JsGlobal>)>,
}

impl JsGlobals {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a host function at a dotted global path.
    #[must_use]
    pub fn with<F, Fut>(mut self, name: impl Into<String>, global: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, HostError>> + Send + 'static,
    {
        self.entries
            .push((name.into(), std::sync::Arc::new(global)));
        self
    }

    /// Adds a host function that receives cooperative cancellation and a deadline.
    #[must_use]
    pub fn with_context<F, Fut>(mut self, name: impl Into<String>, global: F) -> Self
    where
        F: Fn(Value, HostContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, HostError>> + Send + 'static,
    {
        self.entries
            .push((name.into(), std::sync::Arc::new(ContextualGlobal(global))));
        self
    }
}

/// Error returned by an embedder host global or fetch handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    /// Human-readable error message exposed as the JavaScript `Error.message`.
    pub message: String,
    /// Optional machine-readable code exposed as the JavaScript `Error.code`.
    pub code: Option<String>,
}

impl HostError {
    /// Creates a host error with no code.
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

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HostError {}
