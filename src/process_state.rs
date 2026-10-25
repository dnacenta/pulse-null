//! Process-wide state that tests share: the receipt flock, a spawned child,
//! an environment variable, the dumpable flag.
//!
//! A child inherits every descriptor that is not close-on-exec, including a
//! receipt file a parallel test has locked. Holding this lock across that
//! window and across the fork keeps the two apart. The lock is re-entrant
//! on the thread that already holds it. Drop it on that same thread.

/// How long a thread waits for the process lock before panicking.
///
/// A holder must not wait on a worker that needs the same lock. The wait
/// ends in a panic so that mistake cannot hang the suite.
#[cfg(test)]
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Run `body` while this thread holds the process lock.
///
/// The lock is released when `body` returns, so do not await inside it.
/// A holder must not wait on a worker that needs the same lock. Outside
/// tests this only calls `body`.
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
    ///
    /// A holder must not wait on a worker that needs the same lock. A thread
    /// that does not already hold it tries until [`LOCK_WAIT`] and then
    /// panics, instead of blocking with no deadline.
    #[must_use]
    pub fn acquire() -> Self {
        DEPTH.with(|depth| {
            let current = depth.get();
            if current == 0 {
                let guard = wait_for_lock();
                depth.set(1);
                Self { guard: Some(guard) }
            } else {
                depth.set(current.saturating_add(1));
                Self { guard: None }
            }
        })
    }
}

#[cfg(test)]
fn wait_for_lock() -> std::sync::MutexGuard<'static, ()> {
    let wait = WAIT_OVERRIDE.with(|slot| slot.get().unwrap_or(LOCK_WAIT));
    let deadline = std::time::Instant::now() + wait;
    loop {
        match PROCESS.try_lock() {
            Ok(guard) => return guard,
            Err(std::sync::TryLockError::Poisoned(err)) => return err.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    panic!(
                        "this thread waited on the process lock for {wait:?}; a holder must not wait on a worker that needs the same lock"
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
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
    static WAIT_OVERRIDE: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Run this test's body in a fresh process.
///
/// Returns true only in that process. The parent spawns the test binary with
/// this test alone, waits, and returns false. A scoped tracing subscriber in
/// the child is not racing another test on the same callsite.
#[cfg(test)]
#[must_use]
pub fn isolated_test(test_name: &str) -> bool {
    let key = "PN108_ISOLATED_TEST";
    if std::env::var(key).ok().as_deref() == Some(test_name) {
        return true;
    }
    let exe = std::env::current_exe().expect("the test executable");
    let child = during_fork(|| {
        std::process::Command::new(&exe)
            .arg(test_name)
            .arg("--test-threads=1")
            .env(key, test_name)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("could not start {test_name}: {err}"))
    });
    let output = child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("{test_name}: {err}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{test_name} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "{test_name} did not run alone\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_on_the_process_lock_panics_instead_of_hanging() {
        let hold = Hold::acquire();
        let worker = std::thread::spawn(|| {
            WAIT_OVERRIDE.with(|slot| slot.set(Some(std::time::Duration::from_millis(40))));
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _wait = Hold::acquire();
            }))
        });
        let result = worker.join().unwrap();
        let payload = result.expect_err("the wait should panic");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(message.contains("must not wait on a worker"), "{message}");
        drop(hold);
    }
}
