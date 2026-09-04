//! Execution-wide admission and cooperative cancellation.
use super::{Limits, fs::HandleRegistry};
use crate::vfs::{Errno, VfsError, VfsResult};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

pub(crate) struct ExecutionControl {
    deadline: Instant,
    cancelled: AtomicBool,
    open_files: AtomicUsize,
    registries: Mutex<Vec<Weak<HandleRegistry>>>,
    pub(crate) limits: Limits,
}
impl ExecutionControl {
    pub(crate) fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            deadline: Instant::now()
                .checked_add(limits.wall_time)
                .unwrap_or_else(Instant::now),
            cancelled: AtomicBool::new(false),
            open_files: AtomicUsize::new(0),
            registries: Mutex::new(Vec::new()),
            limits,
        })
    }
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.remaining().is_zero()
    }
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let registries =
            std::mem::take(&mut *self.registries.lock().unwrap_or_else(|e| e.into_inner()));
        for registry in registries.into_iter().filter_map(|r| r.upgrade()) {
            registry.abandon_all();
        }
    }
    pub(crate) fn register(&self, registry: &Arc<HandleRegistry>) {
        self.registries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::downgrade(registry));
    }
    pub(crate) fn check(&self) -> VfsResult<()> {
        if self.is_cancelled() {
            Err(VfsError::new(Errno::EIO))
        } else {
            Ok(())
        }
    }
    pub(crate) fn acquire_file(&self) -> VfsResult<()> {
        self.check()?;
        self.open_files
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.limits.max_open_files).then_some(n + 1)
            })
            .map(|_| ())
            .map_err(|_| VfsError::new(Errno::ENOSPC))
    }
    pub(crate) fn release_file(&self) {
        self.open_files.fetch_sub(1, Ordering::AcqRel);
    }
}
/// Cancels retained command capabilities when exec completes or its future is dropped.
pub(crate) struct ExecutionGuard(pub(crate) Arc<ExecutionControl>);
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
