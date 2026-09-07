//! Async filesystem facade exposed to sandbox commands.

use super::{HostContext, Limits, control::ExecutionControl};
use std::collections::BTreeSet;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, ReadBuf};
use tokio::task;

use super::command::BoxAsyncRead;
use crate::vfs::{
    DirEntry, Errno, FileHandle, FileType, Metadata, OpenMode, Vfs, VfsError, VfsResult,
};

pub(crate) const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// Filesystem handle scoped to a sandbox command's current directory.
#[derive(Clone)]
pub struct Fs {
    vfs: Arc<dyn Vfs>,
    bin_commands: Arc<BTreeSet<String>>,
    cwd: String,
    handles: Arc<HandleRegistry>,
}

impl Fs {
    pub(crate) fn new(vfs: Arc<dyn Vfs>, bin_commands: Arc<BTreeSet<String>>, cwd: String) -> Self {
        Self::scoped(vfs, bin_commands, cwd, None)
    }

    pub(crate) fn scoped(
        vfs: Arc<dyn Vfs>,
        bin_commands: Arc<BTreeSet<String>>,
        cwd: String,
        control: Option<Arc<ExecutionControl>>,
    ) -> Self {
        let handles = Arc::new(HandleRegistry {
            vfs: Arc::clone(&vfs),
            files: Mutex::new(BTreeSet::new()),
            control,
        });
        if let Some(control) = &handles.control {
            control.register(&handles);
        }
        Self {
            vfs,
            bin_commands,
            cwd,
            handles,
        }
    }

    pub(crate) fn with_cwd(&self, cwd: String) -> Self {
        Self {
            cwd,
            ..self.clone()
        }
    }

    /// Maximum buffer accepted by one raw read/write operation. Stream chunks
    /// retain a 64 KiB allowance even when the whole-file input limit is lower.
    pub fn max_io_bytes(&self) -> usize {
        self.limits().host_input_bytes.max(STREAM_CHUNK_BYTES)
    }

    /// Cooperative cancellation and deadline for trusted work in this execution.
    /// Host filesystem calls outside an exec return a context with no deadline.
    pub fn host_context(&self) -> HostContext {
        self.handles
            .control
            .as_ref()
            .map_or_else(HostContext::unscoped, |control| control.host_context())
    }

    /// Whether this execution has ended or exhausted its wall-clock budget.
    pub fn is_cancelled(&self) -> bool {
        self.handles
            .control
            .as_ref()
            .is_some_and(|c| c.is_cancelled())
    }

    /// Remaining execution budget, or none for the sandbox's host filesystem facade.
    pub fn remaining_wall_time(&self) -> Option<Duration> {
        self.handles.control.as_ref().map(|c| c.remaining())
    }

    /// Yields cooperatively and checks execution cancellation.
    pub async fn checkpoint(&self) -> VfsResult<()> {
        tokio::task::yield_now().await;
        self.check()
    }

    fn limits(&self) -> Limits {
        self.handles
            .control
            .as_ref()
            .map_or_else(Limits::default, |c| c.limits)
    }
    fn check(&self) -> VfsResult<()> {
        self.handles.control.as_ref().map_or(Ok(()), |c| c.check())
    }
    fn check_path(&self, path: &str) -> VfsResult<()> {
        self.check()?;
        if path.contains('\0')
            || path.split('/').filter(|p| !p.is_empty()).count()
                > self.limits().max_path_depth.min(256)
        {
            return Err(VfsError::new(Errno::EINVAL));
        }
        Ok(())
    }

    /// Returns metadata for a path resolved relative to the current directory.
    pub async fn stat(&self, path: &str) -> VfsResult<Metadata> {
        let path = self.resolve(path);
        self.check_path(&path)?;
        if let Some(metadata) = self.bin_stat(&path) {
            return metadata;
        }
        self.dispatch_path(&path.clone(), move |vfs| vfs.stat(&path))
            .await
    }

    /// Reads directory entries for a path resolved relative to the current directory.
    pub async fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let path = self.resolve(path);
        self.check_path(&path)?;
        if path == "/" {
            let mut entries = self.dispatch_path("/", move |vfs| vfs.readdir("/")).await?;
            entries.push(DirEntry {
                name: "bin".to_owned(),
                metadata: Metadata {
                    file_type: FileType::Directory,
                    len: 0,
                },
            });
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            return Ok(entries);
        }
        if path == "/bin" {
            return Ok(self
                .bin_commands
                .iter()
                .map(|name| DirEntry {
                    name: name.clone(),
                    metadata: Metadata {
                        file_type: FileType::File,
                        len: 0,
                    },
                })
                .collect());
        }
        if path.starts_with("/bin/") {
            return Err(VfsError::new(Errno::ENOTDIR));
        }
        self.dispatch_path(&path.clone(), move |vfs| vfs.readdir(&path))
            .await
    }

    /// Creates a directory.
    pub async fn mkdir(&self, path: &str) -> VfsResult<()> {
        let path = self.resolve(path);
        self.check_path(&path)?;
        if is_bin_path(&path) {
            return Err(VfsError::new(Errno::EACCES));
        }
        self.dispatch_path(&path.clone(), move |vfs| vfs.mkdir(&path))
            .await
    }

    /// Renames a file or directory.
    pub async fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let from = self.resolve(from);
        let to = self.resolve(to);
        if is_bin_path(&from) || is_bin_path(&to) {
            return Err(VfsError::new(Errno::EACCES));
        }
        self.check_path(&from)?;
        self.check_path(&to)?;
        self.dispatch(
            self.vfs.is_fast_path(&from) && self.vfs.is_fast_path(&to),
            false,
            move |vfs| vfs.rename(&from, &to),
        )
        .await
    }

    /// Removes a file.
    pub async fn unlink(&self, path: &str) -> VfsResult<()> {
        let path = self.resolve(path);
        self.check_path(&path)?;
        if is_bin_path(&path) {
            return Err(VfsError::new(Errno::EACCES));
        }
        self.dispatch_path(&path.clone(), move |vfs| vfs.unlink(&path))
            .await
    }

    /// Removes an empty directory.
    pub async fn rmdir(&self, path: &str) -> VfsResult<()> {
        let path = self.resolve(path);
        self.check_path(&path)?;
        if is_bin_path(&path) {
            return Err(VfsError::new(Errno::EACCES));
        }
        self.dispatch_path(&path.clone(), move |vfs| vfs.rmdir(&path))
            .await
    }

    /// Reads a whole file, bounded by `Limits::host_input_bytes`.
    pub async fn read_file(&self, path: &str) -> VfsResult<Vec<u8>> {
        self.read_file_bounded(path, self.limits().host_input_bytes)
            .await
    }

    /// Reads at most `limit` bytes, rejecting larger files with `EFBIG`.
    pub async fn read_file_bounded(&self, path: &str, limit: usize) -> VfsResult<Vec<u8>> {
        let path = self.resolve(path);
        self.check_path(&path)?;
        if path == "/bin" {
            return Err(VfsError::new(Errno::EISDIR));
        }
        if let Some(name) = path.strip_prefix("/bin/") {
            return if self.bin_commands.contains(name) {
                Ok(Vec::new())
            } else {
                Err(VfsError::new(Errno::ENOENT))
            };
        }
        let limit = limit.min(self.limits().host_input_bytes);
        let handle = self.open(&path, OpenMode::read_only()).await?;
        let _owned = OpenHandoff {
            registry: Arc::clone(&self.handles),
            handle: Some(handle),
        };
        let result = async {
            let mut out = Vec::new();
            loop {
                self.checkpoint().await?;
                let want =
                    STREAM_CHUNK_BYTES.min(limit.saturating_sub(out.len()).saturating_add(1));
                let (buf, n) = self
                    .read_at(handle, out.len() as u64, vec![0; want])
                    .await?;
                if n == 0 {
                    return Ok(out);
                }
                if n > limit.saturating_sub(out.len()) {
                    return Err(VfsError::new(Errno::EFBIG));
                }
                out.extend_from_slice(&buf[..n]);
            }
        }
        .await;
        let close = self.close(handle).await;
        match (result, close) {
            (Ok(data), Ok(())) => Ok(data),
            (Err(err), _) | (_, Err(err)) => Err(err),
        }
    }

    pub(crate) async fn stream_reader(&self, path: &str) -> VfsResult<BoxAsyncRead> {
        let handle = self.open(path, OpenMode::read_only()).await?;
        Ok(self.stream_reader_from_handle(handle))
    }

    /// Writes a whole file, appending when `append` is true.
    /// Inputs above `Limits::host_input_bytes` fail before opening the file.
    pub async fn write_file(&self, path: &str, data: &[u8], append: bool) -> VfsResult<()> {
        if data.len() > self.limits().host_input_bytes {
            return Err(VfsError::new(Errno::EFBIG));
        }
        let mode = if append {
            OpenMode::write_only().create().append()
        } else {
            OpenMode::write_only().create().truncate()
        };
        let handle = self.open(path, mode).await?;
        let _owned = OpenHandoff {
            registry: Arc::clone(&self.handles),
            handle: Some(handle),
        };
        let result = async {
            let mut written = 0;
            while written < data.len() {
                self.checkpoint().await?;
                let end = data.len().min(written.saturating_add(STREAM_CHUNK_BYTES));
                let n = self
                    .write_at(handle, written as u64, data[written..end].to_vec())
                    .await?;
                if n == 0 {
                    return Err(VfsError::new(Errno::ENOSPC));
                }
                written += n;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = self.abort(handle).await;
            return result;
        }
        self.close(handle).await
    }

    /// Creates a file if needed or updates its metadata when supported.
    pub async fn touch(&self, path: &str) -> VfsResult<()> {
        let handle = self.open(path, OpenMode::write_only().create()).await?;
        let _owned = OpenHandoff {
            registry: Arc::clone(&self.handles),
            handle: Some(handle),
        };
        self.close(handle).await
    }

    /// Opens a VFS file handle for a path.
    pub async fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        let path = self.resolve(path);
        self.check_path(&path)?;
        if is_bin_path(&path) {
            return Err(if path == "/bin" {
                VfsError::new(Errno::EISDIR)
            } else if !mode.write && !self.bin_commands.contains(path.trim_start_matches("/bin/")) {
                VfsError::new(Errno::ENOENT)
            } else {
                VfsError::new(Errno::EACCES)
            });
        }
        let registry = Arc::clone(&self.handles);
        self.dispatch_path(&path.clone(), move |vfs| {
            registry.reserve()?;
            let handle = match vfs.open(&path, mode) {
                Ok(handle) => handle,
                Err(err) => {
                    registry.release();
                    return Err(err);
                }
            };
            registry
                .files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(handle);
            if registry.control.as_ref().is_some_and(|c| c.is_cancelled()) {
                registry.remove(handle)?;
                let _release = FileAdmission(&registry);
                let _ = vfs.abort(handle);
                return Err(VfsError::new(Errno::EIO));
            }
            Ok(OpenHandoff {
                registry,
                handle: Some(handle),
            })
        })
        .await
        .map(|mut handoff| handoff.handle.take().expect("open handle delivered once"))
    }

    /// Reads from a file handle at `offset`.
    pub async fn read_at(
        &self,
        handle: FileHandle,
        offset: u64,
        mut buf: Vec<u8>,
    ) -> VfsResult<(Vec<u8>, usize)> {
        if buf.len() > self.max_io_bytes() {
            return Err(VfsError::new(Errno::EFBIG));
        }
        self.dispatch(self.vfs.is_fast_handle(handle), false, move |vfs| {
            let n = vfs.read_at(handle, offset, &mut buf)?;
            if n > buf.len() {
                return Err(VfsError::new(Errno::EIO));
            }
            Ok((buf, n))
        })
        .await
    }

    /// Writes to a file handle at `offset`.
    pub async fn write_at(
        &self,
        handle: FileHandle,
        offset: u64,
        data: Vec<u8>,
    ) -> VfsResult<usize> {
        if data.len() > self.max_io_bytes() {
            return Err(VfsError::new(Errno::EFBIG));
        }
        self.dispatch(self.vfs.is_fast_handle(handle), false, move |vfs| {
            let n = vfs.write_at(handle, offset, &data)?;
            if n > data.len() {
                return Err(VfsError::new(Errno::EIO));
            }
            Ok(n)
        })
        .await
    }

    /// Changes a file handle's length.
    pub async fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        self.dispatch(self.vfs.is_fast_handle(handle), false, move |vfs| {
            vfs.truncate(handle, len)
        })
        .await
    }

    /// Closes a file handle.
    pub async fn close(&self, handle: FileHandle) -> VfsResult<()> {
        self.finish(handle, false).await
    }

    /// Releases a handle, discarding staged writes when the backend supports it.
    pub async fn abort(&self, handle: FileHandle) -> VfsResult<()> {
        self.finish(handle, true).await
    }

    async fn finish(&self, handle: FileHandle, abort: bool) -> VfsResult<()> {
        let registry = Arc::clone(&self.handles);
        self.dispatch(self.vfs.is_fast_handle(handle), true, move |vfs| {
            registry.remove(handle)?;
            let _release = FileAdmission(&registry);
            if abort || registry.control.as_ref().is_some_and(|c| c.is_cancelled()) {
                vfs.abort(handle)
            } else {
                vfs.close(handle)
            }
        })
        .await
    }

    pub(crate) fn stream_reader_from_handle(&self, handle: FileHandle) -> BoxAsyncRead {
        Box::pin(FsStreamReader::new(self.clone(), handle))
    }

    async fn dispatch_path<R, F>(&self, path: &str, op: F) -> VfsResult<R>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Vfs) -> VfsResult<R> + Send + 'static,
    {
        self.check_path(path)?;
        self.dispatch(self.vfs.is_fast_path(path), false, op).await
    }

    async fn dispatch<R, F>(&self, fast: bool, cleanup: bool, op: F) -> VfsResult<R>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Vfs) -> VfsResult<R> + Send + 'static,
    {
        if !cleanup {
            self.check()?;
        }
        if fast {
            return op(self.vfs.as_ref());
        }
        static WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(128);
        let permit = WORKERS
            .acquire()
            .await
            .map_err(|_| VfsError::new(Errno::EIO))?;
        let registry = Arc::clone(&self.handles);
        task::spawn_blocking(move || {
            let _permit = permit;
            if !cleanup && let Some(control) = &registry.control {
                control.check()?;
            }
            op(registry.vfs.as_ref())
        })
        .await
        .unwrap_or_else(|_| Err(VfsError::new(Errno::EIO)))
    }

    fn bin_stat(&self, path: &str) -> Option<VfsResult<Metadata>> {
        if path == "/bin" {
            return Some(Ok(Metadata {
                file_type: FileType::Directory,
                len: 0,
            }));
        }

        let name = path.strip_prefix("/bin/")?;
        Some(if !name.contains('/') && self.bin_commands.contains(name) {
            Ok(Metadata {
                file_type: FileType::File,
                len: 0,
            })
        } else {
            Err(VfsError::new(Errno::ENOENT))
        })
    }

    pub(crate) fn resolve(&self, path: &str) -> String {
        normalize_absolute(if path.starts_with('/') {
            path.to_owned()
        } else if self.cwd == "/" {
            format!("/{path}")
        } else {
            format!("{}/{path}", self.cwd)
        })
    }
}

// A global handle ceiling also bounds pending fallback cleanup. Cleanup has a
// fixed worker count, works without a Tokio runtime, and retains admission until
// the backend actually releases the handle.
static OPEN_FILES: AtomicUsize = AtomicUsize::new(0);
pub(crate) struct HandleRegistry {
    vfs: Arc<dyn Vfs>,
    files: Mutex<BTreeSet<FileHandle>>,
    control: Option<Arc<ExecutionControl>>,
}
impl HandleRegistry {
    pub(crate) fn abandon_all(&self) {
        let files = std::mem::take(&mut *self.files.lock().unwrap_or_else(|e| e.into_inner()));
        for handle in files {
            cleanup_handle(Arc::clone(&self.vfs), handle, self.control.clone());
        }
    }
    fn reserve(&self) -> VfsResult<()> {
        OPEN_FILES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 16384).then_some(n + 1)
            })
            .map_err(|_| VfsError::new(Errno::ENOSPC))?;
        if let Some(control) = &self.control
            && let Err(err) = control.acquire_file()
        {
            OPEN_FILES.fetch_sub(1, Ordering::AcqRel);
            return Err(err);
        }
        Ok(())
    }
    fn release(&self) {
        OPEN_FILES.fetch_sub(1, Ordering::AcqRel);
        if let Some(control) = &self.control {
            control.release_file();
        }
    }
    fn remove(&self, handle: FileHandle) -> VfsResult<()> {
        if !self
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&handle)
        {
            return Err(VfsError::new(Errno::EBADF));
        }
        Ok(())
    }
    fn abandon(&self, handle: FileHandle) {
        if self
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&handle)
        {
            cleanup_handle(Arc::clone(&self.vfs), handle, self.control.clone());
        }
    }
}
impl Drop for HandleRegistry {
    fn drop(&mut self) {
        for handle in std::mem::take(self.files.get_mut().unwrap_or_else(|e| e.into_inner())) {
            cleanup_handle(Arc::clone(&self.vfs), handle, self.control.clone());
        }
    }
}
// Owns a newly opened result across the blocking-worker/async handoff. If the
// awaiting future disappears, dropping its unobserved result closes the file.
struct OpenHandoff {
    registry: Arc<HandleRegistry>,
    handle: Option<FileHandle>,
}
impl Drop for OpenHandoff {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.registry.abandon(handle);
        }
    }
}
struct FileAdmission<'a>(&'a HandleRegistry);
impl Drop for FileAdmission<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}
type Cleanup = (Arc<dyn Vfs>, FileHandle, Option<Arc<ExecutionControl>>);
fn cleanup_handle(vfs: Arc<dyn Vfs>, handle: FileHandle, control: Option<Arc<ExecutionControl>>) {
    fn run((vfs, handle, control): Cleanup) {
        let _ = vfs.abort(handle);
        OPEN_FILES.fetch_sub(1, Ordering::AcqRel);
        if let Some(control) = control {
            control.release_file();
        }
    }
    if vfs.is_fast_handle(handle) {
        run((vfs, handle, control));
        return;
    }
    static CLEANUP: OnceLock<std::sync::mpsc::Sender<Cleanup>> = OnceLock::new();
    let sender = CLEANUP.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<Cleanup>();
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 0..4 {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name("tinysandbox-fs-cleanup".into())
                .spawn(move || {
                    loop {
                        let next = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        match next {
                            Ok(item) => run(item),
                            Err(_) => break,
                        }
                    }
                })
                .expect("start filesystem cleanup worker");
        }
        sender
    });
    if let Err(err) = sender.send((vfs, handle, control)) {
        run(err.0);
    }
}

type ReadAtFuture = Pin<Box<dyn Future<Output = VfsResult<(Vec<u8>, usize)>> + Send>>;

struct FsStreamReader {
    fs: Fs,
    handle: Option<FileHandle>,
    offset: u64,
    pending: Vec<u8>,
    pending_start: usize,
    in_flight: Option<ReadAtFuture>,
}

impl FsStreamReader {
    fn new(fs: Fs, handle: FileHandle) -> Self {
        Self {
            fs,
            handle: Some(handle),
            offset: 0,
            pending: Vec::new(),
            pending_start: 0,
            in_flight: None,
        }
    }

    fn close_handle(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        self.fs.handles.abandon(handle);
    }

    fn copy_pending(&mut self, buf: &mut ReadBuf<'_>) -> bool {
        if self.pending_start == self.pending.len() || buf.remaining() == 0 {
            return false;
        }
        let n = (self.pending.len() - self.pending_start).min(buf.remaining());
        buf.put_slice(&self.pending[self.pending_start..self.pending_start + n]);
        self.pending_start += n;
        true
    }
}

impl Drop for FsStreamReader {
    fn drop(&mut self) {
        self.close_handle();
    }
}

impl AsyncRead for FsStreamReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.copy_pending(buf) {
            return Poll::Ready(Ok(()));
        }
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        loop {
            if this.in_flight.is_none() {
                let Some(handle) = this.handle else {
                    return Poll::Ready(Ok(()));
                };
                let fs = this.fs.clone();
                let offset = this.offset;
                this.in_flight = Some(Box::pin(async move {
                    fs.read_at(handle, offset, vec![0; STREAM_CHUNK_BYTES])
                        .await
                }));
            }

            let result = {
                let future = this.in_flight.as_mut().expect("future was just installed");
                match future.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => result,
                }
            };
            this.in_flight = None;

            let (mut bytes, n) = match result {
                Ok(result) => result,
                Err(err) => {
                    this.close_handle();
                    return Poll::Ready(Err(io::Error::other(err)));
                }
            };
            if n == 0 {
                this.close_handle();
                return Poll::Ready(Ok(()));
            }
            bytes.truncate(n);
            this.offset = this.offset.saturating_add(n as u64);
            this.pending = bytes;
            this.pending_start = 0;
            if this.copy_pending(buf) {
                return Poll::Ready(Ok(()));
            }
        }
    }
}

pub(crate) fn errno_message(errno: Errno) -> &'static str {
    match errno {
        Errno::EBADF => "Bad file descriptor",
        Errno::EBUSY => "Device or resource busy",
        Errno::EXDEV => "Invalid cross-device link",
        Errno::EACCES => "Permission denied",
        Errno::EEXIST => "File exists",
        Errno::EFBIG => "File too large",
        Errno::EIO => "Input/output error",
        Errno::EINVAL => "Invalid argument",
        Errno::EISDIR => "Is a directory",
        Errno::ENOENT => "No such file or directory",
        Errno::ENOSPC => "No space left on device",
        Errno::ENOTDIR => "Not a directory",
        Errno::ENOTEMPTY => "Directory not empty",
    }
}

pub(crate) fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn is_bin_path(path: &str) -> bool {
    path == "/bin" || path.starts_with("/bin/")
}

pub(crate) fn normalize_absolute(path: String) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", parts.join("/"))
    }
}
