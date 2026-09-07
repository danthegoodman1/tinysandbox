//! Cancellation at asynchronous filesystem ownership handoffs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tinysandbox::sandbox::{CommandResult, Sandbox};
use tinysandbox::vfs::{DirEntry, FileHandle, InMemoryVfs, Metadata, OpenMode, Vfs, VfsResult};

#[derive(Default)]
struct OpenGate {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    wake: Condvar,
}

impl OpenGate {
    fn wait(&self) {
        self.entered.notify_one();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

#[derive(Default)]
struct GatedVfs {
    inner: InMemoryVfs,
    gate: OpenGate,
    write_gate: Option<Arc<OpenGate>>,
    active: AtomicUsize,
    aborted: AtomicUsize,
    closed: AtomicUsize,
}

impl Vfs for GatedVfs {
    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        self.inner.stat(path)
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
        let handle = self.inner.open(path, mode)?;
        self.active.fetch_add(1, Ordering::SeqCst);
        self.gate.wait();
        Ok(handle)
    }
    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.inner.read_at(handle, offset, buf)
    }
    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        if let Some(gate) = &self.write_gate {
            gate.wait();
        }
        self.inner.write_at(handle, offset, data)
    }
    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        self.inner.truncate(handle, len)
    }
    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        self.inner.close(handle)?;
        self.closed.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
    fn abort(&self, handle: FileHandle) -> VfsResult<()> {
        self.inner.abort(handle)?;
        self.aborted.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
}

// A failed assertion must still release the blocking backend call so runtime
// shutdown cannot hang while waiting for its worker.
struct ReleaseVfsOnDrop(Arc<GatedVfs>);
impl Drop for ReleaseVfsOnDrop {
    fn drop(&mut self) {
        self.0.gate.release();
        if let Some(gate) = &self.0.write_gate {
            gate.release();
        }
    }
}

async fn assert_cleaned_up(vfs: &GatedVfs) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while vfs.active.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("abandoned open handle is released without dropping the sandbox");
    assert_eq!(vfs.aborted.load(Ordering::SeqCst), 1);
    assert_eq!(vfs.closed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dropping_exec_aborts_a_blocking_open_that_finishes_later() {
    let vfs = Arc::new(GatedVfs::default());
    let _release = ReleaseVfsOnDrop(Arc::clone(&vfs));
    let sandbox = Sandbox::builder()
        .clear_mounts()
        .mount_arc("disk", Arc::clone(&vfs) as Arc<dyn Vfs>)
        .build();
    let mut exec = Box::pin(sandbox.exec("echo new > /disk/out"));
    tokio::select! {
        result = &mut exec => panic!("exec completed before the blocked open: {result:?}"),
        _ = vfs.gate.entered.notified() => {}
    }
    assert_eq!(vfs.active.load(Ordering::SeqCst), 1);
    drop(exec);
    vfs.gate.release();
    assert_cleaned_up(&vfs).await;
}

#[tokio::test]
async fn dropping_one_host_open_future_reclaims_its_unobserved_handle() {
    let vfs = Arc::new(GatedVfs::default());
    let _release = ReleaseVfsOnDrop(Arc::clone(&vfs));
    let sandbox = Sandbox::builder()
        .clear_mounts()
        .mount_arc("disk", Arc::clone(&vfs) as Arc<dyn Vfs>)
        .build();
    let fs = sandbox.fs();
    let mut open = Box::pin(fs.open("/disk/out", OpenMode::write_only().create()));
    tokio::select! {
        result = &mut open => panic!("open completed before the backend gate: {result:?}"),
        _ = vfs.gate.entered.notified() => {}
    }
    drop(open);
    vfs.gate.release();
    assert_cleaned_up(&vfs).await;
    // The same public Fs remains usable after cancellation of one operation.
    let handle = fs
        .open("/disk/next", OpenMode::write_only().create())
        .await
        .expect("subsequent open");
    fs.close(handle).await.expect("subsequent close");
}

#[tokio::test]
async fn completion_reclaims_handles_even_when_custom_code_retains_its_fs() {
    let vfs = Arc::new(GatedVfs::default());
    vfs.gate.release();
    let retained = Arc::new(Mutex::new(None));
    let store = Arc::clone(&retained);
    let sandbox = Sandbox::builder()
        .clear_mounts()
        .mount_arc("disk", Arc::clone(&vfs) as Arc<dyn Vfs>)
        .command("retain", move |ctx| {
            let store = Arc::clone(&store);
            async move {
                ctx.fs
                    .open("/disk/open", OpenMode::write_only().create())
                    .await
                    .expect("leave open handle");
                *store.lock().unwrap() = Some(ctx.fs);
                CommandResult::success()
            }
        })
        .build();
    assert_eq!(sandbox.exec("retain").await.exit_code, 0);
    assert_cleaned_up(&vfs).await;
    let fs = retained
        .lock()
        .unwrap()
        .take()
        .expect("retained execution capability");
    assert!(fs.mkdir("/disk/late").await.is_err());
    assert!(vfs.inner.stat("/late").is_err());
}

#[tokio::test]
async fn cancelling_one_redirect_write_does_not_discard_the_next_write() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;
    use tokio::io::AsyncWriteExt;

    let write_gate = Arc::new(OpenGate::default());
    let vfs = Arc::new(GatedVfs {
        write_gate: Some(Arc::clone(&write_gate)),
        ..GatedVfs::default()
    });
    vfs.gate.release();
    let _release = ReleaseVfsOnDrop(Arc::clone(&vfs));
    let sandbox = Sandbox::builder()
        .clear_mounts()
        .mount_arc("disk", Arc::clone(&vfs) as Arc<dyn Vfs>)
        .command("cancelwrite", move |mut ctx| {
            let write_gate = Arc::clone(&write_gate);
            async move {
                let mut first_write = Box::pin(ctx.stdout.write_all(b"old"));
                let accepted = tokio::select! {
                    // An implementation may accept owned bytes immediately
                    // and defer backend completion to flush/shutdown.
                    result = &mut first_write => { result.expect("first write accepted"); true },
                    _ = write_gate.entered.notified() => false
                };
                drop(first_write);
                if accepted {
                    // The first accepted chunk is still blocked in the backend.
                    // Cancel a second write while it waits for that chunk.
                    let mut waiting = Box::pin(ctx.stdout.write_all(b"discarded"));
                    poll_fn(|cx| {
                        assert!(
                            waiting.as_mut().poll(cx).is_pending(),
                            "destination admitted an unbounded second chunk"
                        );
                        Poll::Ready(())
                    })
                    .await;
                    drop(waiting);
                }
                write_gate.release();
                ctx.stdout
                    .write_all(b"new")
                    .await
                    .expect("subsequent write");
                ctx.stdout.flush().await.expect("flush accepted writes");
                CommandResult::success()
            }
        })
        .build();
    let result = sandbox.exec("cancelwrite > /disk/out").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    let bytes = sandbox
        .fs()
        .read_file("/disk/out")
        .await
        .expect("read redirected bytes");
    // The first backend operation had already started and may complete. The
    // subsequent successful write must always contribute its own bytes.
    assert!(
        bytes.ends_with(b"new"),
        "later write was discarded: {bytes:?}"
    );
}

#[tokio::test]
async fn redirect_flush_waits_for_accepted_backend_writes() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;
    use tinysandbox::sandbox::Limits;
    use tokio::io::AsyncWriteExt;

    let write_gate = Arc::new(OpenGate::default());
    let vfs = Arc::new(GatedVfs {
        write_gate: Some(Arc::clone(&write_gate)),
        ..GatedVfs::default()
    });
    vfs.gate.release();
    let _release = ReleaseVfsOnDrop(Arc::clone(&vfs));
    let sandbox = Sandbox::builder()
        .clear_mounts()
        .mount_arc("disk", Arc::clone(&vfs) as Arc<dyn Vfs>)
        .limits(Limits {
            wall_time: Duration::from_millis(500),
            ..Limits::default()
        })
        .command("flushgate", move |mut ctx| {
            let write_gate = Arc::clone(&write_gate);
            async move {
                ctx.stdout
                    .write_all(b"accepted")
                    .await
                    .expect("accept owned bytes");
                let mut flush = Box::pin(ctx.stdout.flush());
                poll_fn(|cx| {
                    assert!(
                        flush.as_mut().poll(cx).is_pending(),
                        "flush completed while its backend write was blocked"
                    );
                    Poll::Ready(())
                })
                .await;
                write_gate.release();
                flush.await.expect("flush completed backend write");
                CommandResult::success()
            }
        })
        .build();
    let result = sandbox.exec("flushgate > /disk/out").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(
        sandbox
            .fs()
            .read_file("/disk/out")
            .await
            .expect("read accepted bytes"),
        b"accepted"
    );
}

fn concurrent_pipe_writer(completed: Arc<AtomicUsize>) -> Sandbox {
    use tinysandbox::sandbox::Limits;
    use tokio::io::AsyncWriteExt;

    Sandbox::builder()
        .limits(Limits {
            wall_time: Duration::from_secs(1),
            ..Limits::default()
        })
        .command("both", move |mut ctx| {
            let first_done = Arc::clone(&completed);
            let second_done = Arc::clone(&completed);
            async move {
                // Separate tasks are essential: joining two writes directly
                // would share one waker and hide lost-wakeup bugs in dup fds.
                let first = tokio::spawn(async move {
                    let result = ctx.stdout.write_all(&b"x\n".repeat(128 * 1024)).await;
                    first_done.fetch_add(1, Ordering::SeqCst);
                    result
                });
                let second = tokio::spawn(async move {
                    let result = ctx.stderr.write_all(&b"x\n".repeat(128 * 1024)).await;
                    second_done.fetch_add(1, Ordering::SeqCst);
                    result
                });
                let (first, second) = tokio::join!(first, second);
                if first.expect("stdout task").is_ok() && second.expect("stderr task").is_ok() {
                    CommandResult::success()
                } else {
                    CommandResult::failure()
                }
            }
        })
        .build()
}

#[tokio::test]
async fn duplicated_pipe_writers_wake_separate_stdout_and_stderr_tasks() {
    let completed = Arc::new(AtomicUsize::new(0));
    let sandbox = concurrent_pipe_writer(Arc::clone(&completed));
    let result = sandbox.exec("both 2>&1 | wc -c").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout.trim(), "524288");
    assert!(result.stderr.is_empty());
    assert_eq!(completed.load(Ordering::SeqCst), 2);
    assert_eq!(result.metrics.pipe_bytes, vec![524288]);
}

#[tokio::test]
async fn early_reader_exit_wakes_both_duplicated_pipe_writers() {
    let completed = Arc::new(AtomicUsize::new(0));
    let sandbox = concurrent_pipe_writer(Arc::clone(&completed));
    let result = sandbox.exec("both 2>&1 | head -n 1").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "x\n");
    assert!(result.stderr.is_empty());
    assert_eq!(completed.load(Ordering::SeqCst), 2);
    assert!(result.metrics.pipe_bytes[0] < 524288);
}

#[tokio::test]
async fn shutting_down_one_pipe_descriptor_preserves_its_duplicate() {
    use tokio::io::AsyncWriteExt;

    let sandbox = Sandbox::builder()
        .command("dupclose", |mut ctx| async move {
            ctx.stdout
                .write_all(b"A")
                .await
                .expect("first descriptor write");
            ctx.stdout.shutdown().await.expect("close first descriptor");
            ctx.stderr
                .write_all(b"B")
                .await
                .expect("duplicate remains writable");
            CommandResult::success()
        })
        .build();
    let result = sandbox.exec("dupclose 2>&1 | wc -c").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout.trim(), "2");
    assert!(result.stderr.is_empty());
    assert_eq!(result.metrics.commands[0].exit_code, 0);
    assert_eq!(result.metrics.pipe_bytes, vec![2]);
}
