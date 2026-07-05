//! Virtual filesystem traits and the in-memory implementation.

pub mod conformance;
pub mod mem;

mod path;

use std::error::Error;
use std::fmt;

pub use mem::{InMemoryVfs, InMemoryVfsSnapshot, VfsQuota, VfsStats};

/// Result type returned by VFS operations.
pub type VfsResult<T> = Result<T, VfsError>;

#[allow(non_camel_case_types)]
/// POSIX-like errno values surfaced by the VFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Errno {
    /// Bad file descriptor.
    EBADF,
    /// Device or resource busy.
    EBUSY,
    /// Permission denied.
    EACCES,
    /// File exists.
    EEXIST,
    /// Invalid argument.
    EINVAL,
    /// Is a directory.
    EISDIR,
    /// No such file or directory.
    ENOENT,
    /// No space left on device.
    ENOSPC,
    /// Not a directory.
    ENOTDIR,
    /// Directory not empty.
    ENOTEMPTY,
}

impl Errno {
    /// Returns the Linux numeric errno value.
    pub const fn code(self) -> i32 {
        match self {
            Self::EBADF => 9,
            Self::EBUSY => 16,
            Self::EACCES => 13,
            Self::EEXIST => 17,
            Self::EINVAL => 22,
            Self::EISDIR => 21,
            Self::ENOENT => 2,
            Self::ENOSPC => 28,
            Self::ENOTDIR => 20,
            Self::ENOTEMPTY => 39,
        }
    }

    /// Returns the symbolic errno name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::EBADF => "EBADF",
            Self::EBUSY => "EBUSY",
            Self::EACCES => "EACCES",
            Self::EEXIST => "EEXIST",
            Self::EINVAL => "EINVAL",
            Self::EISDIR => "EISDIR",
            Self::ENOENT => "ENOENT",
            Self::ENOSPC => "ENOSPC",
            Self::ENOTDIR => "ENOTDIR",
            Self::ENOTEMPTY => "ENOTEMPTY",
        }
    }
}

/// Error returned by VFS operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfsError {
    errno: Errno,
}

impl VfsError {
    /// Creates a VFS error from an errno.
    pub const fn new(errno: Errno) -> Self {
        Self { errno }
    }

    /// Returns the underlying errno.
    pub const fn errno(self) -> Errno {
        self.errno
    }

    /// Returns the Linux numeric errno value.
    pub const fn code(self) -> i32 {
        self.errno.code()
    }
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.errno.name())
    }
}

impl Error for VfsError {}

/// Opaque file handle minted by a VFS implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileHandle(u64);

impl FileHandle {
    /// Creates a handle from a raw implementation-defined value.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw implementation-defined value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// File kind returned by metadata and directory entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// Regular file.
    File,
    /// Directory.
    Directory,
}

/// Metadata for a file or directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    /// File kind.
    pub file_type: FileType,
    /// File length in bytes, or zero for directories.
    pub len: u64,
}

impl Metadata {
    /// Returns true for regular files.
    pub const fn is_file(self) -> bool {
        matches!(self.file_type, FileType::File)
    }

    /// Returns true for directories.
    pub const fn is_dir(self) -> bool {
        matches!(self.file_type, FileType::Directory)
    }
}

/// Directory entry returned by [`Vfs::readdir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Entry name without parent path components.
    pub name: String,
    /// Entry metadata.
    pub metadata: Metadata,
}

/// File open flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenMode {
    /// Allows reads from the handle.
    pub read: bool,
    /// Allows writes through the handle.
    pub write: bool,
    /// Creates the file if it does not exist.
    pub create: bool,
    /// Requires creating a new file and fails if it exists.
    pub create_new: bool,
    /// Truncates an existing file on open.
    pub truncate: bool,
    /// Appends every write to the current end of file.
    pub append: bool,
}

impl OpenMode {
    /// Opens an existing file for reading.
    pub const fn read_only() -> Self {
        Self {
            read: true,
            write: false,
            create: false,
            create_new: false,
            truncate: false,
            append: false,
        }
    }

    /// Opens an existing file for writing.
    pub const fn write_only() -> Self {
        Self {
            read: false,
            write: true,
            create: false,
            create_new: false,
            truncate: false,
            append: false,
        }
    }

    /// Opens an existing file for reading and writing.
    pub const fn read_write() -> Self {
        Self {
            read: true,
            write: true,
            create: false,
            create_new: false,
            truncate: false,
            append: false,
        }
    }

    /// Sets the create-if-missing flag.
    pub const fn create(mut self) -> Self {
        self.create = true;
        self
    }

    /// Sets create-if-missing and create-new flags.
    pub const fn create_new(mut self) -> Self {
        self.create = true;
        self.create_new = true;
        self
    }

    /// Sets the truncate-on-open flag.
    pub const fn truncate(mut self) -> Self {
        self.truncate = true;
        self
    }

    /// Sets append mode and enables writes.
    pub const fn append(mut self) -> Self {
        self.append = true;
        self.write = true;
        self
    }

    pub(crate) const fn validate(self) -> VfsResult<()> {
        if !self.read && !self.write {
            return Err(VfsError::new(Errno::EINVAL));
        }

        if self.truncate && !self.write {
            return Err(VfsError::new(Errno::EINVAL));
        }

        if self.append && !self.write {
            return Err(VfsError::new(Errno::EINVAL));
        }

        Ok(())
    }
}

/// Synchronous, handle-and-offset virtual filesystem interface.
pub trait Vfs: Send + Sync {
    /// Returns metadata for an absolute path.
    fn stat(&self, path: &str) -> VfsResult<Metadata>;

    /// Lists immediate children of a directory path.
    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>>;

    /// Creates an empty directory.
    fn mkdir(&self, path: &str) -> VfsResult<()>;

    /// Renames a file or directory.
    fn rename(&self, from: &str, to: &str) -> VfsResult<()>;

    /// Removes a file path.
    fn unlink(&self, path: &str) -> VfsResult<()>;

    /// Removes an empty directory path.
    fn rmdir(&self, path: &str) -> VfsResult<()>;

    /// Opens a file handle.
    ///
    /// Implementations must return `EISDIR` for any attempt to open a
    /// directory, including read-only opens.
    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle>;

    /// Reads from a handle at `offset` into `buf`.
    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize>;

    /// Writes `data` through a handle at `offset`.
    ///
    /// Empty writes to a valid writable handle succeed and return zero at any
    /// offset. Non-empty writes whose `offset + data.len()` overflows the
    /// implementation's addressable range must fail with `EINVAL`; writes that
    /// fit the address range but exceed quota must fail with `ENOSPC`.
    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize>;

    /// Changes the length of a file opened for writing.
    ///
    /// Lengths that cannot fit in the implementation or exceed byte/file-size
    /// quotas fail with `ENOSPC`; for example, the public conformance suite pins
    /// `truncate(handle, u64::MAX)` to `ENOSPC`.
    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()>;

    /// Closes a file handle.
    fn close(&self, handle: FileHandle) -> VfsResult<()>;

    /// Returns true when operations are cheap enough to run inline.
    fn is_fast(&self) -> bool {
        false
    }

    /// Returns quota usage when the implementation can report it.
    fn stats(&self) -> Option<VfsResult<VfsStats>> {
        None
    }
}

/// Optional snapshot support for VFS implementations.
///
/// A snapshot captures the path-visible filesystem state, not live file
/// handles. Restoring a snapshot replaces the current filesystem contents and
/// invalidates handles that were open before the restore. Branches start with
/// the snapshot contents and no open handles.
pub trait VfsSnapshot: Vfs + Sized {
    /// Opaque snapshot value for this implementation.
    type Snapshot: Clone + Send + Sync + 'static;

    /// Captures the current path-visible filesystem state.
    fn snapshot(&self) -> VfsResult<Self::Snapshot>;

    /// Replaces this VFS contents with `snapshot`.
    fn restore(&self, snapshot: &Self::Snapshot) -> VfsResult<()>;

    /// Creates an independent VFS seeded from `snapshot`.
    fn branch(&self, snapshot: &Self::Snapshot) -> VfsResult<Self>;
}

#[cfg(test)]
mod tests {
    use super::{Errno, VfsError};

    #[test]
    fn errno_values_match_linux_numbers() {
        assert_eq!(Errno::ENOENT.code(), 2);
        assert_eq!(Errno::EACCES.code(), 13);
        assert_eq!(Errno::EBUSY.code(), 16);
        assert_eq!(Errno::ENOSPC.code(), 28);
        assert_eq!(Errno::ENOTEMPTY.code(), 39);
        assert_eq!(VfsError::new(Errno::EBADF).code(), 9);
    }
}
