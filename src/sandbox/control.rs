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
    cancellation: Arc<CancellationState>,
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
            cancellation: Arc::new(CancellationState::default()),
            open_files: AtomicUsize::new(0),
            registries: Mutex::new(Vec::new()),
            limits,
        })
    }
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled() || self.remaining().is_zero()
    }
    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
        let registries =
            std::mem::take(&mut *self.registries.lock().unwrap_or_else(|e| e.into_inner()));
        for registry in registries.into_iter().filter_map(|r| r.upgrade()) {
            registry.abandon_all();
        }
    }
    pub(crate) fn host_context(&self) -> HostContext {
        HostContext {
            cancellation: Arc::clone(&self.cancellation),
            parent: None,
            deadline: Some(self.deadline),
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

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    changed: tokio::sync::Notify,
}
impl CancellationState {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
}

/// Cooperative cancellation and the monotonic deadline for trusted host work.
///
/// Custom commands obtain this from [`super::fs::Fs::host_context`]. Context-aware
/// JS globals and fetch handlers receive a context for their individual call,
/// whose deadline may precede the enclosing execution deadline. An individual
/// call's context is cancelled when its callback settles, whether it succeeds or
/// returns an error. This does not cancel the enclosing execution. All clones
/// also observe cancellation when the execution completes or its future is dropped.
///
/// This signal cannot preempt synchronous blocking code or limit allocations in
/// trusted callbacks. Hosts must bound their own work and propagate cancellation
/// to downstream requests. Waiting uses the current Tokio runtime's time driver.
#[derive(Clone)]
pub struct HostContext {
    cancellation: Arc<CancellationState>,
    parent: Option<Arc<CancellationState>>,
    deadline: Option<Instant>,
}
impl std::fmt::Debug for HostContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostContext")
            .field("deadline", &self.deadline)
            .field("is_cancelled", &self.is_cancelled())
            .finish()
    }
}
impl HostContext {
    pub(crate) fn unscoped() -> Self {
        Self {
            cancellation: Arc::new(CancellationState::default()),
            parent: None,
            deadline: None,
        }
    }

    /// The monotonic deadline, or none for host filesystem work outside an exec.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Time left before the deadline, or none outside a bounded execution.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// Whether the execution or individual host call ended, was cancelled, or expired.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.is_cancelled())
            || self
                .remaining()
                .is_some_and(|remaining| remaining.is_zero())
    }

    /// Resolves when the execution or individual call ends, is cancelled, or expires.
    ///
    /// Registration and the state check prevent a cancellation racing this
    /// method from being lost. Dropping this future releases its timer and
    /// notification registrations; it never starts a background task.
    pub async fn cancelled(&self) {
        use std::future::{Future, poll_fn};
        use std::task::Poll;
        let mut own = Box::pin(self.cancellation.changed.notified());
        own.as_mut().enable();
        let mut parent = self
            .parent
            .as_ref()
            .map(|parent| Box::pin(parent.changed.notified()));
        if let Some(parent) = parent.as_mut() {
            parent.as_mut().enable();
        }
        if self.is_cancelled() {
            return;
        }
        let notified = poll_fn(|cx| {
            if self.is_cancelled()
                || own.as_mut().poll(cx).is_ready()
                || parent
                    .as_mut()
                    .is_some_and(|parent| parent.as_mut().poll(cx).is_ready())
            {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        match self.deadline {
            Some(deadline) => {
                let _ = tokio::time::timeout_at(deadline.into(), notified).await;
            }
            None => notified.await,
        }
    }

    #[cfg(feature = "js")]
    pub(crate) fn child(&self, deadline: Instant) -> Self {
        Self {
            cancellation: Arc::new(CancellationState::default()),
            parent: Some(Arc::clone(
                self.parent.as_ref().unwrap_or(&self.cancellation),
            )),
            deadline: Some(self.deadline.map_or(deadline, |outer| outer.min(deadline))),
        }
    }

    #[cfg(feature = "js")]
    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    #[tokio::test]
    async fn cancellation_notifies_registered_and_late_waiters_without_lost_wakeups() {
        for _ in 0..100 {
            let control = ExecutionControl::new(Limits::default());
            let context = control.host_context();
            let mut first = Box::pin(context.cancelled());
            let mut second = Box::pin(context.cancelled());
            poll_fn(|cx| {
                assert!(first.as_mut().poll(cx).is_pending());
                assert!(second.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            control.cancel();
            tokio::time::timeout(Duration::from_millis(100), async {
                first.await;
                second.await;
                context.cancelled().await;
            })
            .await
            .unwrap();
        }
    }

    #[cfg(feature = "js")]
    #[tokio::test]
    async fn callback_cancellation_does_not_cancel_the_enclosing_execution() {
        let control = ExecutionControl::new(Limits::default());
        let execution = control.host_context();
        let child = execution.child(Instant::now() + Duration::from_secs(1));
        assert!(child.deadline().unwrap() < execution.deadline().unwrap());
        child.cancel();
        tokio::time::timeout(Duration::from_millis(100), child.cancelled())
            .await
            .unwrap();
        assert!(child.is_cancelled());
        assert!(!execution.is_cancelled());
        assert!(child.remaining().unwrap() > Duration::ZERO);
    }

    #[tokio::test]
    async fn context_deadline_wakes_without_an_execution_guard_or_background_timer() {
        let control = ExecutionControl::new(Limits {
            wall_time: Duration::from_millis(10),
            ..Limits::default()
        });
        let context = control.host_context();
        tokio::time::timeout(Duration::from_secs(1), context.cancelled())
            .await
            .unwrap();
        assert!(context.is_cancelled());
    }
}
