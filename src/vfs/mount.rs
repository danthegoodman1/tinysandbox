//! Static top-level mount routing over existing VFS implementations.

use std::collections::{BTreeMap, HashMap, hash_map::Entry};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::path::normalize_path;
use super::{
    DirEntry, Errno, FileHandle, FileType, Metadata, OpenMode, Vfs, VfsError, VfsResult, VfsStats,
};

const FIRST_HANDLE: u64 = 1;

struct MountedHandle {
    vfs: Arc<dyn Vfs>,
    inner: FileHandle,
}

/// A read-only virtual root whose immediate children are static VFS mounts.
///
/// Mount names are single path components. Each backend remains rooted at `/`:
/// a request for `/workspace/src/main.rs` is forwarded to the `workspace`
/// backend as `/src/main.rs`.
pub struct MountedVfs {
    mounts: BTreeMap<String, Arc<dyn Vfs>>,
    handles: Mutex<HashMap<FileHandle, MountedHandle>>,
    next_handle: AtomicU64,
    fast: bool,
}

impl MountedVfs {
    /// Builds a static mount table.
    pub fn new(mounts: BTreeMap<String, Arc<dyn Vfs>>) -> VfsResult<Self> {
        for name in mounts.keys() {
            validate_mount_name(name)?;
        }
        let fast = mounts.values().all(|vfs| vfs.is_fast());
        Ok(Self {
            mounts,
            handles: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(FIRST_HANDLE),
            fast,
        })
    }

    /// Returns the configured mount names in directory-listing order.
    pub fn mount_names(&self) -> impl Iterator<Item = &str> {
        self.mounts.keys().map(String::as_str)
    }

    fn mounted_path(&self, path: &str) -> VfsResult<Option<(String, Arc<dyn Vfs>, String, bool)>> {
        let components = normalize_path(path)?;
        let Some(name) = components.first() else {
            return Ok(None);
        };
        let Some(vfs) = self.mounts.get(name) else {
            return Err(VfsError::new(Errno::ENOENT));
        };
        let at_mount = components.len() == 1;
        let inner = if at_mount {
            "/".to_owned()
        } else {
            format!("/{}", components[1..].join("/"))
        };
        Ok(Some((name.clone(), Arc::clone(vfs), inner, at_mount)))
    }

    fn handle(&self, handle: FileHandle) -> VfsResult<(Arc<dyn Vfs>, FileHandle)> {
        let handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        let mounted = handles.get(&handle).ok_or(VfsError::new(Errno::EBADF))?;
        Ok((Arc::clone(&mounted.vfs), mounted.inner))
    }

    fn insert_handle(&self, vfs: Arc<dyn Vfs>, inner: FileHandle) -> FileHandle {
        let mut handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let raw = self.next_handle.fetch_add(1, Ordering::Relaxed);
            if raw == 0 {
                continue;
            }
            let outer = FileHandle::new(raw);
            if let Entry::Vacant(entry) = handles.entry(outer) {
                entry.insert(MountedHandle { vfs, inner });
                return outer;
            }
        }
    }
}

impl Default for MountedVfs {
    fn default() -> Self {
        Self::new(BTreeMap::new()).expect("an empty mount table is valid")
    }
}

impl Vfs for MountedVfs {
    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        match self.mounted_path(path)? {
            Some((_, vfs, inner, _)) => vfs.stat(&inner),
            None => Ok(directory_metadata()),
        }
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        match self.mounted_path(path)? {
            Some((_, vfs, inner, _)) => vfs.readdir(&inner),
            None => Ok(self
                .mounts
                .keys()
                .map(|name| DirEntry {
                    name: name.clone(),
                    metadata: directory_metadata(),
                })
                .collect()),
        }
    }

    fn mkdir(&self, path: &str) -> VfsResult<()> {
        match self.mounted_path(path) {
            Ok(Some((_, _, _, true))) => Err(VfsError::new(Errno::EEXIST)),
            Ok(Some((_, vfs, inner, false))) => vfs.mkdir(&inner),
            Ok(None) => Err(VfsError::new(Errno::EACCES)),
            Err(err) if err.errno() == Errno::ENOENT => Err(VfsError::new(Errno::EACCES)),
            Err(err) => Err(err),
        }
    }

    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let (from_mount, from_vfs, from_inner, from_root) = self
            .mounted_path(from)?
            .ok_or(VfsError::new(Errno::EBUSY))?;
        if from_root {
            return Err(VfsError::new(Errno::EBUSY));
        }
        let (to_mount, _to_vfs, to_inner, to_root) = self
            .mounted_path(to)
            .map_err(|err| {
                if err.errno() == Errno::ENOENT {
                    VfsError::new(Errno::EACCES)
                } else {
                    err
                }
            })?
            .ok_or(VfsError::new(Errno::EBUSY))?;
        if to_root {
            return Err(VfsError::new(Errno::EBUSY));
        }
        if from_mount != to_mount {
            return Err(VfsError::new(Errno::EXDEV));
        }
        from_vfs.rename(&from_inner, &to_inner)
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        match self.mounted_path(path)? {
            Some((_, _, _, true)) | None => Err(VfsError::new(Errno::EISDIR)),
            Some((_, vfs, inner, false)) => vfs.unlink(&inner),
        }
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        match self.mounted_path(path)? {
            Some((_, _, _, true)) | None => Err(VfsError::new(Errno::EBUSY)),
            Some((_, vfs, inner, false)) => vfs.rmdir(&inner),
        }
    }

    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        let (_, vfs, inner, at_mount) = self
            .mounted_path(path)?
            .ok_or(VfsError::new(Errno::EISDIR))?;
        if at_mount {
            return Err(VfsError::new(Errno::EISDIR));
        }
        let inner_handle = vfs.open(&inner, mode)?;
        Ok(self.insert_handle(vfs, inner_handle))
    }

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let (vfs, inner) = self.handle(handle)?;
        vfs.read_at(inner, offset, buf)
    }

    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        let (vfs, inner) = self.handle(handle)?;
        vfs.write_at(inner, offset, data)
    }

    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        let (vfs, inner) = self.handle(handle)?;
        vfs.truncate(inner, len)
    }

    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        let mounted = self
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&handle)
            .ok_or(VfsError::new(Errno::EBADF))?;
        mounted.vfs.close(mounted.inner)
    }

    fn is_fast(&self) -> bool {
        self.fast
    }

    fn stats(&self) -> Option<VfsResult<VfsStats>> {
        let mut total = VfsStats {
            used_bytes: 0,
            file_count: 0,
        };
        let mut reported = false;
        for vfs in self.mounts.values() {
            let Some(stats) = vfs.stats() else {
                continue;
            };
            reported = true;
            let stats = match stats {
                Ok(stats) => stats,
                Err(err) => return Some(Err(err)),
            };
            total.used_bytes = match total.used_bytes.checked_add(stats.used_bytes) {
                Some(value) => value,
                None => return Some(Err(VfsError::new(Errno::ENOSPC))),
            };
            total.file_count = match total.file_count.checked_add(stats.file_count) {
                Some(value) => value,
                None => return Some(Err(VfsError::new(Errno::ENOSPC))),
            };
        }
        reported.then_some(Ok(total))
    }
}

/// Validates a single-component mount name used immediately beneath `/`.
pub fn validate_mount_name(name: &str) -> VfsResult<()> {
    if name.is_empty()
        || matches!(name, "." | ".." | "bin")
        || name.contains('/')
        || name.contains('\0')
    {
        return Err(VfsError::new(Errno::EINVAL));
    }
    Ok(())
}

const fn directory_metadata() -> Metadata {
    Metadata {
        file_type: FileType::Directory,
        len: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::{MountedVfs, validate_mount_name};
    use crate::vfs::{Errno, InMemoryVfs, OpenMode, Vfs, VfsQuota};

    fn mounted(entries: &[(&str, Arc<InMemoryVfs>)]) -> MountedVfs {
        let mounts = entries
            .iter()
            .map(|(name, vfs)| ((*name).to_owned(), Arc::clone(vfs) as Arc<dyn Vfs>))
            .collect::<BTreeMap<_, _>>();
        MountedVfs::new(mounts).expect("valid mount table")
    }

    #[test]
    fn root_and_mount_paths_are_synthesized_and_routed() {
        let workspace = Arc::new(InMemoryVfs::default());
        workspace.mkdir("/src").expect("seed backend directory");
        let input = Arc::new(InMemoryVfs::default());
        let vfs = mounted(&[
            ("workspace", Arc::clone(&workspace)),
            ("input", Arc::clone(&input)),
        ]);

        assert!(vfs.stat("/").expect("stat virtual root").is_dir());
        assert!(vfs.stat("/workspace").expect("stat mount root").is_dir());
        assert!(
            vfs.stat("/workspace/src")
                .expect("stat mounted path")
                .is_dir()
        );
        let names = vfs
            .readdir("/")
            .expect("list virtual root")
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["input", "workspace"]);
        assert_eq!(
            vfs.readdir("/workspace").expect("list mount root")[0].name,
            "src"
        );
        assert_eq!(
            vfs.stat("/missing").expect_err("unknown mount").errno(),
            Errno::ENOENT
        );
    }

    #[test]
    fn handles_are_unique_and_route_to_the_opening_mount() {
        let left = Arc::new(InMemoryVfs::default());
        let right = Arc::new(InMemoryVfs::default());
        let vfs = mounted(&[("left", left), ("right", right)]);

        let left_handle = vfs
            .open("/left/file", OpenMode::read_write().create_new())
            .expect("open left file");
        let right_handle = vfs
            .open("/right/file", OpenMode::read_write().create_new())
            .expect("open right file");
        assert_ne!(left_handle, right_handle);
        vfs.write_at(left_handle, 0, b"left").expect("write left");
        vfs.write_at(right_handle, 0, b"right")
            .expect("write right");

        let mut buf = [0; 5];
        assert_eq!(vfs.read_at(left_handle, 0, &mut buf).expect("read left"), 4);
        assert_eq!(&buf[..4], b"left");
        assert_eq!(
            vfs.read_at(right_handle, 0, &mut buf).expect("read right"),
            5
        );
        assert_eq!(&buf, b"right");
        vfs.close(left_handle).expect("close left");
        vfs.close(right_handle).expect("close right");
    }

    #[test]
    fn mount_points_are_immutable_and_cross_mount_rename_is_exdev() {
        let shared = Arc::new(InMemoryVfs::default());
        let vfs = mounted(&[("one", Arc::clone(&shared)), ("two", Arc::clone(&shared))]);
        let handle = vfs
            .open("/one/file", OpenMode::write_only().create_new())
            .expect("create mounted file");
        vfs.close(handle).expect("close file");

        assert_eq!(
            vfs.mkdir("/one").expect_err("mount exists").errno(),
            Errno::EEXIST
        );
        assert_eq!(
            vfs.rmdir("/one").expect_err("mount is busy").errno(),
            Errno::EBUSY
        );
        assert_eq!(
            vfs.rename("/one/file", "/two/file")
                .expect_err("cross-mount rename")
                .errno(),
            Errno::EXDEV
        );
        assert_eq!(
            vfs.mkdir("relative")
                .expect_err("relative path is invalid")
                .errno(),
            Errno::EINVAL
        );
        assert_eq!(
            vfs.rename("/one/file", "relative")
                .expect_err("relative destination is invalid")
                .errno(),
            Errno::EINVAL
        );
    }

    #[test]
    fn aggregate_stats_include_reporting_mounts() {
        let left = Arc::new(InMemoryVfs::new(VfsQuota::unlimited()));
        let right = Arc::new(InMemoryVfs::new(VfsQuota::unlimited()));
        left.mkdir("/dir").expect("seed left");
        right.mkdir("/dir").expect("seed right");
        let vfs = mounted(&[("left", left), ("right", right)]);
        let stats = vfs
            .stats()
            .expect("stats supported")
            .expect("stats succeed");
        assert_eq!(stats.file_count, 2);
        assert_eq!(stats.used_bytes, 0);
    }

    #[test]
    fn mount_names_are_single_non_reserved_components() {
        for invalid in ["", ".", "..", "bin", "a/b", "nul\0name"] {
            assert_eq!(
                validate_mount_name(invalid)
                    .expect_err("invalid mount name")
                    .errno(),
                Errno::EINVAL
            );
        }
        validate_mount_name("workspace").expect("ordinary name");
    }
}
