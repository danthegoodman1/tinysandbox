use tinysandbox::vfs::conformance;
use tinysandbox::vfs::{InMemoryVfs, VfsQuota};

#[test]
fn in_memory_vfs_satisfies_public_conformance_suite() {
    conformance::run(|quota: VfsQuota| InMemoryVfs::new(quota));
}

#[test]
fn in_memory_vfs_satisfies_snapshot_conformance_suite() {
    // Snapshot conformance is separate so third-party VFSes can opt into it only when supported.
    conformance::run_snapshots(|quota: VfsQuota| InMemoryVfs::new(quota));
}
