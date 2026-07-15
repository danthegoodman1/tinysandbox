//! Containment, symlink, and persistence tests for the local-directory VFS.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tinysandbox::vfs::{Errno, LocalVfs, OpenMode, Vfs, VfsQuota, VfsResult, VfsStats};

/// Creates a unique empty directory under the system temp dir and returns it
/// alongside its parent scratch dir (useful for planting escape targets).
fn scratch_dirs(test: &str) -> (PathBuf, PathBuf) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = std::env::temp_dir().join(format!(
        "tinysandbox-local-{test}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let root = parent.join("root");
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&root).expect("create scratch root");
    (parent, root)
}

fn write_file(vfs: &LocalVfs, path: &str, data: &[u8]) -> VfsResult<()> {
    let handle = vfs.open(path, OpenMode::write_only().create_new())?;
    vfs.write_at(handle, 0, data)?;
    vfs.close(handle)
}

fn read_file(vfs: &LocalVfs, path: &str) -> VfsResult<Vec<u8>> {
    let len = vfs.stat(path)?.len as usize;
    let handle = vfs.open(path, OpenMode::read_only())?;
    let mut buf = vec![0; len];
    let read = vfs.read_at(handle, 0, &mut buf)?;
    vfs.close(handle)?;
    buf.truncate(read);
    Ok(buf)
}

fn assert_errno<T>(result: VfsResult<T>, errno: Errno) {
    match result {
        Ok(_) => panic!("expected {}, got Ok", errno.name()),
        Err(err) => assert_eq!(err.errno(), errno),
    }
}

#[test]
fn parent_traversal_is_clamped_at_the_root() {
    let (parent, root) = scratch_dirs("traversal");
    std::fs::write(parent.join("secret.txt"), b"top secret").expect("write secret");
    let vfs = LocalVfs::new(&root).expect("open local vfs");

    // "/../secret.txt" resolves to "/secret.txt" inside the root, not to the
    // sibling file next to it.
    assert_errno(vfs.stat("/../secret.txt"), Errno::ENOENT);
    assert_errno(vfs.stat("/../../../../secret.txt"), Errno::ENOENT);
    assert_errno(vfs.stat("relative"), Errno::EINVAL);
    assert_errno(vfs.stat("/\0"), Errno::EINVAL);

    write_file(&vfs, "/../escape.txt", b"contained").expect("write clamped path");
    assert!(root.join("escape.txt").is_file());
    assert!(!parent.join("escape.txt").exists());

    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn symlinks_are_invisible_and_never_followed() {
    let (parent, root) = scratch_dirs("symlink");
    std::fs::write(parent.join("secret.txt"), b"top secret").expect("write secret");
    std::os::unix::fs::symlink(parent.join("secret.txt"), root.join("link"))
        .expect("plant file symlink");
    std::os::unix::fs::symlink(&parent, root.join("linkdir")).expect("plant dir symlink");
    let vfs = LocalVfs::new(&root).expect("open local vfs");

    // Final-component symlink: invisible to stat/unlink, unopenable.
    assert_errno(vfs.stat("/link"), Errno::ENOENT);
    assert_errno(vfs.unlink("/link"), Errno::ENOENT);
    assert_errno(vfs.open("/link", OpenMode::read_only()), Errno::EACCES);
    assert_errno(
        vfs.open("/link", OpenMode::write_only().create()),
        Errno::EACCES,
    );

    // Intermediate-component symlink: the subtree behind it does not exist.
    assert_errno(vfs.stat("/linkdir/secret.txt"), Errno::ENOENT);
    assert_errno(
        vfs.open("/linkdir/new.txt", OpenMode::write_only().create()),
        Errno::ENOENT,
    );
    assert_errno(vfs.readdir("/linkdir"), Errno::ENOENT);

    // Both symlinks are hidden from directory listings.
    assert!(vfs.readdir("/").expect("readdir root").is_empty());

    // Renaming a symlink out or a file over one never follows the link.
    assert_errno(vfs.rename("/link", "/renamed"), Errno::ENOENT);
    write_file(&vfs, "/plain.txt", b"plain").expect("write plain file");
    vfs.rename("/plain.txt", "/link")
        .expect("rename over symlink replaces the link itself");

    let names: Vec<String> = vfs
        .readdir("/")
        .expect("readdir root")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["link"]);

    // The escape target is untouched throughout.
    let secret = std::fs::read(parent.join("secret.txt")).expect("read secret");
    assert_eq!(secret, b"top secret");
    assert_eq!(
        std::fs::read(root.join("link")).expect("replaced link is a regular file"),
        b"plain"
    );

    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn contents_persist_across_instances_and_seed_quota_usage() {
    let (parent, root) = scratch_dirs("persist");

    {
        let vfs = LocalVfs::new(&root).expect("open local vfs");
        vfs.mkdir("/dir").expect("mkdir");
        write_file(&vfs, "/dir/data.txt", b"hello disk").expect("write file");
    }

    assert_eq!(
        std::fs::read(root.join("dir/data.txt")).expect("file persisted"),
        b"hello disk"
    );

    let quota = VfsQuota {
        max_bytes: 16,
        max_files: 4,
        max_file_size: 16,
    };
    let vfs = LocalVfs::with_quota(&root, quota).expect("reopen local vfs");
    assert_eq!(
        read_file(&vfs, "/dir/data.txt").expect("read back"),
        b"hello disk"
    );

    // Usage is seeded from the existing tree: 10 bytes, 1 dir + 1 file.
    let stats = vfs.stats().expect("stats");
    assert_eq!(stats.used_bytes, 10);
    assert_eq!(stats.file_count, 2);

    // The seeded usage feeds quota enforcement for new growth.
    let handle = vfs
        .open("/dir/more.txt", OpenMode::write_only().create_new())
        .expect("open new file");
    assert_errno(vfs.write_at(handle, 0, b"12345678"), Errno::ENOSPC);
    vfs.write_at(handle, 0, b"123456")
        .expect("write within quota");
    vfs.close(handle).expect("close handle");

    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn refresh_and_pushed_usage_rebaseline_quota_accounting() {
    let (parent, root) = scratch_dirs("refresh");
    let quota = VfsQuota {
        max_bytes: 16,
        max_files: 8,
        max_file_size: 16,
    };
    let vfs = LocalVfs::with_quota(&root, quota).expect("open local vfs");
    write_file(&vfs, "/a.bin", b"aaaa").expect("write file");

    // External mutation is invisible until a refresh rescans the tree.
    std::fs::write(root.join("external.bin"), b"bbbbbb").expect("write external file");
    assert_eq!(vfs.stats().expect("stats").used_bytes, 4);
    let stats = vfs.refresh().expect("refresh");
    assert_eq!(stats.used_bytes, 10);
    assert_eq!(stats.file_count, 2);

    // Unlinked-but-open files keep their bytes and slot across a refresh.
    let held = vfs.open("/a.bin", OpenMode::read_write()).expect("open");
    vfs.unlink("/a.bin").expect("unlink open file");
    let stats = vfs.refresh().expect("refresh with unlinked-open file");
    assert_eq!(stats.used_bytes, 10);
    assert_eq!(stats.file_count, 2);
    vfs.close(held).expect("close releases unlinked storage");
    let stats = vfs.stats().expect("stats");
    assert_eq!(stats.used_bytes, 6);
    assert_eq!(stats.file_count, 1);

    // Pushed usage becomes the enforcement baseline.
    vfs.set_usage(VfsStats {
        used_bytes: 16,
        file_count: 1,
    });
    let handle = vfs
        .open("/b.bin", OpenMode::write_only().create_new())
        .expect("open new file");
    assert_errno(vfs.write_at(handle, 0, b"x"), Errno::ENOSPC);
    vfs.set_usage(VfsStats {
        used_bytes: 6,
        file_count: 2,
    });
    vfs.write_at(handle, 0, b"x")
        .expect("write fits the pushed baseline");
    vfs.close(handle).expect("close handle");
    assert_eq!(vfs.stats().expect("stats").used_bytes, 7);

    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn root_must_be_an_existing_directory() {
    let (parent, root) = scratch_dirs("badroot");

    assert!(LocalVfs::new(parent.join("missing")).is_err());

    let file = root.join("file.txt");
    std::fs::write(&file, b"x").expect("write file");
    assert!(LocalVfs::new(&file).is_err());

    let _ = std::fs::remove_dir_all(&parent);
}
