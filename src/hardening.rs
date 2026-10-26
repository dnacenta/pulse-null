//! Clear the dumpable flag.
//!
//! A dumpable process lets the same user read its memory. The daemon clears
//! the flag at boot, and receipts clear it too. A second call leaves it
//! cleared. Exec of another program sets the flag back to 1.
//!
//! Tests hold the process lock around the change and keep a guard. Dropping
//! the last guard restores the ordinary value, so a test process does not
//! stay non-dumpable.

#[cfg(test)]
use std::sync::Arc;

/// Owns the restore guard in tests. Empty when tests are off.
#[derive(Clone)]
pub struct DumpableSlot {
    /// Nothing reads this. Dropping the last clone restores the flag.
    #[cfg(test)]
    #[expect(
        dead_code,
        reason = "dropped with the slot to restore the dumpable flag"
    )]
    inner: Option<Arc<DumpableRestore>>,
}

impl DumpableSlot {
    /// No flag was cleared. Dropping this restores nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            #[cfg(test)]
            inner: None,
        }
    }
}

/// Clear the dumpable flag. One warning, and the caller continues, if the
/// call fails.
///
/// In tests this arms a restore guard. [`take_slot`] on this same thread,
/// before the task awaits, moves that guard out of thread-local storage.
pub fn set_non_dumpable() {
    #[cfg(test)]
    let _hold = crate::process_state::Hold::acquire();
    // SAFETY: `PR_SET_DUMPABLE` takes an integer and no pointer. Zero clears
    // the flag for this process.
    let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if rc != 0 {
        tracing::warn!(
            "could not clear the dumpable flag: {}",
            std::io::Error::last_os_error()
        );
    }
    #[cfg(test)]
    arm_restore();
}

/// Clear the flag and take the test guard before any await.
#[must_use]
pub fn own_non_dumpable() -> DumpableSlot {
    set_non_dumpable();
    take_slot()
}

/// Take the guard [`set_non_dumpable`] armed on this thread.
#[must_use]
pub fn take_slot() -> DumpableSlot {
    DumpableSlot {
        #[cfg(test)]
        inner: take_guard(),
    }
}

#[cfg(test)]
fn arm_restore() {
    RESTORE.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Arc::new(DumpableRestore));
        }
    });
}

#[cfg(test)]
fn take_guard() -> Option<Arc<DumpableRestore>> {
    RESTORE.with(|slot| slot.borrow_mut().take())
}

/// Restores the dumpable flag when the last clone drops.
#[cfg(test)]
struct DumpableRestore;

#[cfg(test)]
impl Drop for DumpableRestore {
    fn drop(&mut self) {
        let _hold = crate::process_state::Hold::acquire();
        // SAFETY: integer flag only. 1 is the ordinary value for this process.
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) };
    }
}

#[cfg(test)]
thread_local! {
    static RESTORE: std::cell::RefCell<Option<Arc<DumpableRestore>>> =
        const { std::cell::RefCell::new(None) };
}
