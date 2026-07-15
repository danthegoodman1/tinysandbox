//! Local-directory VFS that persists sandbox files under a host directory.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::path::normalize_path;
use super::{
    DirEntry, Errno, FileHandle, FileType, Metadata, OpenMode, Vfs, VfsError, VfsQuota, VfsResult,
    VfsStats,
};

/// Quota-enforced [`Vfs`] backed by a directory on the host filesystem.
///
/// Every sandbox path resolves strictly beneath the root directory given at
/// construction: normalization clamps `..` at the sandbox root, and symbolic
/// links are never followed. Symlinks, hard links to them, and special files
/// (FIFOs, sockets, devices) are invisible — path lookups treat them as
/// absent, `readdir` skips them, and opening one fails with `EACCES`.
///
/// Unlike [`InMemoryVfs`](crate::vfs::InMemoryVfs), contents persist on disk
/// across instances: a new `LocalVfs` over the same root seeds its quota
/// usage by scanning the existing tree. Existing content larger than the
/// quota is tolerated; the quota only rejects further growth.
///
/// The root directory must be dedicated to the sandbox. Quota accounting and
/// handle semantics assume no other process mutates the tree while the VFS is
/// live; external writers can skew usage numbers but cannot break path
/// containment.
#[derive(Debug)]
pub struct LocalVfs {
    root: PathBuf,
    quota: VfsQuota,
    state: Mutex<State>,
}

impl LocalVfs {
    /// Opens a local VFS rooted at an existing directory with no quota limits.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        Self::with_quota(root, VfsQuota::unlimited())
    }

    /// Opens a local VFS rooted at an existing directory, enforcing `quota`.
    pub fn with_quota(root: impl AsRef<Path>, quota: VfsQuota) -> io::Result<Self> {
        let root = fs::canonicalize(root)?;
        if !fs::symlink_metadata(&root)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "LocalVfs root must be a directory",
            ));
        }

        let mut used_bytes = 0;
        let mut file_count = 0;
        scan_tree(&root, &mut used_bytes, &mut file_count)?;

        Ok(Self {
            root,
            quota,
            state: Mutex::new(State {
                handles: BTreeMap::new(),
                open_files: BTreeMap::new(),
                next_handle: 1,
                used_bytes,
                file_count,
            }),
        })
    }

    /// Returns the canonicalized host directory backing this VFS.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns current quota usage.
    pub fn stats(&self) -> VfsResult<VfsStats> {
        let state = self.state();
        Ok(VfsStats {
            used_bytes: state.used_bytes,
            file_count: state.file_count,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Maps a VFS path onto the host directory, verifying that every
    /// intermediate component is a real directory (never a symlink).
    fn resolve(&self, path: &str) -> VfsResult<PathBuf> {
        let components = normalize_path(path)?;
        let mut resolved = self.root.clone();

        for (index, component) in components.iter().enumerate() {
            resolved.push(component);
            if index + 1 == components.len() {
                break;
            }
            match lookup(&resolved)?.as_ref().map(entry_kind) {
                Some(EntryKind::Directory) => {}
                Some(EntryKind::File) => return Err(VfsError::new(Errno::ENOTDIR)),
                Some(EntryKind::Other) | None => return Err(VfsError::new(Errno::ENOENT)),
            }
        }

        Ok(resolved)
    }

    fn ensure_entry_slot(&self, state: &State) -> VfsResult<()> {
        if state.file_count >= self.quota.max_files {
            return Err(VfsError::new(Errno::ENOSPC));
        }

        Ok(())
    }

    /// Checks that resizing a file from `old_len` to `new_len` fits the quota
    /// and returns the resulting total usage.
    fn resized_usage(&self, used_bytes: u64, old_len: u64, new_len: u64) -> VfsResult<u64> {
        if new_len > self.quota.max_file_size {
            return Err(VfsError::new(Errno::ENOSPC));
        }

        let used_bytes = if new_len >= old_len {
            used_bytes
                .checked_add(new_len - old_len)
                .ok_or(VfsError::new(Errno::ENOSPC))?
        } else {
            used_bytes.saturating_sub(old_len - new_len)
        };
        if used_bytes > self.quota.max_bytes {
            return Err(VfsError::new(Errno::ENOSPC));
        }

        Ok(used_bytes)
    }
}

impl Vfs for LocalVfs {
    fn stats(&self) -> Option<VfsResult<VfsStats>> {
        Some(LocalVfs::stats(self))
    }

    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        let resolved = self.resolve(path)?;
        let _guard = self.state();
        let meta = lookup(&resolved)?.ok_or(VfsError::new(Errno::ENOENT))?;
        metadata_from(&meta).ok_or(VfsError::new(Errno::ENOENT))
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let resolved = self.resolve(path)?;
        let _guard = self.state();
        match lookup(&resolved)?.as_ref().map(entry_kind) {
            Some(EntryKind::Directory) => {}
            Some(EntryKind::File) => return Err(VfsError::new(Errno::ENOTDIR)),
            Some(EntryKind::Other) | None => return Err(VfsError::new(Errno::ENOENT)),
        }

        let mut entries = Vec::new();
        for entry in fs::read_dir(&resolved).map_err(|err| io_error(&err))? {
            let entry = entry.map_err(|err| io_error(&err))?;
            // Names the String-based VFS API cannot express are invisible.
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(io_error(&err)),
            };
            if let Some(metadata) = metadata_from(&meta) {
                entries.push(DirEntry { name, metadata });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn mkdir(&self, path: &str) -> VfsResult<()> {
        let resolved = self.resolve(path)?;
        if resolved == self.root {
            return Err(VfsError::new(Errno::EEXIST));
        }

        let mut state = self.state();
        if lookup(&resolved)?.is_some() {
            return Err(VfsError::new(Errno::EEXIST));
        }
        self.ensure_entry_slot(&state)?;

        fs::create_dir(&resolved).map_err(|err| io_error(&err))?;
        state.file_count += 1;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let from_resolved = self.resolve(from)?;
        let to_resolved = self.resolve(to)?;
        if from_resolved == self.root || to_resolved == self.root {
            return Err(VfsError::new(Errno::EINVAL));
        }

        let mut guard = self.state();
        let state = &mut *guard;
        let source_kind = match lookup(&from_resolved)?.as_ref().map(entry_kind) {
            Some(EntryKind::Other) | None => return Err(VfsError::new(Errno::ENOENT)),
            Some(kind) => kind,
        };
        if from_resolved == to_resolved {
            return Ok(());
        }

        if source_kind == EntryKind::Directory && to_resolved.starts_with(&from_resolved) {
            return Err(VfsError::new(Errno::EINVAL));
        }

        let target = lookup(&to_resolved)?;
        fs::rename(&from_resolved, &to_resolved).map_err(|err| {
            // POSIX allows either code when the target directory is non-empty.
            let err = io_error(&err);
            if err.errno() == Errno::EEXIST {
                VfsError::new(Errno::ENOTEMPTY)
            } else {
                err
            }
        })?;

        if let Some(meta) = target
            && entry_kind(&meta) == EntryKind::File
        {
            release_entry(state, file_key(&meta), meta.len());
        }
        Ok(())
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        let resolved = self.resolve(path)?;
        if resolved == self.root {
            return Err(VfsError::new(Errno::EISDIR));
        }

        let mut state = self.state();
        let meta = lookup(&resolved)?.ok_or(VfsError::new(Errno::ENOENT))?;
        match entry_kind(&meta) {
            EntryKind::File => {}
            EntryKind::Directory => return Err(VfsError::new(Errno::EISDIR)),
            EntryKind::Other => return Err(VfsError::new(Errno::ENOENT)),
        }

        fs::remove_file(&resolved).map_err(|err| io_error(&err))?;
        release_entry(&mut state, file_key(&meta), meta.len());
        Ok(())
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        let resolved = self.resolve(path)?;
        if resolved == self.root {
            return Err(VfsError::new(Errno::EBUSY));
        }

        let mut state = self.state();
        match lookup(&resolved)?.as_ref().map(entry_kind) {
            Some(EntryKind::Directory) => {}
            Some(EntryKind::File) => return Err(VfsError::new(Errno::ENOTDIR)),
            Some(EntryKind::Other) | None => return Err(VfsError::new(Errno::ENOENT)),
        }

        fs::remove_dir(&resolved).map_err(|err| {
            // POSIX allows either code when the directory is non-empty.
            let err = io_error(&err);
            if err.errno() == Errno::EEXIST {
                VfsError::new(Errno::ENOTEMPTY)
            } else {
                err
            }
        })?;
        state.file_count = state.file_count.saturating_sub(1);
        Ok(())
    }

    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        mode.validate()?;
        let resolved = self.resolve(path)?;

        let mut state = self.state();
        let existing_len = match lookup(&resolved)? {
            Some(meta) => match entry_kind(&meta) {
                EntryKind::Directory => return Err(VfsError::new(Errno::EISDIR)),
                EntryKind::Other => return Err(VfsError::new(Errno::EACCES)),
                EntryKind::File if mode.create_new => {
                    return Err(VfsError::new(Errno::EEXIST));
                }
                EntryKind::File => Some(meta.len()),
            },
            None if mode.create || mode.create_new => {
                self.ensure_entry_slot(&state)?;
                None
            }
            None => return Err(VfsError::new(Errno::ENOENT)),
        };
        let creating = existing_len.is_none();

        // O_NOFOLLOW keeps a symlink swapped in after the lookup from being
        // followed; O_NONBLOCK keeps a swapped-in FIFO from blocking the open.
        // The fstat below then rejects anything that is not a regular file.
        let file = OpenOptions::new()
            .read(mode.read)
            .write(mode.write || creating)
            .create(creating)
            .create_new(mode.create_new)
            .truncate(mode.truncate)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&resolved)
            .map_err(|err| io_error(&err))?;

        let meta = file.metadata().map_err(|err| io_error(&err))?;
        if !meta.is_file() {
            return Err(VfsError::new(Errno::EACCES));
        }

        if mode.truncate
            && let Some(old_len) = existing_len
        {
            // The open already truncated; usage drops by the previous length.
            state.used_bytes = state.used_bytes.saturating_sub(old_len);
        }
        if creating {
            state.file_count += 1;
        }

        let handle = FileHandle::new(state.next_handle);
        state.next_handle += 1;
        state
            .open_files
            .entry(file_key(&meta))
            .or_default()
            .open_handles += 1;
        state.handles.insert(
            handle,
            HandleState {
                file,
                key: file_key(&meta),
                readable: mode.read,
                writable: mode.write,
                append: mode.append,
            },
        );
        Ok(handle)
    }

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let state = self.state();
        let handle = state
            .handles
            .get(&handle)
            .ok_or(VfsError::new(Errno::EBADF))?;
        if !handle.readable {
            return Err(VfsError::new(Errno::EBADF));
        }

        let mut total = 0;
        while total < buf.len() {
            let position = offset
                .checked_add(total as u64)
                .ok_or(VfsError::new(Errno::EINVAL))?;
            match handle.file.read_at(&mut buf[total..], position) {
                Ok(0) => break,
                Ok(read) => total += read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(io_error(&err)),
            }
        }
        Ok(total)
    }

    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        let mut guard = self.state();
        let state = &mut *guard;
        let handle = state
            .handles
            .get(&handle)
            .ok_or(VfsError::new(Errno::EBADF))?;
        if !handle.writable {
            return Err(VfsError::new(Errno::EBADF));
        }

        if data.is_empty() {
            return Ok(0);
        }

        let old_len = file_len(&handle.file)?;
        let write_offset = if handle.append { old_len } else { offset };
        let write_end = write_offset
            .checked_add(data.len() as u64)
            .ok_or(VfsError::new(Errno::EINVAL))?;
        let new_len = old_len.max(write_end);
        let used_bytes = self.resized_usage(state.used_bytes, old_len, new_len)?;

        match handle.file.write_all_at(data, write_offset) {
            Ok(()) => {
                state.used_bytes = used_bytes;
                Ok(data.len())
            }
            Err(err) => {
                // Resync usage with whatever the partial write left on disk.
                let actual_len = file_len(&handle.file).unwrap_or(old_len);
                state.used_bytes = state
                    .used_bytes
                    .saturating_sub(old_len)
                    .saturating_add(actual_len);
                Err(io_error(&err))
            }
        }
    }

    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        let mut guard = self.state();
        let state = &mut *guard;
        let handle = state
            .handles
            .get(&handle)
            .ok_or(VfsError::new(Errno::EBADF))?;
        if !handle.writable {
            return Err(VfsError::new(Errno::EINVAL));
        }

        let old_len = file_len(&handle.file)?;
        let used_bytes = self.resized_usage(state.used_bytes, old_len, len)?;
        handle.file.set_len(len).map_err(|err| io_error(&err))?;
        state.used_bytes = used_bytes;
        Ok(())
    }

    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        let mut guard = self.state();
        let state = &mut *guard;
        let handle = state
            .handles
            .remove(&handle)
            .ok_or(VfsError::new(Errno::EBADF))?;

        let Some(open_file) = state.open_files.get_mut(&handle.key) else {
            return Ok(());
        };
        open_file.open_handles -= 1;
        if open_file.open_handles > 0 {
            return Ok(());
        }

        let unlinked = open_file.unlinked;
        state.open_files.remove(&handle.key);
        if unlinked {
            // The last handle to an unlinked file releases its storage.
            let len = file_len(&handle.file).unwrap_or(0);
            state.used_bytes = state.used_bytes.saturating_sub(len);
            state.file_count = state.file_count.saturating_sub(1);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct State {
    handles: BTreeMap<FileHandle, HandleState>,
    open_files: BTreeMap<FileKey, OpenFileState>,
    next_handle: u64,
    used_bytes: u64,
    file_count: u64,
}

#[derive(Debug)]
struct HandleState {
    file: File,
    key: FileKey,
    readable: bool,
    writable: bool,
    append: bool,
}

/// Device and inode pair identifying a file across renames and unlinks.
type FileKey = (u64, u64);

#[derive(Debug, Default)]
struct OpenFileState {
    open_handles: u64,
    unlinked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Other,
}

fn entry_kind(meta: &fs::Metadata) -> EntryKind {
    if meta.is_dir() {
        EntryKind::Directory
    } else if meta.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn metadata_from(meta: &fs::Metadata) -> Option<Metadata> {
    match entry_kind(meta) {
        EntryKind::File => Some(Metadata {
            file_type: FileType::File,
            len: meta.len(),
        }),
        EntryKind::Directory => Some(Metadata {
            file_type: FileType::Directory,
            len: 0,
        }),
        EntryKind::Other => None,
    }
}

/// Stats a path without following symlinks; `None` when it does not exist.
fn lookup(path: &Path) -> VfsResult<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_error(&err)),
    }
}

fn file_key(meta: &fs::Metadata) -> FileKey {
    (meta.dev(), meta.ino())
}

fn file_len(file: &File) -> VfsResult<u64> {
    file.metadata()
        .map(|meta| meta.len())
        .map_err(|err| io_error(&err))
}

/// Releases a removed directory entry, deferring to the last close while any
/// handle keeps the file open.
fn release_entry(state: &mut State, key: FileKey, len: u64) {
    if let Some(open_file) = state.open_files.get_mut(&key) {
        open_file.unlinked = true;
    } else {
        state.used_bytes = state.used_bytes.saturating_sub(len);
        state.file_count = state.file_count.saturating_sub(1);
    }
}

fn scan_tree(dir: &Path, used_bytes: &mut u64, file_count: &mut u64) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        // DirEntry::metadata does not follow symlinks, so links and special
        // files fall through unaccounted, matching their invisibility.
        let meta = entry.metadata()?;
        if meta.is_dir() {
            *file_count += 1;
            scan_tree(&entry.path(), used_bytes, file_count)?;
        } else if meta.is_file() {
            *file_count += 1;
            *used_bytes = used_bytes.saturating_add(meta.len());
        }
    }
    Ok(())
}

fn io_error(err: &io::Error) -> VfsError {
    let errno = match err.raw_os_error() {
        Some(libc::ENOENT) => Errno::ENOENT,
        Some(libc::ENOTDIR) => Errno::ENOTDIR,
        Some(libc::EISDIR) => Errno::EISDIR,
        Some(libc::EEXIST) => Errno::EEXIST,
        Some(libc::ENOTEMPTY) => Errno::ENOTEMPTY,
        Some(libc::EACCES | libc::EPERM | libc::ELOOP | libc::EROFS) => Errno::EACCES,
        Some(libc::ENOSPC | libc::EDQUOT | libc::EFBIG) => Errno::ENOSPC,
        Some(libc::EBUSY) => Errno::EBUSY,
        Some(libc::EBADF) => Errno::EBADF,
        _ => Errno::EINVAL,
    };
    VfsError::new(errno)
}
