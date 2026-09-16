use tinysandbox::vfs::conformance;
use tinysandbox::vfs::{InMemoryVfs, VfsQuota};

#[cfg(unix)]
#[test]
fn local_vfs_satisfies_public_conformance_suite() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let base = std::env::temp_dir().join(format!(
        "tinysandbox-local-conformance-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create conformance base dir");

    let counter = AtomicU64::new(0);
    conformance::run(|quota: VfsQuota| {
        let root = base.join(counter.fetch_add(1, Ordering::Relaxed).to_string());
        std::fs::create_dir(&root).expect("create conformance root");
        tinysandbox::vfs::LocalVfs::with_quota(&root, quota).expect("open local vfs")
    });

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn in_memory_vfs_satisfies_public_conformance_suite() {
    conformance::run(|quota: VfsQuota| InMemoryVfs::new(quota));
}

#[test]
fn in_memory_vfs_satisfies_snapshot_conformance_suite() {
    // Snapshot conformance is separate so third-party VFSes can opt into it only when supported.
    conformance::run_snapshots(|quota: VfsQuota| InMemoryVfs::new(quota));
}

fn rename_depth_boundary(vfs: &impl tinysandbox::vfs::Vfs) {
    use tinysandbox::vfs::{Errno, OpenMode};
    vfs.mkdir("/source").unwrap();
    vfs.mkdir("/source/child").unwrap();
    let handle = vfs
        .open("/source/child/leaf", OpenMode::read_write().create())
        .unwrap();
    vfs.write_at(handle, 0, b"retained").unwrap();
    let mut parent = String::new();
    for _ in 0..253 {
        parent.push_str("/d");
        vfs.mkdir(&parent).unwrap();
    }
    // Endpoints fit; only the resulting leaf would exceed the ceiling.
    let target = format!("{parent}/target");
    vfs.mkdir(&target).unwrap();
    let stats = vfs.stats();
    assert_eq!(
        vfs.rename("/source", &format!("{target}/source"))
            .unwrap_err()
            .errno(),
        Errno::EINVAL
    );
    assert_eq!(vfs.stats(), stats);
    assert!(vfs.readdir(&target).unwrap().is_empty());
    assert_eq!(vfs.stat("/source/child/leaf").unwrap().len, 8);
    vfs.rename("/source", &target)
        .expect("leaf at exactly depth 256");
    assert_eq!(vfs.stat(&format!("{target}/child/leaf")).unwrap().len, 8);
    let mut bytes = [0; 8];
    vfs.read_at(handle, 0, &mut bytes).unwrap();
    assert_eq!(&bytes, b"retained");
    vfs.close(handle).unwrap();
    vfs.rename(&target, "/source")
        .expect("moving back up remains valid");
}

#[test]
fn memory_rename_preserves_depth_and_open_handle_identity() {
    use tinysandbox::vfs::{Vfs, VfsSnapshot};
    let vfs = InMemoryVfs::new(VfsQuota::unlimited());
    rename_depth_boundary(&vfs);
    let snapshot = vfs.snapshot().unwrap();
    let branch = vfs.branch(&snapshot).unwrap();
    vfs.restore(&snapshot).unwrap();
    assert_eq!(branch.stat("/source/child/leaf").unwrap().len, 8);
}

#[cfg(unix)]
#[test]
fn local_rename_preserves_depth_and_open_handle_identity() {
    let root = std::env::temp_dir().join(format!(
        "tinysandbox-rename-boundary-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let vfs = tinysandbox::vfs::LocalVfs::new(&root).unwrap();
    rename_depth_boundary(&vfs);
    drop(vfs);
    std::fs::remove_dir_all(root).unwrap();
}
