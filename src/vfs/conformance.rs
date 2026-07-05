//! Public conformance suites for third-party VFS implementations.

use std::collections::BTreeSet;

use super::{DirEntry, Errno, FileType, OpenMode, Vfs, VfsQuota, VfsResult, VfsSnapshot};

/// Runs the public VFS conformance suite against a VFS implementation.
///
/// The factory receives the quota configuration required by each test case.
/// Implementations under test must enforce `max_bytes`, `max_files`, and
/// `max_file_size`; the suite has no quota skip mode.
pub fn run<V, F>(factory: F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    open_modes_and_errors(&factory);
    append_mode_appends(&factory);
    offsets_and_overwrite_semantics(&factory);
    cross_handle_visibility(&factory);
    rename_semantics(&factory);
    removal_semantics(&factory);
    quota_accounting_and_reuse(&factory);
    path_normalization_and_containment(&factory);
    closed_handle_errors(&factory);
    truncate_grows_and_shrinks(&factory);
    offset_and_length_boundaries(&factory);
    readdir_semantics(&factory);
    mkdir_errors(&factory);
    open_handle_identity_semantics(&factory);
    directories_count_against_file_quota(&factory);
}

/// Runs the snapshot extension of the public VFS conformance suite.
///
/// Implementations that support [`VfsSnapshot`] should run this in addition to
/// [`run`]. Snapshot tests require `Vfs::stats` to return quota usage because
/// quota accounting across restore and branch is part of the snapshot contract.
pub fn run_snapshots<V, F>(factory: F)
where
    V: VfsSnapshot,
    F: Fn(VfsQuota) -> V,
{
    mutate_after_snapshot_is_isolated(&factory);
    restore_recreates_the_snapshot_tree(&factory);
    branches_are_independent(&factory);
    snapshot_quota_accounting(&factory);
}

fn open_modes_and_errors<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    assert_errno(vfs.open("/missing", OpenMode::read_only()), Errno::ENOENT);
    assert_errno(vfs.open("/invalid", OpenMode::default()), Errno::EINVAL);

    let write_only = vfs
        .open("/file", OpenMode::write_only().create_new())
        .expect("create_new should create a missing file");
    assert_errno(
        vfs.open("/file", OpenMode::write_only().create_new()),
        Errno::EEXIST,
    );
    let plain_create = vfs
        .open("/file", OpenMode::write_only().create())
        .expect("plain create should open an existing file");
    vfs.close(plain_create).expect("close plain create");

    let mut buf = [0; 1];
    assert_errno(vfs.read_at(write_only, 0, &mut buf), Errno::EBADF);
    vfs.write_at(write_only, 0, b"abc")
        .expect("write-only handle should accept writes");
    vfs.close(write_only).expect("close succeeds");

    let read_only = vfs
        .open("/file", OpenMode::read_only())
        .expect("read-only open should find file");
    assert_errno(vfs.write_at(read_only, 0, b"x"), Errno::EBADF);
    assert_errno(vfs.truncate(read_only, 0), Errno::EINVAL);
    vfs.close(read_only).expect("close succeeds");

    let truncated = vfs
        .open("/file", OpenMode::write_only().truncate())
        .expect("truncate open should find file");
    vfs.close(truncated).expect("close succeeds");
    assert_eq!(vfs.stat("/file").expect("stat succeeds").len, 0);

    vfs.mkdir("/dir").expect("mkdir succeeds");
    assert_errno(vfs.open("/dir", OpenMode::read_only()), Errno::EISDIR);
    assert_errno(
        vfs.open("/file/child", OpenMode::read_only()),
        Errno::ENOTDIR,
    );
    assert_errno(
        vfs.open("/missing/file", OpenMode::write_only().create()),
        Errno::ENOENT,
    );
}

fn append_mode_appends<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    write_file(&vfs, "/log", b"first").expect("write initial contents");

    let append = vfs
        .open("/log", OpenMode::write_only().append())
        .expect("append open succeeds");
    vfs.write_at(append, 0, b"-second")
        .expect("append write succeeds");
    vfs.close(append).expect("close append handle");

    assert_contents(&vfs, "/log", b"first-second");
}

fn offsets_and_overwrite_semantics<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    let handle = vfs
        .open("/sparse", OpenMode::read_write().create_new())
        .expect("file opens");

    assert_eq!(vfs.write_at(handle, 4, b"x").expect("write succeeds"), 1);
    assert_eq!(vfs.stat("/sparse").expect("stat succeeds").len, 5);

    let mut buf = [9; 8];
    assert_eq!(vfs.read_at(handle, 10, &mut buf).expect("past EOF read"), 0);
    let read = vfs.read_at(handle, 0, &mut buf).expect("read succeeds");
    assert_eq!(read, 5);
    assert_eq!(&buf[..5], &[0, 0, 0, 0, b'x']);

    write_file(&vfs, "/overwrite", b"hello world").expect("write longer file");
    let overwrite = vfs
        .open("/overwrite", OpenMode::read_write())
        .expect("open for overwrite");
    vfs.write_at(overwrite, 0, b"X")
        .expect("prefix overwrite succeeds");
    vfs.close(overwrite).expect("close overwrite handle");
    assert_contents(&vfs, "/overwrite", b"Xello world");
}

fn cross_handle_visibility<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    let writer = vfs
        .open("/shared", OpenMode::read_write().create_new())
        .expect("create shared file");
    let reader = vfs
        .open("/shared", OpenMode::read_only())
        .expect("open shared file through second handle");

    vfs.write_at(writer, 0, b"abc")
        .expect("write through first handle");
    let mut buf = [0; 8];
    let read = vfs
        .read_at(reader, 0, &mut buf)
        .expect("second handle sees write");
    assert_eq!(&buf[..read], b"abc");

    vfs.truncate(writer, 1)
        .expect("truncate through first handle");
    assert_eq!(vfs.stat("/shared").expect("stat shared file").len, 1);
    let read = vfs
        .read_at(reader, 0, &mut buf)
        .expect("second handle sees truncate");
    assert_eq!(&buf[..read], b"a");

    vfs.close(reader).expect("close reader handle");
    vfs.close(writer).expect("close writer handle");
}

fn rename_semantics<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    write_file(&vfs, "/a", b"replacement").expect("write source");
    write_file(&vfs, "/b", b"old").expect("write target");

    vfs.rename("/a", "/b").expect("rename over file succeeds");
    assert_errno(vfs.stat("/a"), Errno::ENOENT);
    assert_contents(&vfs, "/b", b"replacement");

    assert_errno(vfs.rename("/missing", "/target"), Errno::ENOENT);
    assert_errno(vfs.rename("/also-missing", "/also-missing"), Errno::ENOENT);

    vfs.mkdir("/dir").expect("mkdir dir");
    write_file(&vfs, "/file", b"x").expect("write file");
    assert_errno(vfs.rename("/file", "/dir"), Errno::EISDIR);
    assert_errno(vfs.rename("/dir", "/file"), Errno::ENOTDIR);

    vfs.mkdir("/empty-source").expect("mkdir empty source");
    vfs.mkdir("/empty-target").expect("mkdir empty target");
    vfs.rename("/empty-source", "/empty-target")
        .expect("directory can replace empty directory");
    assert_errno(vfs.stat("/empty-source"), Errno::ENOENT);
    assert!(vfs.stat("/empty-target").expect("stat target").is_dir());

    vfs.mkdir("/source-dir").expect("mkdir source dir");
    vfs.mkdir("/non-empty-target").expect("mkdir target dir");
    write_file(&vfs, "/non-empty-target/child", b"x").expect("write child");
    assert_errno(
        vfs.rename("/source-dir", "/non-empty-target"),
        Errno::ENOTEMPTY,
    );

    vfs.mkdir("/parent").expect("mkdir parent");
    vfs.mkdir("/parent/child").expect("mkdir child");
    assert_errno(vfs.rename("/parent", "/parent/child/moved"), Errno::EINVAL);
}

fn removal_semantics<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    vfs.mkdir("/dir").expect("mkdir succeeds");
    write_file(&vfs, "/dir/file", b"x").expect("write child");

    assert_errno(vfs.rmdir("/dir"), Errno::ENOTEMPTY);
    assert_errno(vfs.unlink("/dir"), Errno::EISDIR);
    vfs.unlink("/dir/file").expect("unlink child succeeds");
    vfs.rmdir("/dir").expect("empty directory removes");
    assert_errno(vfs.stat("/dir"), Errno::ENOENT);

    write_file(&vfs, "/file", b"x").expect("write file");
    assert_errno(vfs.rmdir("/file"), Errno::ENOTDIR);
    assert_errno(vfs.rmdir("/missing"), Errno::ENOENT);
    assert_errno(vfs.rmdir("/"), Errno::EBUSY);
}

fn quota_accounting_and_reuse<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(VfsQuota {
        max_bytes: 4,
        max_files: 8,
        max_file_size: 4,
    });
    write_file(&vfs, "/file", b"1234").expect("write exact quota");
    let handle = vfs
        .open("/file", OpenMode::write_only())
        .expect("open file");
    assert_errno(vfs.write_at(handle, 0, b"12345"), Errno::ENOSPC);
    vfs.close(handle).expect("close handle");
    assert_contents(&vfs, "/file", b"1234");

    let vfs = factory(VfsQuota {
        max_bytes: 4,
        max_files: 8,
        max_file_size: 4,
    });
    write_file(&vfs, "/a", b"1234").expect("write first file");
    let b = vfs
        .open("/b", OpenMode::write_only().create_new())
        .expect("empty file fits");
    assert_errno(vfs.write_at(b, 0, b"x"), Errno::ENOSPC);
    vfs.unlink("/a").expect("unlink frees bytes");
    vfs.write_at(b, 0, b"x").expect("freed bytes are reusable");
    vfs.close(b).expect("close b");

    let vfs = factory(VfsQuota {
        max_bytes: 4,
        max_files: 8,
        max_file_size: 4,
    });
    write_file(&vfs, "/a", b"1234").expect("write first file");
    let a = vfs.open("/a", OpenMode::write_only()).expect("open file");
    vfs.truncate(a, 1).expect("truncate frees bytes");
    vfs.close(a).expect("close a");
    write_file(&vfs, "/b", b"234").expect("freed truncate bytes are reusable");

    let vfs = factory(VfsQuota {
        max_bytes: 8,
        max_files: 8,
        max_file_size: 4,
    });
    write_file(&vfs, "/old", b"1234").expect("write old file");
    write_file(&vfs, "/new", b"abcd").expect("write new file");
    vfs.rename("/new", "/old")
        .expect("rename-over frees replaced file");
    write_file(&vfs, "/extra", b"wxyz").expect("freed rename bytes are reusable");
}

fn path_normalization_and_containment<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    vfs.mkdir("/workspace").expect("mkdir succeeds");
    write_file(&vfs, "/workspace//./file", b"x").expect("normalized path writes");
    assert!(
        vfs.stat("/workspace/file")
            .expect("stat normalized")
            .is_file()
    );

    write_file(&vfs, "/../../contained", b"x").expect("root-contained path writes");
    assert!(vfs.stat("/contained").expect("stat contained").is_file());
    assert_errno(vfs.stat("relative"), Errno::EINVAL);
}

fn closed_handle_errors<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    let handle = vfs
        .open("/file", OpenMode::read_write().create_new())
        .expect("file opens");
    vfs.close(handle).expect("first close succeeds");

    let mut buf = [0; 1];
    assert_errno(vfs.read_at(handle, 0, &mut buf), Errno::EBADF);
    assert_errno(vfs.write_at(handle, 0, b"x"), Errno::EBADF);
    assert_errno(vfs.truncate(handle, 0), Errno::EBADF);
    assert_errno(vfs.close(handle), Errno::EBADF);
}

fn truncate_grows_and_shrinks<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    let handle = vfs
        .open("/file", OpenMode::read_write().create_new())
        .expect("file opens");
    vfs.write_at(handle, 0, b"abc").expect("write succeeds");
    vfs.truncate(handle, 5).expect("truncate grows file");

    let mut buf = [9; 8];
    let read = vfs.read_at(handle, 0, &mut buf).expect("read grown file");
    assert_eq!(read, 5);
    assert_eq!(&buf[..5], b"abc\0\0");

    vfs.truncate(handle, 2).expect("truncate shrinks file");
    assert_eq!(vfs.stat("/file").expect("stat succeeds").len, 2);
    let read = vfs.read_at(handle, 0, &mut buf).expect("read shrunk file");
    assert_eq!(read, 2);
    assert_eq!(&buf[..2], b"ab");
}

fn offset_and_length_boundaries<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(VfsQuota {
        max_bytes: 8,
        max_files: 4,
        max_file_size: 8,
    });
    let handle = vfs
        .open("/file", OpenMode::read_write().create_new())
        .expect("file opens");

    assert_eq!(
        vfs.write_at(handle, u64::MAX, b"")
            .expect("empty writes do not consult offset growth"),
        0
    );
    assert_errno(vfs.write_at(handle, u64::MAX, b"x"), Errno::EINVAL);
    assert_errno(vfs.truncate(handle, u64::MAX), Errno::ENOSPC);

    vfs.write_at(handle, 0, b"12345678")
        .expect("exact byte quota write succeeds");
    assert_errno(vfs.write_at(handle, 8, b"x"), Errno::ENOSPC);
    vfs.truncate(handle, 0).expect("truncate to zero succeeds");
    assert_eq!(vfs.stat("/file").expect("zero-length stat").len, 0);
    vfs.close(handle).expect("close handle");
}

fn readdir_semantics<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    vfs.mkdir("/dir").expect("mkdir dir");
    write_file(&vfs, "/file", b"x").expect("write file");

    let entries = vfs.readdir("/").expect("read root directory");
    assert_entry(&entries, "dir", FileType::Directory);
    assert_entry(&entries, "file", FileType::File);

    vfs.unlink("/file").expect("unlink file");
    vfs.mkdir("/new").expect("mkdir new dir");
    let names: BTreeSet<_> = vfs
        .readdir("/")
        .expect("read mutated root")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, BTreeSet::from(["dir".to_owned(), "new".to_owned()]));

    assert_errno(vfs.readdir("/dir/missing"), Errno::ENOENT);
    write_file(&vfs, "/dir/file", b"x").expect("write nested file");
    assert_errno(vfs.readdir("/dir/file"), Errno::ENOTDIR);
}

fn mkdir_errors<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    vfs.mkdir("/dir").expect("mkdir succeeds");
    assert_errno(vfs.mkdir("/dir"), Errno::EEXIST);
    assert_errno(vfs.mkdir("/missing/child"), Errno::ENOENT);
    write_file(&vfs, "/file", b"x").expect("write file");
    assert_errno(vfs.mkdir("/file/child"), Errno::ENOTDIR);
}

fn open_handle_identity_semantics<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(VfsQuota {
        max_bytes: 4,
        max_files: 4,
        max_file_size: 4,
    });
    let held = vfs
        .open("/held", OpenMode::read_write().create_new())
        .expect("open held file");
    vfs.write_at(held, 0, b"1234").expect("fill quota");
    vfs.unlink("/held").expect("unlink open file");
    assert_errno(vfs.stat("/held"), Errno::ENOENT);

    let mut buf = [0; 4];
    assert_eq!(vfs.read_at(held, 0, &mut buf).expect("read unlinked"), 4);
    assert_eq!(&buf, b"1234");

    let next = vfs
        .open("/next", OpenMode::write_only().create_new())
        .expect("new empty file still fits file-count quota");
    assert_errno(vfs.write_at(next, 0, b"x"), Errno::ENOSPC);
    vfs.close(held).expect("close releases unlinked storage");
    vfs.write_at(next, 0, b"x")
        .expect("quota releases after last close");
    vfs.close(next).expect("close next");

    let vfs = factory(generous_quota());
    write_file(&vfs, "/path", b"old").expect("write old path");
    let old = vfs.open("/path", OpenMode::read_only()).expect("open old");
    write_file(&vfs, "/replacement", b"new").expect("write replacement");
    vfs.rename("/replacement", "/path")
        .expect("rename over open path");
    assert_contents(&vfs, "/path", b"new");

    let mut buf = [0; 8];
    let read = vfs.read_at(old, 0, &mut buf).expect("read old handle");
    assert_eq!(&buf[..read], b"old");
    vfs.close(old).expect("close old handle");
}

fn directories_count_against_file_quota<V, F>(factory: &F)
where
    V: Vfs,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(VfsQuota {
        max_bytes: 16,
        max_files: 1,
        max_file_size: 16,
    });
    vfs.mkdir("/dir").expect("directory consumes non-root slot");
    assert_errno(vfs.mkdir("/other"), Errno::ENOSPC);
    assert_errno(
        vfs.open("/file", OpenMode::write_only().create_new()),
        Errno::ENOSPC,
    );

    vfs.rmdir("/dir").expect("directory slot is reusable");
    vfs.open("/file", OpenMode::write_only().create_new())
        .expect("freed directory slot can hold a file");
}

fn mutate_after_snapshot_is_isolated<V, F>(factory: &F)
where
    V: VfsSnapshot,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    write_file(&vfs, "/file", b"before").expect("write original");
    let snapshot = vfs.snapshot().expect("snapshot succeeds");

    let handle = vfs
        .open("/file", OpenMode::write_only())
        .expect("open file");
    vfs.write_at(handle, 0, b"after")
        .expect("overwrite original");
    vfs.close(handle).expect("close write handle");
    vfs.unlink("/file").expect("remove current file");

    let branch = vfs.branch(&snapshot).expect("branch from snapshot");
    assert_contents(&branch, "/file", b"before");

    vfs.restore(&snapshot).expect("restore snapshot");
    assert_contents(&vfs, "/file", b"before");
}

fn restore_recreates_the_snapshot_tree<V, F>(factory: &F)
where
    V: VfsSnapshot,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    vfs.mkdir("/dir").expect("mkdir dir");
    write_file(&vfs, "/dir/a", b"alpha").expect("write nested file");
    write_file(&vfs, "/root", b"root").expect("write root file");
    let held = vfs.open("/root", OpenMode::read_only()).expect("open root");
    let snapshot = vfs.snapshot().expect("snapshot succeeds");

    vfs.rename("/dir/a", "/renamed")
        .expect("rename after snapshot");
    vfs.unlink("/root").expect("unlink after snapshot");
    vfs.restore(&snapshot).expect("restore snapshot");

    assert_contents(&vfs, "/dir/a", b"alpha");
    assert_contents(&vfs, "/root", b"root");
    assert_errno(vfs.stat("/renamed"), Errno::ENOENT);

    let mut buf = [0; 1];
    assert_errno(vfs.read_at(held, 0, &mut buf), Errno::EBADF);
}

fn branches_are_independent<V, F>(factory: &F)
where
    V: VfsSnapshot,
    F: Fn(VfsQuota) -> V,
{
    let vfs = factory(generous_quota());
    write_file(&vfs, "/file", b"base").expect("write base");
    let snapshot = vfs.snapshot().expect("snapshot succeeds");
    let branch = vfs.branch(&snapshot).expect("branch succeeds");

    let source_handle = vfs
        .open("/file", OpenMode::write_only())
        .expect("open source");
    vfs.write_at(source_handle, 0, b"src!")
        .expect("source write succeeds");
    vfs.close(source_handle).expect("close source");

    let branch_handle = branch
        .open("/file", OpenMode::write_only())
        .expect("open branch");
    branch
        .write_at(branch_handle, 0, b"br")
        .expect("branch write succeeds");
    branch.close(branch_handle).expect("close branch");

    assert_contents(&vfs, "/file", b"src!");
    assert_contents(&branch, "/file", b"brse");
}

fn snapshot_quota_accounting<V, F>(factory: &F)
where
    V: VfsSnapshot,
    F: Fn(VfsQuota) -> V,
{
    let quota = VfsQuota {
        max_bytes: 4,
        max_files: 2,
        max_file_size: 4,
    };
    let vfs = factory(quota);
    vfs.mkdir("/dir")
        .expect("directory consumes exact file quota");
    write_file(&vfs, "/file", b"1234").expect("write exact byte quota");
    assert_stats(&vfs, 4, 2);
    let snapshot = vfs.snapshot().expect("snapshot succeeds at quota");

    vfs.unlink("/file").expect("free bytes after snapshot");
    assert_stats(&vfs, 0, 1);
    vfs.restore(&snapshot)
        .expect("restore exact-quota snapshot");
    assert_stats(&vfs, 4, 2);
    assert_contents(&vfs, "/file", b"1234");

    let branch = vfs.branch(&snapshot).expect("branch exact-quota snapshot");
    assert_stats(&branch, 4, 2);
    let handle = branch
        .open("/file", OpenMode::write_only())
        .expect("open branch file");
    assert_errno(branch.write_at(handle, 0, b"12345"), Errno::ENOSPC);
    branch.close(handle).expect("close branch handle");

    let too_small = factory(VfsQuota {
        max_bytes: 3,
        max_files: 2,
        max_file_size: 4,
    });
    assert_errno(too_small.restore(&snapshot), Errno::ENOSPC);
    assert_stats(&too_small, 0, 0);
}

fn generous_quota() -> VfsQuota {
    VfsQuota {
        max_bytes: 1024,
        max_files: 64,
        max_file_size: 1024,
    }
}

fn write_file<V: Vfs>(vfs: &V, path: &str, data: &[u8]) -> VfsResult<()> {
    let handle = vfs.open(path, OpenMode::write_only().create_new())?;
    vfs.write_at(handle, 0, data)?;
    vfs.close(handle)
}

fn assert_contents<V: Vfs>(vfs: &V, path: &str, expected: &[u8]) {
    let handle = vfs
        .open(path, OpenMode::read_only())
        .expect("open for read");
    let mut buf = vec![0; expected.len() + 8];
    let read = vfs.read_at(handle, 0, &mut buf).expect("read contents");
    vfs.close(handle).expect("close read handle");
    assert_eq!(read, expected.len());
    assert_eq!(&buf[..read], expected);
}

fn assert_stats<V: Vfs>(vfs: &V, used_bytes: u64, file_count: u64) {
    let stats = vfs
        .stats()
        .expect("snapshot conformance requires Vfs::stats")
        .expect("stats succeeds");
    assert_eq!(stats.used_bytes, used_bytes);
    assert_eq!(stats.file_count, file_count);
}

fn assert_entry(entries: &[DirEntry], name: &str, file_type: FileType) {
    let entry = entries
        .iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("missing directory entry {name}"));
    assert_eq!(entry.metadata.file_type, file_type);
}

fn assert_errno<T>(result: VfsResult<T>, errno: Errno) {
    match result {
        Ok(_) => panic!("expected {}, got Ok", errno.name()),
        Err(err) if err.errno() == errno => {}
        Err(err) => panic!("expected {}, got {}", errno.name(), err),
    }
}

#[cfg(test)]
mod tests {
    use crate::vfs::VfsError;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn vfs_error_is_send_sync() {
        assert_send_sync::<VfsError>();
    }
}
