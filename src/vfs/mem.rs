//! In-memory VFS with quota enforcement and copy-on-write snapshots.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::path::normalize_path;
use super::{
    DirEntry, Errno, FileHandle, FileType, Metadata, OpenMode, Vfs, VfsError, VfsResult,
    VfsSnapshot,
};

/// Storage limits enforced by [`InMemoryVfs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfsQuota {
    /// Maximum total bytes stored in visible files and open unlinked files.
    pub max_bytes: u64,
    /// Maximum non-root directory entries, including files and directories.
    pub max_files: u64,
    /// Maximum size of any single file.
    pub max_file_size: u64,
}

impl VfsQuota {
    /// Returns a quota with all limits set to `u64::MAX`.
    pub const fn unlimited() -> Self {
        Self {
            max_bytes: u64::MAX,
            max_files: u64::MAX,
            max_file_size: u64::MAX,
        }
    }
}

impl Default for VfsQuota {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// Current quota usage for a VFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfsStats {
    /// Total bytes stored in files.
    pub used_bytes: u64,
    /// Count of non-root files and directories.
    pub file_count: u64,
}

/// Quota-enforced in-memory implementation of [`Vfs`].
#[derive(Debug)]
pub struct InMemoryVfs {
    quota: VfsQuota,
    state: Mutex<State>,
}

/// Copy-on-write snapshot captured from an [`InMemoryVfs`].
#[derive(Debug, Clone)]
pub struct InMemoryVfsSnapshot {
    state: SnapshotState,
}

impl InMemoryVfs {
    /// Creates an empty in-memory VFS with `quota`.
    pub fn new(quota: VfsQuota) -> Self {
        Self {
            quota,
            state: Mutex::new(State::default()),
        }
    }

    /// Returns current quota usage.
    pub fn stats(&self) -> VfsResult<VfsStats> {
        let state = self.state();
        Ok(VfsStats {
            used_bytes: state.used_bytes,
            file_count: state.file_count,
        })
    }

    fn from_snapshot(quota: VfsQuota, snapshot: &InMemoryVfsSnapshot) -> VfsResult<Self> {
        ensure_snapshot_fits_quota(&snapshot.state, quota)?;
        Ok(Self {
            quota,
            state: Mutex::new(State::from_snapshot(&snapshot.state, 1)),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ensure_entry_slot(&self, state: &State) -> VfsResult<()> {
        if state.file_count >= self.quota.max_files {
            return Err(VfsError::new(Errno::ENOSPC));
        }

        Ok(())
    }

    fn resize_file(&self, state: &mut State, inode: Inode, new_len: usize) -> VfsResult<()> {
        let old_len = state
            .files
            .get(&inode)
            .ok_or(VfsError::new(Errno::ENOENT))?
            .data
            .len();

        let new_len_u64 = u64::try_from(new_len).map_err(|_| VfsError::new(Errno::ENOSPC))?;
        if new_len_u64 > self.quota.max_file_size {
            return Err(VfsError::new(Errno::ENOSPC));
        }

        let used_bytes = adjusted_used_bytes(state.used_bytes, old_len, new_len)?;
        if used_bytes > self.quota.max_bytes {
            return Err(VfsError::new(Errno::ENOSPC));
        }

        let file = state
            .files
            .get_mut(&inode)
            .ok_or(VfsError::new(Errno::ENOENT))?;
        if new_len < old_len && Arc::strong_count(&file.data) > 1 {
            file.data = Arc::new(file.data[..new_len].to_vec());
        } else {
            Arc::make_mut(&mut file.data).resize(new_len, 0);
        }
        state.used_bytes = used_bytes;
        Ok(())
    }
}

impl Default for InMemoryVfs {
    fn default() -> Self {
        Self::new(VfsQuota::default())
    }
}

impl Vfs for InMemoryVfs {
    fn is_fast(&self) -> bool {
        true
    }

    fn stats(&self) -> Option<VfsResult<VfsStats>> {
        Some(InMemoryVfs::stats(self))
    }

    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        let path = normalize_path(path)?;
        let state = self.state();
        metadata_for_node(&state, get_node(&state.root, &path)?)
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let path = normalize_path(path)?;
        let state = self.state();

        match get_node(&state.root, &path)? {
            Node::File(_) => Err(VfsError::new(Errno::ENOTDIR)),
            Node::Directory(entries) => entries
                .iter()
                .map(|(name, node)| {
                    Ok(DirEntry {
                        name: name.clone(),
                        metadata: metadata_for_node(&state, node)?,
                    })
                })
                .collect(),
        }
    }

    fn mkdir(&self, path: &str) -> VfsResult<()> {
        let path = normalize_path(path)?;
        if path.is_empty() {
            return Err(VfsError::new(Errno::EEXIST));
        }

        let mut state = self.state();
        match get_node(&state.root, &path) {
            Ok(_) => return Err(VfsError::new(Errno::EEXIST)),
            Err(err) if err.errno() == Errno::ENOENT => {}
            Err(err) => return Err(err),
        }
        ensure_parent_is_dir(&state.root, &path)?;
        self.ensure_entry_slot(&state)?;

        let (parent, name) = parent_dir_mut(&mut state.root, &path)?;
        parent.insert(name.to_owned(), Node::Directory(BTreeMap::new()));
        state.file_count += 1;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let from_path = normalize_path(from)?;
        let to_path = normalize_path(to)?;

        if from_path.is_empty() || to_path.is_empty() {
            return Err(VfsError::new(Errno::EINVAL));
        }

        let mut state = self.state();
        let source_kind = node_kind(get_node(&state.root, &from_path)?);
        if from_path == to_path {
            return Ok(());
        }

        if source_kind == NodeKind::Directory && has_prefix(&to_path, &from_path) {
            return Err(VfsError::new(Errno::EINVAL));
        }

        ensure_parent_is_dir(&state.root, &to_path)?;
        match get_node(&state.root, &to_path) {
            Ok(target) => match (source_kind, target) {
                (NodeKind::File, Node::File(_)) => {}
                (NodeKind::File, Node::Directory(_)) => {
                    return Err(VfsError::new(Errno::EISDIR));
                }
                (NodeKind::Directory, Node::File(_)) => {
                    return Err(VfsError::new(Errno::ENOTDIR));
                }
                (NodeKind::Directory, Node::Directory(entries)) if !entries.is_empty() => {
                    return Err(VfsError::new(Errno::ENOTEMPTY));
                }
                (NodeKind::Directory, Node::Directory(_)) => {}
            },
            Err(err) if err.errno() == Errno::ENOENT => {}
            Err(err) => return Err(err),
        }

        let removed = {
            let (parent, name) = parent_dir_mut(&mut state.root, &from_path)?;
            parent.remove(name).ok_or(VfsError::new(Errno::ENOENT))?
        };

        let replaced = {
            let (parent, name) = parent_dir_mut(&mut state.root, &to_path)?;
            parent.insert(name.to_owned(), removed)
        };

        if let Some(replaced) = replaced {
            remove_tree(&mut state, replaced)?;
        }

        Ok(())
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        let path = normalize_path(path)?;
        if path.is_empty() {
            return Err(VfsError::new(Errno::EISDIR));
        }

        let mut state = self.state();
        let removed = {
            let (parent, name) = parent_dir_mut(&mut state.root, &path)?;
            match parent.get(name) {
                Some(Node::File(_)) => {}
                Some(Node::Directory(_)) => return Err(VfsError::new(Errno::EISDIR)),
                None => return Err(VfsError::new(Errno::ENOENT)),
            }
            parent.remove(name).ok_or(VfsError::new(Errno::ENOENT))?
        };

        remove_tree(&mut state, removed)
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        let path = normalize_path(path)?;
        if path.is_empty() {
            return Err(VfsError::new(Errno::EBUSY));
        }

        let mut state = self.state();
        let removed = {
            let (parent, name) = parent_dir_mut(&mut state.root, &path)?;
            match parent.get(name) {
                Some(Node::File(_)) => return Err(VfsError::new(Errno::ENOTDIR)),
                Some(Node::Directory(entries)) if !entries.is_empty() => {
                    return Err(VfsError::new(Errno::ENOTEMPTY));
                }
                Some(Node::Directory(_)) => {}
                None => return Err(VfsError::new(Errno::ENOENT)),
            }
            parent.remove(name).ok_or(VfsError::new(Errno::ENOENT))?
        };

        remove_tree(&mut state, removed)
    }

    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        mode.validate()?;
        let path = normalize_path(path)?;

        let mut state = self.state();
        let inode = match get_node(&state.root, &path) {
            Ok(Node::Directory(_)) => return Err(VfsError::new(Errno::EISDIR)),
            Ok(Node::File(_)) if mode.create_new => {
                return Err(VfsError::new(Errno::EEXIST));
            }
            Ok(Node::File(inode)) => {
                let inode = *inode;
                if mode.truncate {
                    self.resize_file(&mut state, inode, 0)?;
                }
                inode
            }
            Err(err) if err.errno() == Errno::ENOENT && (mode.create || mode.create_new) => {
                ensure_parent_is_dir(&state.root, &path)?;
                self.ensure_entry_slot(&state)?;

                let inode = state.next_inode;
                state.next_inode += 1;
                state.files.insert(
                    inode,
                    FileNode {
                        links: 1,
                        ..FileNode::default()
                    },
                );
                let (parent, name) = parent_dir_mut(&mut state.root, &path)?;
                parent.insert(name.to_owned(), Node::File(inode));
                state.file_count += 1;
                inode
            }
            Err(err) => return Err(err),
        };

        let handle = FileHandle(state.next_handle);
        state.next_handle += 1;
        state.handles.insert(
            handle,
            Handle {
                inode,
                readable: mode.read,
                writable: mode.write,
                append: mode.append,
            },
        );
        state
            .files
            .get_mut(&inode)
            .ok_or(VfsError::new(Errno::ENOENT))?
            .open_handles += 1;
        Ok(handle)
    }

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let offset = usize::try_from(offset).map_err(|_| VfsError::new(Errno::EINVAL))?;
        let state = self.state();
        let handle = state
            .handles
            .get(&handle)
            .copied()
            .ok_or(VfsError::new(Errno::EBADF))?;

        if !handle.readable {
            return Err(VfsError::new(Errno::EBADF));
        }

        let data = &state
            .files
            .get(&handle.inode)
            .ok_or(VfsError::new(Errno::ENOENT))?
            .data;
        if offset >= data.len() {
            return Ok(0);
        }

        let available = data.len() - offset;
        let len = available.min(buf.len());
        buf[..len].copy_from_slice(&data[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        let mut state = self.state();
        let handle = state
            .handles
            .get(&handle)
            .copied()
            .ok_or(VfsError::new(Errno::EBADF))?;

        if !handle.writable {
            return Err(VfsError::new(Errno::EBADF));
        }

        if data.is_empty() {
            return Ok(0);
        }

        let offset = usize::try_from(offset).map_err(|_| VfsError::new(Errno::EINVAL))?;
        let old_len = state
            .files
            .get(&handle.inode)
            .ok_or(VfsError::new(Errno::ENOENT))?
            .data
            .len();
        let write_offset = if handle.append { old_len } else { offset };
        let write_end = write_offset
            .checked_add(data.len())
            .ok_or(VfsError::new(Errno::EINVAL))?;
        let new_len = old_len.max(write_end);

        self.resize_file(&mut state, handle.inode, new_len)?;
        let file = state
            .files
            .get_mut(&handle.inode)
            .ok_or(VfsError::new(Errno::ENOENT))?;
        Arc::make_mut(&mut file.data)[write_offset..write_end].copy_from_slice(data);
        Ok(data.len())
    }

    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        let len = usize::try_from(len).map_err(|_| VfsError::new(Errno::ENOSPC))?;
        let mut state = self.state();
        let handle = state
            .handles
            .get(&handle)
            .copied()
            .ok_or(VfsError::new(Errno::EBADF))?;

        if !handle.writable {
            return Err(VfsError::new(Errno::EINVAL));
        }

        self.resize_file(&mut state, handle.inode, len)
    }

    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        let mut state = self.state();
        let handle = state
            .handles
            .remove(&handle)
            .ok_or(VfsError::new(Errno::EBADF))?;

        let file = state
            .files
            .get_mut(&handle.inode)
            .ok_or(VfsError::new(Errno::ENOENT))?;
        file.open_handles -= 1;
        release_file_if_unreferenced(&mut state, handle.inode)
    }
}

impl VfsSnapshot for InMemoryVfs {
    type Snapshot = InMemoryVfsSnapshot;

    fn snapshot(&self) -> VfsResult<Self::Snapshot> {
        let state = self.state();
        Ok(InMemoryVfsSnapshot {
            state: SnapshotState::from_state(&state)?,
        })
    }

    fn restore(&self, snapshot: &Self::Snapshot) -> VfsResult<()> {
        ensure_snapshot_fits_quota(&snapshot.state, self.quota)?;
        let mut state = self.state();
        let next_handle = state.next_handle;
        *state = State::from_snapshot(&snapshot.state, next_handle);
        Ok(())
    }

    fn branch(&self, snapshot: &Self::Snapshot) -> VfsResult<Self> {
        Self::from_snapshot(self.quota, snapshot)
    }
}

type Directory = BTreeMap<String, Node>;
type Inode = u64;

#[derive(Debug)]
struct State {
    root: Node,
    files: BTreeMap<Inode, FileNode>,
    next_inode: Inode,
    next_handle: u64,
    handles: BTreeMap<FileHandle, Handle>,
    used_bytes: u64,
    file_count: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            root: Node::Directory(BTreeMap::new()),
            files: BTreeMap::new(),
            next_inode: 1,
            next_handle: 1,
            handles: BTreeMap::new(),
            used_bytes: 0,
            file_count: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct SnapshotState {
    root: Node,
    files: BTreeMap<Inode, FileNode>,
    next_inode: Inode,
    used_bytes: u64,
    file_count: u64,
}

impl SnapshotState {
    fn from_state(state: &State) -> VfsResult<Self> {
        let mut files = BTreeMap::new();
        let mut used_bytes = 0;
        let mut file_count = 0;
        collect_snapshot_entries(
            &state.root,
            &state.files,
            &mut files,
            &mut used_bytes,
            &mut file_count,
        )?;
        Ok(Self {
            root: state.root.clone(),
            files,
            next_inode: state.next_inode,
            used_bytes,
            file_count,
        })
    }
}

impl State {
    fn from_snapshot(snapshot: &SnapshotState, next_handle: u64) -> Self {
        Self {
            root: snapshot.root.clone(),
            files: snapshot.files.clone(),
            next_inode: snapshot.next_inode,
            next_handle,
            handles: BTreeMap::new(),
            used_bytes: snapshot.used_bytes,
            file_count: snapshot.file_count,
        }
    }
}

#[derive(Debug, Clone)]
struct FileNode {
    data: Arc<Vec<u8>>,
    links: u64,
    open_handles: u64,
}

impl Default for FileNode {
    fn default() -> Self {
        Self {
            data: Arc::new(Vec::new()),
            links: 0,
            open_handles: 0,
        }
    }
}

#[derive(Debug, Clone)]
enum Node {
    File(Inode),
    Directory(Directory),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Copy)]
struct Handle {
    inode: Inode,
    readable: bool,
    writable: bool,
    append: bool,
}

fn metadata_for_node(state: &State, node: &Node) -> VfsResult<Metadata> {
    match node {
        Node::File(inode) => Ok(Metadata {
            file_type: FileType::File,
            len: u64::try_from(
                state
                    .files
                    .get(inode)
                    .ok_or(VfsError::new(Errno::ENOENT))?
                    .data
                    .len(),
            )
            .unwrap_or(u64::MAX),
        }),
        Node::Directory(_) => Ok(Metadata {
            file_type: FileType::Directory,
            len: 0,
        }),
    }
}

fn node_kind(node: &Node) -> NodeKind {
    match node {
        Node::File(_) => NodeKind::File,
        Node::Directory(_) => NodeKind::Directory,
    }
}

fn get_node<'a>(root: &'a Node, path: &[String]) -> VfsResult<&'a Node> {
    let mut current = root;

    for component in path {
        current = match current {
            Node::File(_) => return Err(VfsError::new(Errno::ENOTDIR)),
            Node::Directory(entries) => {
                entries.get(component).ok_or(VfsError::new(Errno::ENOENT))?
            }
        };
    }

    Ok(current)
}

fn get_node_mut<'a>(root: &'a mut Node, path: &[String]) -> VfsResult<&'a mut Node> {
    let mut current = root;

    for component in path {
        current = match current {
            Node::File(_) => return Err(VfsError::new(Errno::ENOTDIR)),
            Node::Directory(entries) => entries
                .get_mut(component)
                .ok_or(VfsError::new(Errno::ENOENT))?,
        };
    }

    Ok(current)
}

fn parent_dir_mut<'a>(
    root: &'a mut Node,
    path: &'a [String],
) -> VfsResult<(&'a mut Directory, &'a str)> {
    let Some((name, parent_path)) = path.split_last() else {
        return Err(VfsError::new(Errno::EINVAL));
    };

    match get_node_mut(root, parent_path)? {
        Node::File(_) => Err(VfsError::new(Errno::ENOTDIR)),
        Node::Directory(entries) => Ok((entries, name)),
    }
}

fn ensure_parent_is_dir(root: &Node, path: &[String]) -> VfsResult<()> {
    let Some((_name, parent_path)) = path.split_last() else {
        return Err(VfsError::new(Errno::EINVAL));
    };

    match get_node(root, parent_path)? {
        Node::File(_) => Err(VfsError::new(Errno::ENOTDIR)),
        Node::Directory(_) => Ok(()),
    }
}

fn adjusted_used_bytes(used_bytes: u64, old_len: usize, new_len: usize) -> VfsResult<u64> {
    let old_len = u64::try_from(old_len).map_err(|_| VfsError::new(Errno::EINVAL))?;
    let new_len = u64::try_from(new_len).map_err(|_| VfsError::new(Errno::ENOSPC))?;

    if new_len >= old_len {
        used_bytes
            .checked_add(new_len - old_len)
            .ok_or(VfsError::new(Errno::ENOSPC))
    } else {
        Ok(used_bytes - (old_len - new_len))
    }
}

fn collect_snapshot_entries(
    node: &Node,
    state_files: &BTreeMap<Inode, FileNode>,
    snapshot_files: &mut BTreeMap<Inode, FileNode>,
    used_bytes: &mut u64,
    file_count: &mut u64,
) -> VfsResult<()> {
    match node {
        Node::File(inode) => {
            let file = state_files.get(inode).ok_or(VfsError::new(Errno::ENOENT))?;
            let len = u64::try_from(file.data.len()).map_err(|_| VfsError::new(Errno::EINVAL))?;
            *used_bytes = used_bytes
                .checked_add(len)
                .ok_or(VfsError::new(Errno::ENOSPC))?;
            *file_count = file_count
                .checked_add(1)
                .ok_or(VfsError::new(Errno::ENOSPC))?;
            snapshot_files.insert(
                *inode,
                FileNode {
                    data: Arc::clone(&file.data),
                    links: 1,
                    open_handles: 0,
                },
            );
            Ok(())
        }
        Node::Directory(entries) => {
            for child in entries.values() {
                *file_count = file_count
                    .checked_add(matches!(child, Node::Directory(_)) as u64)
                    .ok_or(VfsError::new(Errno::ENOSPC))?;
                collect_snapshot_entries(
                    child,
                    state_files,
                    snapshot_files,
                    used_bytes,
                    file_count,
                )?;
            }
            Ok(())
        }
    }
}

fn ensure_snapshot_fits_quota(snapshot: &SnapshotState, quota: VfsQuota) -> VfsResult<()> {
    if snapshot.used_bytes > quota.max_bytes || snapshot.file_count > quota.max_files {
        return Err(VfsError::new(Errno::ENOSPC));
    }

    for file in snapshot.files.values() {
        let len = u64::try_from(file.data.len()).map_err(|_| VfsError::new(Errno::ENOSPC))?;
        if len > quota.max_file_size {
            return Err(VfsError::new(Errno::ENOSPC));
        }
    }

    Ok(())
}

fn remove_tree(state: &mut State, node: Node) -> VfsResult<()> {
    match node {
        Node::File(inode) => {
            let file = state
                .files
                .get_mut(&inode)
                .ok_or(VfsError::new(Errno::ENOENT))?;
            file.links -= 1;
            release_file_if_unreferenced(state, inode)
        }
        Node::Directory(entries) => {
            for child in entries.into_values() {
                remove_tree(state, child)?;
            }
            state.file_count -= 1;
            Ok(())
        }
    }
}

fn release_file_if_unreferenced(state: &mut State, inode: Inode) -> VfsResult<()> {
    let Some(file) = state.files.get(&inode) else {
        return Ok(());
    };
    if file.links > 0 || file.open_handles > 0 {
        return Ok(());
    }

    let file = state
        .files
        .remove(&inode)
        .ok_or(VfsError::new(Errno::ENOENT))?;
    let len = u64::try_from(file.data.len()).map_err(|_| VfsError::new(Errno::EINVAL))?;
    state.used_bytes -= len;
    state.file_count -= 1;
    Ok(())
}

fn has_prefix(path: &[String], prefix: &[String]) -> bool {
    path.len() >= prefix.len() && path[..prefix.len()] == *prefix
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{InMemoryVfs, VfsQuota};
    use crate::vfs::{Errno, OpenMode, Vfs, VfsSnapshot};

    #[test]
    fn write_extending_past_eof_fills_gap_with_zeroes() {
        let vfs = InMemoryVfs::default();
        let handle = vfs
            .open("/file", OpenMode::read_write().create_new())
            .expect("file opens");

        vfs.write_at(handle, 3, b"x").expect("write succeeds");

        let mut buf = [1; 4];
        let read = vfs.read_at(handle, 0, &mut buf).expect("read succeeds");
        assert_eq!(read, 4);
        assert_eq!(buf, [0, 0, 0, b'x']);
    }

    #[test]
    fn max_file_size_is_enforced_before_mutation() {
        let vfs = InMemoryVfs::new(VfsQuota {
            max_bytes: 10,
            max_files: 1,
            max_file_size: 3,
        });
        let handle = vfs
            .open("/file", OpenMode::write_only().create_new())
            .expect("file opens");

        let err = vfs
            .write_at(handle, 0, b"abcd")
            .expect_err("oversized write is rejected");
        assert_eq!(err.errno(), Errno::ENOSPC);
        assert_eq!(vfs.stats().expect("stats").used_bytes, 0);
    }

    #[test]
    fn file_count_quota_rejects_new_file() {
        let vfs = InMemoryVfs::new(VfsQuota {
            max_bytes: 10,
            max_files: 1,
            max_file_size: 10,
        });

        vfs.open("/a", OpenMode::write_only().create_new())
            .expect("first file opens");
        let err = vfs
            .open("/b", OpenMode::write_only().create_new())
            .expect_err("second file exceeds quota");

        assert_eq!(err.errno(), Errno::ENOSPC);
        assert_eq!(vfs.stats().expect("stats").file_count, 1);
    }

    #[test]
    fn snapshot_data_is_shared_until_live_write() {
        // This pins the CoW invariant snapshots rely on: reads keep file data
        // shared, while writes detach the live file from the snapshot.
        let vfs = InMemoryVfs::default();
        let handle = vfs
            .open("/file", OpenMode::read_write().create_new())
            .expect("file opens");
        vfs.write_at(handle, 0, b"abcdef")
            .expect("seed write succeeds");

        let snapshot = vfs.snapshot().expect("snapshot succeeds");
        let live_after_snapshot = only_file_data(&vfs);
        let snapshot_data = snapshot
            .state
            .files
            .values()
            .next()
            .expect("snapshot file exists")
            .data
            .clone();
        assert!(Arc::ptr_eq(&live_after_snapshot, &snapshot_data));

        let mut buf = [0; 3];
        vfs.read_at(handle, 0, &mut buf)
            .expect("read keeps data shared");
        assert!(Arc::ptr_eq(&only_file_data(&vfs), &snapshot_data));

        vfs.write_at(handle, 0, b"Z").expect("write detaches data");
        assert!(!Arc::ptr_eq(&only_file_data(&vfs), &snapshot_data));
        vfs.close(handle).expect("close handle");
    }

    #[test]
    fn truncating_shared_snapshot_data_copies_only_the_prefix() {
        // Shrinking a shared buffer should detach to the requested prefix
        // without cloning the discarded suffix first.
        let vfs = InMemoryVfs::default();
        let handle = vfs
            .open("/file", OpenMode::read_write().create_new())
            .expect("file opens");
        vfs.write_at(handle, 0, b"abcdef")
            .expect("seed write succeeds");
        let snapshot = vfs.snapshot().expect("snapshot succeeds");
        let snapshot_data = snapshot
            .state
            .files
            .values()
            .next()
            .expect("snapshot file exists")
            .data
            .clone();

        vfs.truncate(handle, 3).expect("truncate succeeds");

        let live_data = only_file_data(&vfs);
        assert!(!Arc::ptr_eq(&live_data, &snapshot_data));
        assert_eq!(&live_data[..], b"abc");
        assert_eq!(&snapshot_data[..], b"abcdef");
        vfs.close(handle).expect("close handle");
    }

    fn only_file_data(vfs: &InMemoryVfs) -> Arc<Vec<u8>> {
        vfs.state()
            .files
            .values()
            .next()
            .expect("file exists")
            .data
            .clone()
    }
}
