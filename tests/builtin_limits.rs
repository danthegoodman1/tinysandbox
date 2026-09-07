use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tinysandbox::sandbox::{Limits, Sandbox};
use tinysandbox::vfs::{
    DirEntry, Errno, FileHandle, FileType, InMemoryVfs, Metadata, OpenMode, Vfs, VfsError,
    VfsResult,
};

const CHUNK: usize = 64 * 1024;
const GENERATED_HANDLE: FileHandle = FileHandle::new(u64::MAX);

#[tokio::test]
async fn sort_rejects_during_input_consumption() {
    for command in ["sort huge", "cat huge | sort"] {
        let vfs = Arc::new(ProbeVfs::new(4 * 1024 * 1024));
        let sandbox = Sandbox::builder()
            .mount_arc("workspace", vfs.clone())
            .limits(Limits {
                sort_input_bytes: 1024,
                ..Limits::default()
            })
            .build();
        let result = tokio::time::timeout(Duration::from_secs(2), sandbox.exec(command))
            .await
            .expect("bounded sort should stop its producer");
        assert_eq!(result.exit_code, 2, "{command}: {}", result.stderr);
        assert!(result.stderr.contains("input too large"));
        assert!(
            vfs.served.load(Ordering::Relaxed) <= (CHUNK * 4) as u64,
            "{command} consumed {}",
            vfs.served.load(Ordering::Relaxed)
        );
    }
}

#[tokio::test]
async fn sort_shares_one_budget_across_files_and_stdin() {
    let sandbox = Sandbox::builder()
        .limits(Limits {
            sort_input_bytes: 8,
            ..Limits::default()
        })
        .build();
    assert_eq!(
        sandbox
            .exec("echo abc > one; echo def > two")
            .await
            .exit_code,
        0
    );
    let allowed = sandbox.exec("sort one two").await;
    assert_eq!(allowed.exit_code, 0, "{}", allowed.stderr);
    assert_eq!(allowed.stdout, "abc\ndef\n");
    let rejected = sandbox.exec("echo x | sort one two -").await;
    assert_eq!(rejected.exit_code, 2);
    assert!(rejected.stderr.contains("input too large"));
}

#[tokio::test]
async fn tail_caps_retained_bytes_and_evicts_before_admission() {
    let sandbox = Sandbox::builder()
        .limits(Limits {
            tail_input_bytes: 4,
            ..Limits::default()
        })
        .build();
    assert_eq!(sandbox.exec("echo 'aa\nbb\ncc' > lines").await.exit_code, 0);
    let one = sandbox.exec("tail -n 1 lines").await;
    assert_eq!(one.exit_code, 0, "{}", one.stderr);
    assert_eq!(one.stdout, "cc\n");
    let two = sandbox.exec("tail -n 2 lines").await;
    assert_eq!(two.exit_code, 1);
    assert!(two.stderr.contains("retained input too large"));
    let from = sandbox.exec("tail -n +2 lines").await;
    assert_eq!(from.exit_code, 0, "{}", from.stderr);
    assert_eq!(from.stdout, "bb\ncc\n");
    let zero = sandbox.exec("tail -n 0 lines").await;
    assert_eq!(zero.exit_code, 0, "{}", zero.stderr);
    assert!(zero.stdout.is_empty());
}

#[tokio::test]
async fn copy_rejects_normalized_self_and_descendant_targets_without_mutation() {
    let vfs = Arc::new(InMemoryVfs::default());
    let sandbox = Sandbox::builder()
        .mount_arc("workspace", vfs.clone())
        .build();
    assert_eq!(
        sandbox
            .exec("mkdir a; echo preserved > a/file")
            .await
            .exit_code,
        0
    );
    for command in [
        "cp -r a a/sub",
        "cp -r a a",
        "cp -r ./a a/../a/sub",
        "cp a/file a/./file",
        "cp -r a .",
    ] {
        let result = sandbox.exec(command).await;
        assert_eq!(result.exit_code, 1, "{command}: {}", result.stderr);
        assert_eq!(sandbox.exec("cat a/file").await.stdout, "preserved\n");
        let entries = vfs.readdir("/a").unwrap();
        assert_eq!(entries.len(), 1, "{command} mutated source directory");
    }
}

#[tokio::test]
async fn copy_streams_large_files_handles_short_writes_and_closes_both_handles() {
    let size = CHUNK as u64 * 3 + 17;
    let vfs = Arc::new(ProbeVfs::new(size));
    let sandbox = Sandbox::builder()
        .mount_arc("workspace", vfs.clone())
        .limits(Limits {
            host_input_bytes: 1024,
            ..Limits::default()
        })
        .build();
    let result = sandbox.exec("cp huge copied").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(vfs.inner.stat("/copied").unwrap().len, size);
    assert_eq!(vfs.served.load(Ordering::Relaxed), size);
    assert!(vfs.first_write_after_read.load(Ordering::Relaxed) <= CHUNK as u64);
    assert!(vfs.largest_read.load(Ordering::Relaxed) <= CHUNK);
    assert!(vfs.largest_write.load(Ordering::Relaxed) <= CHUNK);
    assert_eq!(vfs.source_closes.load(Ordering::Relaxed), 1);
    assert_eq!(vfs.dest_closes.load(Ordering::Relaxed), 1);
    let handle = vfs.inner.open("/copied", OpenMode::read_only()).unwrap();
    let mut content = vec![0; size as usize];
    assert_eq!(
        vfs.inner.read_at(handle, 0, &mut content).unwrap(),
        content.len()
    );
    vfs.inner.close(handle).unwrap();
    assert!(
        content
            .iter()
            .enumerate()
            .all(|(i, byte)| *byte == generated_byte(i as u64))
    );
}

#[tokio::test]
async fn copy_surfaces_destination_close_failure() {
    let vfs = Arc::new(ProbeVfs::new(123));
    vfs.fail_dest_close.store(true, Ordering::Relaxed);
    let sandbox = Sandbox::builder()
        .mount_arc("workspace", vfs.clone())
        .build();
    let result = sandbox.exec("cp huge copied").await;
    assert_eq!(result.exit_code, 1);
    assert!(result.stderr.contains("Input/output error"));
    assert_eq!(vfs.source_closes.load(Ordering::Relaxed), 1);
    assert_eq!(vfs.dest_closes.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn copy_aborts_destination_on_read_write_or_source_close_error() {
    for failure in ["read", "write", "close"] {
        let vfs = Arc::new(ProbeVfs::new(CHUNK as u64 * 2));
        match failure {
            "read" => vfs.fail_read_at.store(CHUNK as u64, Ordering::Relaxed),
            "write" => vfs.zero_write.store(true, Ordering::Relaxed),
            "close" => vfs.fail_source_close.store(true, Ordering::Relaxed),
            _ => unreachable!(),
        }
        let sandbox = Sandbox::builder()
            .mount_arc("workspace", vfs.clone())
            .build();
        let result = sandbox.exec("cp huge copied").await;
        assert_eq!(result.exit_code, 1, "{failure}: {}", result.stderr);
        assert_eq!(vfs.source_closes.load(Ordering::Relaxed), 1, "{failure}");
        assert_eq!(vfs.dest_closes.load(Ordering::Relaxed), 0, "{failure}");
        assert_eq!(vfs.dest_aborts.load(Ordering::Relaxed), 1, "{failure}");
    }
}

#[tokio::test]
async fn sed_streams_amplified_replacements_to_an_early_consumer() {
    let vfs = Arc::new(InMemoryVfs::default());
    let handle = vfs.open("/input", OpenMode::write_only().create()).unwrap();
    vfs.write_at(handle, 0, &vec![b'a'; 8192]).unwrap();
    vfs.close(handle).unwrap();
    let sandbox = Sandbox::builder().mount_arc("workspace", vfs).build();
    let command = format!("sed 's/a/done\\n{}/g' input | head -n 1", "x".repeat(8192));
    let result = tokio::time::timeout(Duration::from_secs(2), sandbox.exec(&command))
        .await
        .expect("sed should stop as soon as the consumer closes");
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "done\n");
    assert!(result.stderr.is_empty());
}

#[tokio::test]
async fn mkdir_parents_respects_cwd_and_requires_existing_directories() {
    let sandbox = Sandbox::builder().build();
    let result = sandbox
        .exec("mkdir -p a/b; echo content > file; mkdir -p file")
        .await;
    assert_eq!(result.exit_code, 1);
    assert_eq!(sandbox.exec("stat a/b").await.exit_code, 0);
    assert!(result.stderr.contains("Not a directory"));
}

#[tokio::test]
async fn recursive_copy_and_remove_preserve_siblings_and_contents() {
    let sandbox = Sandbox::builder().build();
    assert_eq!(
        sandbox
            .exec("mkdir -p a/x/y a/z; echo data > a/x/y/f; echo sibling > a/z/g")
            .await
            .exit_code,
        0
    );
    let result = sandbox.exec("cp -r a b; rm -r a; cat b/x/y/f b/z/g").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "data\nsibling\n");
    assert_eq!(sandbox.exec("stat a").await.exit_code, 1);
}

#[tokio::test]
async fn line_buffer_handles_chunk_boundaries_and_unterminated_tail() {
    let vfs = Arc::new(InMemoryVfs::default());
    let mut content = b"x\n".repeat(4095);
    content.extend_from_slice(&vec![b'a'; 8193]);
    content.extend_from_slice(b"\nlast");
    let handle = vfs.open("/lines", OpenMode::write_only().create()).unwrap();
    vfs.write_at(handle, 0, &content).unwrap();
    vfs.close(handle).unwrap();
    let sandbox = Sandbox::builder().mount_arc("workspace", vfs).build();
    let result = sandbox.exec("tail -n 2 lines").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout.as_bytes(), &content[8190..]);
    assert_eq!(sandbox.exec("grep -c x lines").await.stdout, "4095\n");
}

#[tokio::test]
async fn jq_serializes_large_single_values_without_changing_bytes() {
    let sandbox = Sandbox::builder().build();
    let result = sandbox.exec("jq -nr '\"x\" * 200000' | wc -c").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout.trim(), "200001");
}

#[derive(Debug)]
struct ProbeVfs {
    inner: InMemoryVfs,
    size: u64,
    served: AtomicU64,
    largest_read: AtomicUsize,
    largest_write: AtomicUsize,
    first_write_after_read: AtomicU64,
    source_closes: AtomicUsize,
    dest_closes: AtomicUsize,
    fail_dest_close: AtomicBool,
    fail_source_close: AtomicBool,
    zero_write: AtomicBool,
    fail_read_at: AtomicU64,
    dest_aborts: AtomicUsize,
}

impl ProbeVfs {
    fn new(size: u64) -> Self {
        Self {
            inner: InMemoryVfs::default(),
            size,
            served: AtomicU64::new(0),
            largest_read: AtomicUsize::new(0),
            largest_write: AtomicUsize::new(0),
            first_write_after_read: AtomicU64::new(u64::MAX),
            source_closes: AtomicUsize::new(0),
            dest_closes: AtomicUsize::new(0),
            fail_dest_close: AtomicBool::new(false),
            fail_source_close: AtomicBool::new(false),
            zero_write: AtomicBool::new(false),
            fail_read_at: AtomicU64::new(u64::MAX),
            dest_aborts: AtomicUsize::new(0),
        }
    }
}

impl Vfs for ProbeVfs {
    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        if path == "/huge" {
            Ok(Metadata {
                file_type: FileType::File,
                len: self.size,
            })
        } else {
            self.inner.stat(path)
        }
    }
    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        self.inner.readdir(path)
    }
    fn mkdir(&self, path: &str) -> VfsResult<()> {
        self.inner.mkdir(path)
    }
    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        self.inner.rename(from, to)
    }
    fn unlink(&self, path: &str) -> VfsResult<()> {
        self.inner.unlink(path)
    }
    fn rmdir(&self, path: &str) -> VfsResult<()> {
        self.inner.rmdir(path)
    }
    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        if path == "/huge" {
            if mode.read && !mode.write {
                Ok(GENERATED_HANDLE)
            } else {
                Err(VfsError::new(Errno::EACCES))
            }
        } else {
            self.inner.open(path, mode)
        }
    }
    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if handle != GENERATED_HANDLE {
            return self.inner.read_at(handle, offset, buf);
        }
        if offset >= self.fail_read_at.load(Ordering::Relaxed) {
            return Err(VfsError::new(Errno::EIO));
        }
        self.largest_read.fetch_max(buf.len(), Ordering::Relaxed);
        let n = buf.len().min(self.size.saturating_sub(offset) as usize);
        for (i, byte) in buf[..n].iter_mut().enumerate() {
            *byte = generated_byte(offset + i as u64);
        }
        self.served.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        self.first_write_after_read
            .compare_exchange(
                u64::MAX,
                self.served.load(Ordering::Relaxed),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok();
        self.largest_write.fetch_max(data.len(), Ordering::Relaxed);
        if self.zero_write.load(Ordering::Relaxed) {
            return Ok(0);
        }
        self.inner
            .write_at(handle, offset, &data[..data.len().min(1023)])
    }
    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        self.inner.truncate(handle, len)
    }
    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        if handle == GENERATED_HANDLE {
            self.source_closes.fetch_add(1, Ordering::Relaxed);
            if self.fail_source_close.load(Ordering::Relaxed) {
                Err(VfsError::new(Errno::EIO))
            } else {
                Ok(())
            }
        } else {
            self.dest_closes.fetch_add(1, Ordering::Relaxed);
            self.inner.close(handle)?;
            if self.fail_dest_close.load(Ordering::Relaxed) {
                Err(VfsError::new(Errno::EIO))
            } else {
                Ok(())
            }
        }
    }
    fn abort(&self, handle: FileHandle) -> VfsResult<()> {
        self.dest_aborts.fetch_add(1, Ordering::Relaxed);
        self.inner.close(handle)
    }
    fn is_fast(&self) -> bool {
        true
    }
}

fn generated_byte(offset: u64) -> u8 {
    if offset % 79 == 78 {
        b'\n'
    } else {
        b'a' + (offset % 26) as u8
    }
}
