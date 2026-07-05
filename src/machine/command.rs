use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};

use super::fs::Fs;

pub type CommandFuture = Pin<Box<dyn Future<Output = CommandResult> + Send>>;
pub type BoxAsyncRead = Pin<Box<dyn AsyncRead + Send>>;
pub type BoxAsyncWrite = Pin<Box<dyn AsyncWrite + Send>>;

pub trait Command: Send + Sync {
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

pub struct CommandContext {
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub stdin: BoxAsyncRead,
    pub stdout: BoxAsyncWrite,
    pub stderr: BoxAsyncWrite,
    pub fs: Fs,
    pub limits: Limits,
    pub commands: Arc<BTreeSet<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandResult {
    pub exit_code: i32,
}

impl CommandResult {
    pub const fn new(exit_code: i32) -> Self {
        Self { exit_code }
    }

    pub const fn success() -> Self {
        Self::new(0)
    }

    pub const fn failure() -> Self {
        Self::new(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub wall_time: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub max_commands: usize,
    pub sort_input_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_time: Duration::from_secs(30),
            stdout_bytes: 1024 * 1024,
            stderr_bytes: 1024 * 1024,
            max_commands: 1024,
            sort_input_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SharedWriter {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn boxed(&self) -> BoxAsyncWrite {
        Box::pin(self.clone())
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl AsyncWrite for SharedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
