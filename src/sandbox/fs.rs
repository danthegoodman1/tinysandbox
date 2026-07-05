//! Async filesystem facade exposed to sandbox commands.

use std::collections::BTreeSet;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

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
}

impl Fs {
    pub(crate) fn new(vfs: Arc<dyn Vfs>, bin_commands: Arc<BTreeSet<String>>, cwd: String) -> Self {
        Self {
            vfs,
            bin_commands,
            cwd,
        }
    }

    /// Returns metadata for a path resolved relative to the current directory.
    pub async fn stat(&self, path: &str) -> VfsResult<Metadata> {
        let path = self.resolve(path);
        if let Some(metadata) = self.bin_stat(&path) {
            return metadata;
        }
        self.dispatch(move |vfs| vfs.stat(&path)).await
    }

    /// Reads directory entries for a path resolved relative to the current directory.
    pub async fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let path = self.resolve(path);
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
        self.dispatch(move |vfs| vfs.readdir(&path)).await
    }

    /// Creates a directory.
    pub async fn mkdir(&self, path: &str) -> VfsResult<()> {
        let path = self.resolve(path);
        if is_bin_path(&path) {
            return Err(VfsError::new(Errno::EACCES));
        }
        self.dispatch(move |vfs| vfs.mkdir(&path)).await
    }

    /// Renames a file or directory.
    pub async fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let from = self.resolve(from);
        let to = self.resolve(to);
        if is_bin_path(&from) || is_bin_path(&to) {
            return Err(VfsError::new(Errno::EACCES));
        }
        self.dispatch(move |vfs| vfs.rename(&from, &to)).await
    }

    /// Removes a file.
    pub async fn unlink(&self, path: &str) -> VfsResult<()> {
        let path = self.resolve(path);
        if is_bin_path(&path) {
            return Err(VfsError::new(Errno::EACCES));
        }
        self.dispatch(move |vfs| vfs.unlink(&path)).await
    }

    /// Removes an empty directory.
    pub async fn rmdir(&self, path: &str) -> VfsResult<()> {
        let path = self.resolve(path);
        if is_bin_path(&path) {
            return Err(VfsError::new(Errno::EACCES));
        }
        self.dispatch(move |vfs| vfs.rmdir(&path)).await
    }

    /// Reads an entire file into memory.
    pub async fn read_file(&self, path: &str) -> VfsResult<Vec<u8>> {
        let path = self.resolve(path);
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
        self.dispatch(move |vfs| read_file_sync(vfs, &path)).await
    }

    pub(crate) async fn stream_reader(&self, path: &str) -> VfsResult<BoxAsyncRead> {
        let handle = self.open(path, OpenMode::read_only()).await?;
        Ok(self.stream_reader_from_handle(handle))
    }

    /// Writes a whole file, appending when `append` is true.
    pub async fn write_file(&self, path: &str, data: &[u8], append: bool) -> VfsResult<()> {
        let path = self.resolve(path);
        let data = data.to_vec();
        if is_bin_path(&path) {
            return Err(VfsError::new(Errno::EACCES));
        }
        self.dispatch(move |vfs| write_file_sync(vfs, &path, &data, append))
            .await
    }

    /// Creates a file if needed or updates its metadata when supported.
    pub async fn touch(&self, path: &str) -> VfsResult<()> {
        let path = self.resolve(path);
        if is_bin_path(&path) {
            return Err(VfsError::new(Errno::EACCES));
        }
        self.dispatch(move |vfs| {
            let handle = match vfs.open(&path, OpenMode::write_only().create()) {
                Ok(handle) => handle,
                Err(err) => return Err(err),
            };
            vfs.close(handle)
        })
        .await
    }

    /// Opens a VFS file handle for a path.
    pub async fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        let path = self.resolve(path);
        if is_bin_path(&path) {
            return Err(if path == "/bin" {
                VfsError::new(Errno::EISDIR)
            } else if !mode.write && !self.bin_commands.contains(path.trim_start_matches("/bin/")) {
                VfsError::new(Errno::ENOENT)
            } else {
                VfsError::new(Errno::EACCES)
            });
        }
        self.dispatch(move |vfs| vfs.open(&path, mode)).await
    }

    /// Reads from a file handle at `offset`.
    pub async fn read_at(
        &self,
        handle: FileHandle,
        offset: u64,
        mut buf: Vec<u8>,
    ) -> VfsResult<(Vec<u8>, usize)> {
        self.dispatch(move |vfs| {
            let n = vfs.read_at(handle, offset, &mut buf)?;
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
        self.dispatch(move |vfs| vfs.write_at(handle, offset, &data))
            .await
    }

    /// Changes a file handle's length.
    pub async fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        self.dispatch(move |vfs| vfs.truncate(handle, len)).await
    }

    /// Closes a file handle.
    pub async fn close(&self, handle: FileHandle) -> VfsResult<()> {
        self.dispatch(move |vfs| vfs.close(handle)).await
    }

    pub(crate) fn stream_reader_from_handle(&self, handle: FileHandle) -> BoxAsyncRead {
        Box::pin(FsStreamReader::new(self.clone(), handle))
    }

    async fn dispatch<R, F>(&self, op: F) -> VfsResult<R>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Vfs) -> VfsResult<R> + Send + 'static,
    {
        if self.vfs.is_fast() {
            return op(self.vfs.as_ref());
        }

        let vfs = Arc::clone(&self.vfs);
        task::spawn_blocking(move || op(vfs.as_ref()))
            .await
            .unwrap_or_else(|_| Err(VfsError::new(Errno::EINVAL)))
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

type ReadAtFuture = Pin<Box<dyn Future<Output = VfsResult<(Vec<u8>, usize)>> + Send>>;

struct FsStreamReader {
    fs: Fs,
    handle: Option<FileHandle>,
    offset: u64,
    pending: Vec<u8>,
    in_flight: Option<ReadAtFuture>,
}

impl FsStreamReader {
    fn new(fs: Fs, handle: FileHandle) -> Self {
        Self {
            fs,
            handle: Some(handle),
            offset: 0,
            pending: Vec::new(),
            in_flight: None,
        }
    }

    fn close_handle(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let fs = self.fs.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = fs.close(handle).await;
            });
        }
    }

    fn copy_pending(&mut self, buf: &mut ReadBuf<'_>) -> bool {
        if self.pending.is_empty() || buf.remaining() == 0 {
            return false;
        }
        let n = self.pending.len().min(buf.remaining());
        buf.put_slice(&self.pending[..n]);
        self.pending.drain(..n);
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
        Errno::EACCES => "Permission denied",
        Errno::EEXIST => "File exists",
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

fn read_file_sync(vfs: &dyn Vfs, path: &str) -> VfsResult<Vec<u8>> {
    let handle = vfs.open(path, OpenMode::read_only())?;
    let result = read_all_from_handle(vfs, handle);
    let close = vfs.close(handle);
    match (result, close) {
        (Ok(data), Ok(())) => Ok(data),
        (Err(err), _) | (_, Err(err)) => Err(err),
    }
}

fn read_all_from_handle(vfs: &dyn Vfs, handle: FileHandle) -> VfsResult<Vec<u8>> {
    let mut out = Vec::new();
    let mut offset = 0_u64;
    let mut buf = [0_u8; 8192];
    loop {
        let read = vfs.read_at(handle, offset, &mut buf)?;
        if read == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..read]);
        offset += u64::try_from(read).map_err(|_| VfsError::new(Errno::EINVAL))?;
    }
}

fn write_file_sync(vfs: &dyn Vfs, path: &str, data: &[u8], append: bool) -> VfsResult<()> {
    let mode = if append {
        OpenMode::write_only().create().append()
    } else {
        OpenMode::write_only().create().truncate()
    };
    let handle = vfs.open(path, mode)?;
    let result = write_all_to_handle(vfs, handle, data);
    let close = vfs.close(handle);
    result.and(close)
}

fn write_all_to_handle(vfs: &dyn Vfs, handle: FileHandle, data: &[u8]) -> VfsResult<()> {
    let mut written = 0;
    while written < data.len() {
        let n = vfs.write_at(
            handle,
            u64::try_from(written).map_err(|_| VfsError::new(Errno::EINVAL))?,
            &data[written..],
        )?;
        if n == 0 {
            return Err(VfsError::new(Errno::ENOSPC));
        }
        written += n;
    }
    Ok(())
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
