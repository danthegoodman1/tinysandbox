//! Concurrency ceilings shared by every sandbox built from the same [`Pools`].
//!
//! [`Limits`](super::Limits) bounds one execution. The resources here are
//! bounded across executions, because what they protect — OS threads, file
//! descriptors, and blocking-pool capacity — is owned by the process rather
//! than by any one sandbox. Hosts that run several tenants in one process give
//! each tenant its own `Pools` so one tenant cannot exhaust another's share.

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Semaphore;

/// Capacity of the resources that sandboxes sharing a [`Pools`] contend for.
///
/// The defaults match the values tinysandbox has always enforced, so a sandbox
/// built without explicit pools behaves exactly as before.
///
/// ```
/// use std::sync::Arc;
/// use tinysandbox::sandbox::{PoolCapacity, Pools, Sandbox};
///
/// let tenant = Pools::new(PoolCapacity::default().with_jq_workers(4));
/// let sandbox = Sandbox::builder().pools(Arc::clone(&tenant)).build();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolCapacity {
    /// Concurrent `jq` guests. Each holds an OS thread while it evaluates.
    pub jq_workers: usize,
    /// Concurrent `js` guests. Each holds an OS thread while it evaluates.
    pub js_workers: usize,
    /// Concurrent blocking VFS operations dispatched to the blocking pool.
    pub blocking_vfs_workers: usize,
    /// Open file handles held at once across every sharing sandbox.
    pub open_files: usize,
    /// Threads that release handles a cancelled execution abandoned.
    pub cleanup_threads: usize,
}

impl Default for PoolCapacity {
    fn default() -> Self {
        Self {
            jq_workers: 16,
            js_workers: 16,
            blocking_vfs_workers: 128,
            open_files: 16384,
            cleanup_threads: 4,
        }
    }
}

impl PoolCapacity {
    /// Sets the concurrent `jq` guest ceiling.
    #[must_use]
    pub const fn with_jq_workers(mut self, jq_workers: usize) -> Self {
        self.jq_workers = jq_workers;
        self
    }

    /// Sets the concurrent `js` guest ceiling.
    #[must_use]
    pub const fn with_js_workers(mut self, js_workers: usize) -> Self {
        self.js_workers = js_workers;
        self
    }

    /// Sets the concurrent blocking VFS operation ceiling.
    #[must_use]
    pub const fn with_blocking_vfs_workers(mut self, blocking_vfs_workers: usize) -> Self {
        self.blocking_vfs_workers = blocking_vfs_workers;
        self
    }

    /// Sets the shared open-handle ceiling.
    #[must_use]
    pub const fn with_open_files(mut self, open_files: usize) -> Self {
        self.open_files = open_files;
        self
    }

    /// Sets the handle-cleanup thread count.
    #[must_use]
    pub const fn with_cleanup_threads(mut self, cleanup_threads: usize) -> Self {
        self.cleanup_threads = cleanup_threads;
        self
    }
}

/// Work handed to a cleanup thread. Releasing a handle can call a remote
/// backend, so it must not block an executor thread or require a runtime.
pub(crate) type CleanupJob = Box<dyn FnOnce() + Send>;

/// Resources shared by every sandbox built with these pools.
///
/// Build one per isolation domain and pass it to
/// [`SandboxBuilder::pools`](super::SandboxBuilder::pools). Sandboxes built
/// without one share a process-wide default, which keeps a single-sandbox
/// embedding working with no configuration.
pub struct Pools {
    capacity: PoolCapacity,
    jq_workers: Arc<Semaphore>,
    js_workers: Arc<Semaphore>,
    blocking_vfs_workers: Arc<Semaphore>,
    open_files: AtomicUsize,
    cleanup: OnceLock<std::sync::mpsc::Sender<CleanupJob>>,
}

impl fmt::Debug for Pools {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Pools")
            .field("capacity", &self.capacity)
            .field("open_files", &self.open_files.load(Ordering::Relaxed))
            .finish()
    }
}

impl Pools {
    /// Builds pools with the given capacity.
    ///
    /// A zero ceiling would deadlock the work it admits, so every field is
    /// raised to at least one.
    pub fn new(capacity: PoolCapacity) -> Arc<Self> {
        let capacity = PoolCapacity {
            jq_workers: capacity.jq_workers.max(1),
            js_workers: capacity.js_workers.max(1),
            blocking_vfs_workers: capacity.blocking_vfs_workers.max(1),
            open_files: capacity.open_files.max(1),
            cleanup_threads: capacity.cleanup_threads.max(1),
        };
        Arc::new(Self {
            jq_workers: Arc::new(Semaphore::new(capacity.jq_workers)),
            js_workers: Arc::new(Semaphore::new(capacity.js_workers)),
            blocking_vfs_workers: Arc::new(Semaphore::new(capacity.blocking_vfs_workers)),
            open_files: AtomicUsize::new(0),
            cleanup: OnceLock::new(),
            capacity,
        })
    }

    /// Returns the process-wide pools used by sandboxes that request none.
    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<Pools>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| Pools::new(PoolCapacity::default())))
    }

    /// Returns the capacity these pools enforce, after the minimum is applied.
    pub fn capacity(&self) -> PoolCapacity {
        self.capacity
    }

    /// Returns the handles currently open across every sharing sandbox.
    pub fn open_files(&self) -> usize {
        self.open_files.load(Ordering::Relaxed)
    }

    pub(crate) fn jq_workers(&self) -> Arc<Semaphore> {
        Arc::clone(&self.jq_workers)
    }

    pub(crate) fn js_workers(&self) -> Arc<Semaphore> {
        Arc::clone(&self.js_workers)
    }

    pub(crate) fn blocking_vfs_workers(&self) -> Arc<Semaphore> {
        Arc::clone(&self.blocking_vfs_workers)
    }

    pub(crate) fn acquire_file(&self) -> bool {
        self.open_files
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.capacity.open_files).then_some(n + 1)
            })
            .is_ok()
    }

    pub(crate) fn release_file(&self) {
        self.open_files.fetch_sub(1, Ordering::AcqRel);
    }

    /// Runs `job` on a cleanup thread, or inline when no thread can take it.
    pub(crate) fn spawn_cleanup(&self, job: CleanupJob) {
        let sender = self.cleanup.get_or_init(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<CleanupJob>();
            let receiver = Arc::new(Mutex::new(receiver));
            for _ in 0..self.capacity.cleanup_threads {
                let receiver = Arc::clone(&receiver);
                // Dropping the pools closes the channel, which drains the queue
                // and then ends every worker.
                let worker = std::thread::Builder::new()
                    .name("tinysandbox-fs-cleanup".into())
                    .spawn(move || {
                        while let Ok(job) =
                            receiver.lock().unwrap_or_else(|e| e.into_inner()).recv()
                        {
                            job();
                        }
                    });
                if worker.is_err() {
                    break;
                }
            }
            sender
        });
        if let Err(returned) = sender.send(job) {
            returned.0();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_capacity_cannot_deadlock_the_work_it_admits() {
        let pools = Pools::new(
            PoolCapacity::default()
                .with_jq_workers(0)
                .with_open_files(0)
                .with_cleanup_threads(0),
        );
        assert_eq!(pools.capacity().jq_workers, 1);
        assert_eq!(pools.capacity().open_files, 1);
        assert_eq!(pools.capacity().cleanup_threads, 1);
    }

    #[test]
    fn separate_pools_account_for_handles_independently() {
        let first = Pools::new(PoolCapacity::default().with_open_files(1));
        let second = Pools::new(PoolCapacity::default().with_open_files(1));
        assert!(first.acquire_file());
        assert!(!first.acquire_file(), "the ceiling is enforced");
        assert!(
            second.acquire_file(),
            "a separate domain keeps its own budget"
        );
        first.release_file();
        assert!(first.acquire_file());
    }

    #[test]
    fn cleanup_runs_inline_once_the_queue_is_gone() {
        let pools = Pools::new(PoolCapacity::default().with_cleanup_threads(1));
        let (done, finished) = std::sync::mpsc::channel();
        pools.spawn_cleanup(Box::new(move || done.send(()).unwrap()));
        finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("queued cleanup runs");
    }
}
