//! Local-directory VFS that persists sandbox files under a host directory.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rustix::fs::{AtFlags, Dir, Mode, OFlags, mkdirat, openat, renameat, statat, unlinkat};

use super::path::{MAX_PATH_DEPTH, normalize_path};
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
/// live. External writers can skew usage numbers, but replacing the root or
/// an ancestor with a symlink cannot redirect filesystem operations. Opened
/// directories retain their identity if the host renames them. The host must
/// not plant hard links to outside files: a hard link grants access to the
/// same inode, regardless of where its other names are located.
#[derive(Debug)]
pub struct LocalVfs {
    root: PathBuf,
    root_dir: File,
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
        let root_dir = File::from(rustix::fs::open(&root, directory_flags(), Mode::empty())?);

        let mut used_bytes = 0;
        let mut file_count = 0;
        scan_tree(&root_dir, &mut used_bytes, &mut file_count)?;

        Ok(Self {
            root,
            root_dir,
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

    /// Returns the root's canonicalized path at construction. Operations use
    /// the opened directory, which remains valid if this path is replaced.
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

    /// Rescans the backing directory and replaces quota usage with the
    /// result, returning the new numbers.
    ///
    /// Call this after the host mutates the tree so enforcement runs against
    /// reality again. Unlinked-but-open files are no longer visible in the
    /// tree but keep their bytes and entry slot accounted until the last
    /// handle closes, exactly as live accounting does.
    pub fn refresh(&self) -> VfsResult<VfsStats> {
        let mut state = self.state();
        let mut used_bytes = 0;
        let mut file_count = 0;
        scan_tree(&self.root_dir, &mut used_bytes, &mut file_count)
            .map_err(|err| io_error(&err))?;

        let mut counted = BTreeSet::new();
        for handle in state.handles.values() {
            let unlinked = state
                .open_files
                .get(&handle.key)
                .is_some_and(|open_file| open_file.unlinked);
            if unlinked && counted.insert(handle.key) {
                used_bytes = used_bytes.saturating_add(file_len(&handle.file).unwrap_or(0));
                file_count += 1;
            }
        }

        state.used_bytes = used_bytes;
        state.file_count = file_count;
        Ok(VfsStats {
            used_bytes,
            file_count,
        })
    }

    /// Replaces quota usage with externally computed numbers.
    ///
    /// For hosts that track usage out of band instead of trusting the VFS to
    /// aggregate it. Subsequent operations apply their deltas on top of the
    /// pushed baseline, and quota enforcement blocks growth against it.
    /// Numbers above the quota are accepted; they only prevent further
    /// growth, matching how construction treats oversized existing trees.
    pub fn set_usage(&self, usage: VfsStats) {
        let mut state = self.state();
        state.used_bytes = usage.used_bytes;
        state.file_count = usage.file_count;
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens each ancestor relative to its already-open parent. Each syscall
    /// receives one normalized component and refuses symlinks; namespace
    /// replacement cannot redirect a later operation through a checked path.
    fn resolve(&self, path: &str) -> VfsResult<Resolved> {
        let components = normalize_path(path)?;
        let mut parent = self.root_dir.try_clone().map_err(|err| io_error(&err))?;
        for component in components.iter().take(components.len().saturating_sub(1)) {
            parent = open_directory(&parent, component)?;
        }
        let name = components.last().cloned().unwrap_or_else(|| ".".into());
        Ok(Resolved {
            parent,
            name,
            components,
        })
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
        let meta = resolved.lookup()?.ok_or(VfsError::new(Errno::ENOENT))?;
        metadata_from(&meta).ok_or(VfsError::new(Errno::ENOENT))
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let resolved = self.resolve(path)?;
        let _guard = self.state();
        let dir = open_directory(&resolved.parent, &resolved.name)?;
        let mut entries = Vec::new();
        for entry in Dir::read_from(&dir).map_err(os_error)? {
            let entry = entry.map_err(os_error)?;
            // Names the String-based VFS API cannot express are invisible.
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if matches!(name, "." | "..") {
                continue;
            }
            let Some(meta) = lookup(&dir, name)? else {
                continue;
            };
            if let Some(metadata) = metadata_from(&meta) {
                entries.push(DirEntry {
                    name: name.into(),
                    metadata,
                });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn mkdir(&self, path: &str) -> VfsResult<()> {
        let resolved = self.resolve(path)?;
        if resolved.is_root() {
            return Err(VfsError::new(Errno::EEXIST));
        }

        let mut state = self.state();
        if resolved.lookup()?.is_some() {
            return Err(VfsError::new(Errno::EEXIST));
        }
        self.ensure_entry_slot(&state)?;

        mkdirat(&resolved.parent, &resolved.name, Mode::from_raw_mode(0o777)).map_err(os_error)?;
        state.file_count += 1;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let from_resolved = self.resolve(from)?;
        let to_resolved = self.resolve(to)?;
        if from_resolved.is_root() || to_resolved.is_root() {
            return Err(VfsError::new(Errno::EINVAL));
        }

        let mut guard = self.state();
        let state = &mut *guard;
        let source_kind = match from_resolved.lookup()?.as_ref().map(entry_kind) {
            Some(EntryKind::Other) | None => return Err(VfsError::new(Errno::ENOENT)),
            Some(kind) => kind,
        };
        if from_resolved.components == to_resolved.components {
            return Ok(());
        }

        if source_kind == EntryKind::Directory
            && to_resolved
                .components
                .starts_with(&from_resolved.components)
        {
            return Err(VfsError::new(Errno::EINVAL));
        }

        let target = to_resolved.lookup()?;
        renameat(
            &from_resolved.parent,
            &from_resolved.name,
            &to_resolved.parent,
            &to_resolved.name,
        )
        .map_err(|err| {
            // POSIX allows either code when the target directory is non-empty.
            let err = os_error(err);
            if err.errno() == Errno::EEXIST {
                VfsError::new(Errno::ENOTEMPTY)
            } else {
                err
            }
        })?;

        if let Some(meta) = target {
            match meta.kind {
                EntryKind::File => release_entry(state, meta.key, meta.len),
                EntryKind::Directory => state.file_count = state.file_count.saturating_sub(1),
                EntryKind::Other => {}
            }
        }
        Ok(())
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        let resolved = self.resolve(path)?;
        if resolved.is_root() {
            return Err(VfsError::new(Errno::EISDIR));
        }

        let mut state = self.state();
        let meta = resolved.lookup()?.ok_or(VfsError::new(Errno::ENOENT))?;
        match entry_kind(&meta) {
            EntryKind::File => {}
            EntryKind::Directory => return Err(VfsError::new(Errno::EISDIR)),
            EntryKind::Other => return Err(VfsError::new(Errno::ENOENT)),
        }

        unlinkat(&resolved.parent, &resolved.name, AtFlags::empty()).map_err(os_error)?;
        release_entry(&mut state, meta.key, meta.len);
        Ok(())
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        let resolved = self.resolve(path)?;
        if resolved.is_root() {
            return Err(VfsError::new(Errno::EBUSY));
        }

        let mut state = self.state();
        match resolved.lookup()?.as_ref().map(entry_kind) {
            Some(EntryKind::Directory) => {}
            Some(EntryKind::File) => return Err(VfsError::new(Errno::ENOTDIR)),
            Some(EntryKind::Other) | None => return Err(VfsError::new(Errno::ENOENT)),
        }

        unlinkat(&resolved.parent, &resolved.name, AtFlags::REMOVEDIR).map_err(|err| {
            // POSIX allows either code when the directory is non-empty.
            let err = os_error(err);
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
        let creating = match resolved.lookup()? {
            Some(meta) => match entry_kind(&meta) {
                EntryKind::Directory => return Err(VfsError::new(Errno::EISDIR)),
                EntryKind::Other => return Err(VfsError::new(Errno::EACCES)),
                EntryKind::File if mode.create_new => {
                    return Err(VfsError::new(Errno::EEXIST));
                }
                EntryKind::File => false,
            },
            None if mode.create || mode.create_new => {
                self.ensure_entry_slot(&state)?;
                true
            }
            None => return Err(VfsError::new(Errno::ENOENT)),
        };
        // O_NOFOLLOW keeps a symlink swapped in after the lookup from being
        // followed; O_NONBLOCK keeps a swapped-in FIFO from blocking the open.
        // Validate the opened file before truncating so replacement with a
        // special file cannot make truncation act on an unchecked object.
        let mut flags = OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        flags |= match (mode.read, mode.write || creating) {
            (true, true) => OFlags::RDWR,
            (false, true) => OFlags::WRONLY,
            _ => OFlags::RDONLY,
        };
        if creating {
            // A racing host creation must not consume an unaccounted entry.
            flags |= OFlags::CREATE | OFlags::EXCL;
        }
        let file = File::from(
            openat(
                &resolved.parent,
                &resolved.name,
                flags,
                Mode::from_raw_mode(0o666),
            )
            .map_err(os_error)?,
        );

        let meta = file.metadata().map_err(|err| io_error(&err))?;
        if !meta.is_file() {
            return Err(VfsError::new(Errno::EACCES));
        }

        if mode.truncate {
            file.set_len(0).map_err(|err| io_error(&err))?;
            state.used_bytes = state.used_bytes.saturating_sub(meta.len());
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

/// The only namespace reference passed to mutations: an owned parent and a
/// single entry name. Neither a host pathname nor a symlink is resolved again.
struct Resolved {
    parent: File,
    name: String,
    components: Vec<String>,
}

impl Resolved {
    fn is_root(&self) -> bool {
        self.components.is_empty()
    }

    fn lookup(&self) -> VfsResult<Option<EntryMetadata>> {
        lookup(&self.parent, &self.name)
    }
}

struct EntryMetadata {
    kind: EntryKind,
    key: FileKey,
    len: u64,
}

fn entry_kind(meta: &EntryMetadata) -> EntryKind {
    meta.kind
}

fn metadata_from(meta: &EntryMetadata) -> Option<Metadata> {
    match entry_kind(meta) {
        EntryKind::File => Some(Metadata {
            file_type: FileType::File,
            len: meta.len,
        }),
        EntryKind::Directory => Some(Metadata {
            file_type: FileType::Directory,
            len: 0,
        }),
        EntryKind::Other => None,
    }
}

/// Stats one entry without following symlinks; `None` when it does not exist.
fn lookup(dir: &File, name: &str) -> VfsResult<Option<EntryMetadata>> {
    lookup_io(dir, name).map_err(|err| io_error(&err))
}

// dev_t and ino_t have different widths/signedness across supported Unix hosts.
#[allow(clippy::unnecessary_cast)]
fn lookup_io(dir: impl AsFd, name: impl rustix::path::Arg) -> io::Result<Option<EntryMetadata>> {
    match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(meta) => Ok(Some(EntryMetadata {
            kind: match rustix::fs::FileType::from_raw_mode(meta.st_mode) {
                rustix::fs::FileType::RegularFile => EntryKind::File,
                rustix::fs::FileType::Directory => EntryKind::Directory,
                _ => EntryKind::Other,
            },
            key: (meta.st_dev as u64, meta.st_ino as u64),
            len: meta.st_size.max(0) as u64,
        })),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn open_directory(parent: &File, name: &str) -> VfsResult<File> {
    match openat(parent, name, directory_flags(), Mode::empty()) {
        Ok(fd) => Ok(fd.into()),
        // NOFOLLOW|DIRECTORY reports ELOOP on some Unix hosts and ENOTDIR
        // on others. Preserve the VFS's invisible-link semantics in both.
        Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
            match lookup(parent, name)?.as_ref().map(entry_kind) {
                Some(EntryKind::File) => Err(VfsError::new(Errno::ENOTDIR)),
                _ => Err(VfsError::new(Errno::ENOENT)),
            }
        }
        Err(err) => Err(os_error(err)),
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

fn scan_tree(root: &File, used_bytes: &mut u64, file_count: &mut u64) -> io::Result<()> {
    // Keep only the active ancestry open, without recursive Rust calls or a
    // pending descriptor for every sibling directory in a wide host tree.
    let mut stack = vec![Dir::read_from(root)?];
    loop {
        let depth = stack.len();
        let Some(entries) = stack.last_mut() else {
            break;
        };
        let Some(entry) = entries.next() else {
            stack.pop();
            continue;
        };
        let entry = entry?;
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        let dir = entries.fd()?;
        let Some(meta) = lookup_io(dir, name)? else {
            continue;
        };
        if meta.kind != EntryKind::Other && depth > MAX_PATH_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "LocalVfs tree exceeds maximum path depth",
            ));
        }
        match meta.kind {
            EntryKind::Directory => {
                let child = match openat(dir, name, directory_flags(), Mode::empty()) {
                    Ok(fd) => fd,
                    // A concurrent unlink/symlink replacement is invisible.
                    Err(
                        rustix::io::Errno::NOENT
                        | rustix::io::Errno::NOTDIR
                        | rustix::io::Errno::LOOP,
                    ) => continue,
                    Err(err) => return Err(err.into()),
                };
                *file_count = file_count.saturating_add(1);
                stack.push(Dir::new(child)?);
            }
            EntryKind::File => {
                *file_count = file_count.saturating_add(1);
                *used_bytes = used_bytes.saturating_add(meta.len);
            }
            EntryKind::Other => {}
        }
    }
    Ok(())
}

fn os_error(err: rustix::io::Errno) -> VfsError {
    io_error(&err.into())
}

fn io_error(err: &io::Error) -> VfsError {
    let errno = match err.raw_os_error() {
        Some(libc::ENOENT) => Errno::ENOENT,
        Some(libc::ENOTDIR) => Errno::ENOTDIR,
        Some(libc::EISDIR) => Errno::EISDIR,
        Some(libc::EEXIST) => Errno::EEXIST,
        Some(libc::ENOTEMPTY) => Errno::ENOTEMPTY,
        Some(libc::EXDEV) => Errno::EXDEV,
        Some(libc::EACCES | libc::EPERM | libc::ELOOP | libc::EROFS) => Errno::EACCES,
        Some(libc::ENOSPC | libc::EDQUOT | libc::EFBIG) => Errno::ENOSPC,
        Some(libc::EBUSY) => Errno::EBUSY,
        Some(libc::EBADF) => Errno::EBADF,
        _ => Errno::EINVAL,
    };
    VfsError::new(errno)
}
