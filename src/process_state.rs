//! Process-wide state that tests share: the receipt flock, a spawned child,
//! an environment variable, the dumpable flag.
//!
//! A child inherits every descriptor that is not close-on-exec, including a
//! receipt file a parallel test has locked. Holding this lock across that
//! window and across the fork keeps the two apart. The lock is re-entrant
//! on the thread that already holds it. Drop it on that same thread.

/// Run `body` while this thread holds the process lock.
///
/// The lock is released when `body` returns, so do not await inside it.
/// Outside tests this only calls `body`.
pub fn during_fork<T>(body: impl FnOnce() -> T) -> T {
    #[cfg(test)]
    let _hold = Hold::acquire();
    body()
}

/// The process lock, held by the outermost acquire on this thread.
#[cfg(test)]
pub struct Hold {
    guard: Option<std::sync::MutexGuard<'static, ()>>,
}

#[cfg(test)]
impl Hold {
    /// Take the lock, or share the one this thread already holds.
    #[must_use]
    pub fn acquire() -> Self {
        DEPTH.with(|depth| {
            let current = depth.get();
            depth.set(current.saturating_add(1));
            if current == 0 {
                let guard = PROCESS.lock().unwrap_or_else(|err| err.into_inner());
                Self { guard: Some(guard) }
            } else {
                Self { guard: None }
            }
        })
    }
}

#[cfg(test)]
impl Drop for Hold {
    fn drop(&mut self) {
        DEPTH.with(|depth| {
            let current = depth.get();
            depth.set(current.saturating_sub(1));
        });
        drop(self.guard.take());
    }
}

#[cfg(test)]
static PROCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
thread_local! {
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}
