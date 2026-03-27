use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

/// Coordinates fire-and-forget persistence tasks.
///
/// Tracks in-flight `spawn_blocking` writes so the shutdown sequence can
/// wait for them to complete instead of dropping them on the floor.
#[derive(Debug)]
pub struct PersistCoordinator {
    in_flight: AtomicUsize,
    notify: Notify,
}

impl PersistCoordinator {
    pub fn new() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    /// Spawn a blocking persistence task, tracked by this coordinator.
    ///
    /// The closure runs on tokio's blocking thread pool. The coordinator
    /// increments its counter before spawning and decrements it (with
    /// notification) when the task completes, regardless of success or panic.
    pub fn track<F>(self: &Arc<Self>, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let coord = Arc::clone(self);

        #[allow(clippy::let_underscore_future)]
        let _ = tokio::task::spawn_blocking(move || {
            // Run the actual persistence work.
            // Use a drop guard so the counter is decremented even on panic.
            let _guard = DropGuard(coord);
            f();
        });
    }

    /// Wait until all in-flight persistence tasks complete, or until `timeout`
    /// expires. Returns `true` if all tasks completed, `false` on timeout.
    pub async fn flush(&self, timeout: Duration) -> bool {
        if self.in_flight.load(Ordering::SeqCst) == 0 {
            return true;
        }

        tokio::select! {
            _ = self.wait_for_zero() => true,
            _ = tokio::time::sleep(timeout) => {
                let remaining = self.in_flight.load(Ordering::SeqCst);
                if remaining > 0 {
                    tracing::warn!(
                        "PersistCoordinator: flush timed out with {} task(s) still in flight",
                        remaining
                    );
                }
                remaining == 0
            }
        }
    }

    /// Spin on notifications until the counter hits zero.
    ///
    /// The `Notified` future is created *before* checking the counter to avoid
    /// a race where a task completes (and calls `notify_waiters`) between the
    /// load and the await.  `enable()` registers interest so the notification
    /// is captured even if it fires before we reach `.await`.
    async fn wait_for_zero(&self) {
        loop {
            // Register interest FIRST — before reading the counter.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Current number of in-flight tasks (for diagnostics).
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }
}

/// Drop guard that decrements the counter and notifies waiters.
struct DropGuard(Arc<PersistCoordinator>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn flush_returns_true_when_empty() {
        let coord = Arc::new(PersistCoordinator::new());
        assert!(coord.flush(Duration::from_millis(10)).await);
    }

    #[tokio::test]
    async fn tracks_and_completes() {
        let coord = Arc::new(PersistCoordinator::new());
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_clone = Arc::clone(&flag);

        coord.track(move || {
            std::thread::sleep(Duration::from_millis(50));
            flag_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        assert!(coord.in_flight_count() > 0 || flag.load(std::sync::atomic::Ordering::SeqCst));

        let flushed = coord.flush(Duration::from_secs(2)).await;
        assert!(flushed);
        assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(coord.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn flush_waits_for_multiple_tasks() {
        let coord = Arc::new(PersistCoordinator::new());
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..5 {
            let c = Arc::clone(&counter);
            coord.track(move || {
                std::thread::sleep(Duration::from_millis(30));
                c.fetch_add(1, Ordering::SeqCst);
            });
        }

        let flushed = coord.flush(Duration::from_secs(5)).await;
        assert!(flushed);
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn flush_times_out_gracefully() {
        let coord = Arc::new(PersistCoordinator::new());

        coord.track(move || {
            std::thread::sleep(Duration::from_secs(10)); // Very long task
        });

        // Very short timeout — should fail
        let flushed = coord.flush(Duration::from_millis(10)).await;
        assert!(!flushed);
    }
}
