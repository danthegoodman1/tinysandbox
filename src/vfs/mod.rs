pub mod conformance;
pub mod mem;

mod path;

use std::error::Error;
use std::fmt;

pub use mem::{InMemoryVfs, VfsQuota, VfsStats};

pub type VfsResult<T> = Result<T, VfsError>;

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Errno {
    EBADF,
    EBUSY,
    EACCES,
    EEXIST,
    EINVAL,
    EISDIR,
    ENOENT,
    ENOSPC,
    ENOTDIR,
    ENOTEMPTY,
}

impl Errno {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfsError {
    errno: Errno,
}

impl VfsError {
    pub const fn new(errno: Errno) -> Self {
        Self { errno }
    }

    pub const fn errno(self) -> Errno {
        self.errno
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileHandle(u64);

impl FileHandle {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub file_type: FileType,
    pub len: u64,
}

impl Metadata {
    pub const fn is_file(self) -> bool {
        matches!(self.file_type, FileType::File)
    }

    pub const fn is_dir(self) -> bool {
        matches!(self.file_type, FileType::Directory)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub metadata: Metadata,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenMode {
    pub read: bool,
    pub write: bool,
    pub create: bool,
    pub create_new: bool,
    pub truncate: bool,
    pub append: bool,
}

impl OpenMode {
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

    pub const fn create(mut self) -> Self {
        self.create = true;
        self
    }

    pub const fn create_new(mut self) -> Self {
        self.create = true;
        self.create_new = true;
        self
    }

    pub const fn truncate(mut self) -> Self {
        self.truncate = true;
        self
    }

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

pub trait Vfs: Send + Sync {
    fn stat(&self, path: &str) -> VfsResult<Metadata>;

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>>;

    fn mkdir(&self, path: &str) -> VfsResult<()>;

    fn rename(&self, from: &str, to: &str) -> VfsResult<()>;

    fn unlink(&self, path: &str) -> VfsResult<()>;

    fn rmdir(&self, path: &str) -> VfsResult<()>;

    /// Opens a file handle.
    ///
    /// Implementations must return `EISDIR` for any attempt to open a
    /// directory, including read-only opens.
    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle>;

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize>;

    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize>;

    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()>;

    fn close(&self, handle: FileHandle) -> VfsResult<()>;

    fn is_fast(&self) -> bool {
        false
    }

    fn stats(&self) -> Option<VfsResult<VfsStats>> {
        None
    }
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
