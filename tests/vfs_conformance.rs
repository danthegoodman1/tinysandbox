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
