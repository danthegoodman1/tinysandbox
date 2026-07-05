use tinysandbox::vfs::{DirEntry, Errno, FileHandle, Metadata, OpenMode, Vfs, VfsError, VfsResult};

struct ExternalStyleVfs;

impl Vfs for ExternalStyleVfs {
    fn stat(&self, _path: &str) -> VfsResult<Metadata> {
        Err(VfsError::new(Errno::ENOENT))
    }

    fn readdir(&self, _path: &str) -> VfsResult<Vec<DirEntry>> {
        Ok(Vec::new())
    }

    fn mkdir(&self, _path: &str) -> VfsResult<()> {
        Ok(())
    }

    fn rename(&self, _from: &str, _to: &str) -> VfsResult<()> {
        Ok(())
    }

    fn unlink(&self, _path: &str) -> VfsResult<()> {
        Ok(())
    }

    fn rmdir(&self, _path: &str) -> VfsResult<()> {
        Ok(())
    }

    fn open(&self, _path: &str, _mode: OpenMode) -> VfsResult<FileHandle> {
        Ok(FileHandle::new(0))
    }

    fn read_at(&self, _handle: FileHandle, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, _handle: FileHandle, _offset: u64, data: &[u8]) -> VfsResult<usize> {
        Ok(data.len())
    }

    fn truncate(&self, _handle: FileHandle, _len: u64) -> VfsResult<()> {
        Ok(())
    }

    fn close(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }
}

#[test]
fn external_vfs_impl_can_mint_handles() {
    // Integration tests compile as an external crate, so this catches regressions in third-party implementability.
    let vfs = ExternalStyleVfs;
    let handle = vfs
        .open("/anything", OpenMode::read_only())
        .expect("stub open succeeds");

    assert_eq!(handle.raw(), 0);
    vfs.close(handle).expect("stub close succeeds");
}
