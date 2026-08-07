//! Control-plane leadership: the coordinator owns the scheduler, never the
//! interactive faculties.
//!
//! The load-bearing separation (coordinator spec, decision 1): chat and voice
//! are the data plane and must stay reachable no matter what happens here.
//! This module therefore owns exactly the control plane — the scheduler task
//! loops, the intent drain, the liveness watchdog — and gates them on a
//! `control-plane` lease from the Stage 0 substrate. The axum router, the
//! session store, and every interactive handler are wired independently at
//! boot and never consult the coordinator.
//!
//! Leadership lifecycle:
//! - Acquire the `control-plane` lease (WAL-backed, fencing-tokened), then
//!   start the scheduler. The lease WAL's exclusive file lock already
//!   excludes a second coordinator *process*; the lease on top records the
//!   tenure durably and fences a paused predecessor.
//! - Renew at a third of the ttl. A failed renewal (e.g. the process stalled
//!   past expiry and someone reclaimed) aborts the scheduler tasks and drops
//!   back to acquiring — the stale tenure never keeps acting.
//! - After a crash, a successor waits out the previous lease's remaining ttl
//!   (bounded by `LEASE_TTL`) before taking over.
//! - If the scheduler itself fails to start (bad config), the coordinator
//!   logs, releases the lease, and exits — the data plane keeps serving.
//!   (Previously a scheduler start error aborted the whole boot, chat
//!   included; fail-open forbids that coupling.)
//!
//! Shutdown goes through `Coordinator::shutdown`, which aborts the scheduler
//! tasks and releases the lease so a successor does not have to wait out the
//! ttl.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{watch, RwLock};
use tokio::task::JoinHandle;

use super::durable::{DurableError, DurableLeaseTable};
use super::lease::LeaseError;
use crate::scheduler::{self, intent::IntentQueue, Schedule};
use crate::server::AppState;

pub const CONTROL_PLANE_RESOURCE: &str = "control-plane";
const LEASE_TTL: Duration = Duration::from_secs(90);
const RENEW_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_BACKOFF: Duration = Duration::from_secs(15);

/// Handle to the coordinator's leadership loop.
pub struct Coordinator {
    shutdown_tx: watch::Sender<bool>,
    handle: Option<JoinHandle<()>>,
}

impl Coordinator {
    /// Start the leadership loop. The scheduler will run only while the
    /// `control-plane` lease is held. `schedule`/`intent_queue` are shared
    /// with the event listener and CLI surfaces, so the coordinator refreshes
    /// their contents rather than replacing the `Arc`s.
    pub fn start(
        state: Arc<AppState>,
        schedule: Arc<RwLock<Schedule>>,
        intent_queue: Arc<RwLock<IntentQueue>>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        if !state.config.scheduler.enabled {
            tracing::info!("Scheduler disabled in config — coordinator not started");
            return Self {
                shutdown_tx,
                handle: None,
            };
        }
        let handle = tokio::spawn(leadership_loop(state, schedule, intent_queue, shutdown_rx));
        Self {
            shutdown_tx,
            handle: Some(handle),
        }
    }

    /// Abort the scheduler tasks and release the lease. Bounded: gives the
    /// loop 10s to wind down before letting go.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(handle) = self.handle {
            if tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .is_err()
            {
                tracing::warn!("coordinator: shutdown timed out after 10s");
            }
        }
    }
}

enum TenureEnd {
    Shutdown,
    Lost,
}

/// Sleep for `dur` unless shutdown fires first. Returns true on shutdown.
async fn wait_or_shutdown(shutdown_rx: &mut watch::Receiver<bool>, dur: Duration) -> bool {
    tokio::select! {
        _ = shutdown_rx.changed() => true,
        () = tokio::time::sleep(dur) => *shutdown_rx.borrow(),
    }
}

/// Lease holder id: entity name (sanitized to the lease id charset) + pid,
/// so concurrent processes are distinguishable in the lease WAL.
fn holder_id(entity_name: &str) -> String {
    let sanitized: String = entity_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let base = if sanitized.is_empty() {
        "entity".to_string()
    } else {
        sanitized
    };
    format!("{base}-{}", std::process::id())
}

async fn leadership_loop(
    state: Arc<AppState>,
    schedule: Arc<RwLock<Schedule>>,
    intent_queue: Arc<RwLock<IntentQueue>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let dir = state.root_dir.join("coordinator");
    let holder = holder_id(&state.config.entity.name);

    'open: loop {
        if *shutdown_rx.borrow() {
            return;
        }
        // Opening takes the WAL's exclusive file lock — held for as long as
        // this table lives, so it spans tenures and re-acquire attempts.
        let mut table = match DurableLeaseTable::open(&dir) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    "coordinator: lease table unavailable ({e}); retrying in {RETRY_BACKOFF:?}"
                );
                if wait_or_shutdown(&mut shutdown_rx, RETRY_BACKOFF).await {
                    return;
                }
                continue 'open;
            }
        };

        loop {
            if *shutdown_rx.borrow() {
                return;
            }
            let lease = match table.acquire(CONTROL_PLANE_RESOURCE, &holder, LEASE_TTL, Utc::now())
            {
                Ok(lease) => lease,
                Err(DurableError::Lease(LeaseError::Held {
                    holder_id,
                    expires_at,
                    ..
                })) => {
                    // A crashed predecessor's lease runs out on its own; a
                    // live one keeps renewing. Either way: wait, retry.
                    tracing::info!(
                        "coordinator: control plane held by '{holder_id}' until {expires_at}; waiting"
                    );
                    if wait_or_shutdown(&mut shutdown_rx, RETRY_BACKOFF).await {
                        return;
                    }
                    continue;
                }
                Err(e) => {
                    tracing::warn!("coordinator: lease acquire failed ({e}); retrying");
                    if wait_or_shutdown(&mut shutdown_rx, RETRY_BACKOFF).await {
                        return;
                    }
                    continue;
                }
            };

            tracing::info!(
                "coordinator: control-plane leadership acquired (token {}, ttl {LEASE_TTL:?})",
                lease.fencing_token
            );

            let handles = match scheduler::start(
                Arc::clone(&state),
                Arc::clone(&schedule),
                Arc::clone(&intent_queue),
            )
            .await
            {
                Ok(handles) => handles,
                Err(e) => {
                    // Bad scheduler config must not hot-loop and must not
                    // touch the data plane: give up the control plane and
                    // leave chat/voice serving.
                    tracing::error!(
                        "coordinator: scheduler failed to start ({e}); \
                         releasing control plane — interactive faculties unaffected"
                    );
                    let _ = table.release(CONTROL_PLANE_RESOURCE, &holder, Utc::now());
                    return;
                }
            };

            let tenure_end =
                renew_until_lost_or_shutdown(&mut table, &holder, &mut shutdown_rx).await;
            tracing::info!(
                "coordinator: ending tenure, aborting {} scheduler task(s)",
                handles.len()
            );
            for handle in &handles {
                handle.abort();
            }

            match tenure_end {
                TenureEnd::Shutdown => {
                    if let Err(e) = table.release(CONTROL_PLANE_RESOURCE, &holder, Utc::now()) {
                        tracing::warn!("coordinator: lease release on shutdown failed: {e}");
                    }
                    return;
                }
                TenureEnd::Lost => {
                    tracing::warn!("coordinator: leadership lost; will re-acquire");
                }
            }
        }
    }
}

async fn renew_until_lost_or_shutdown(
    table: &mut DurableLeaseTable,
    holder: &str,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> TenureEnd {
    loop {
        if wait_or_shutdown(shutdown_rx, RENEW_INTERVAL).await {
            return TenureEnd::Shutdown;
        }
        match table.renew(CONTROL_PLANE_RESOURCE, holder, LEASE_TTL, Utc::now()) {
            Ok(_) => {}
            Err(e) => {
                // Expired (we stalled past ttl), reclaimed by a successor,
                // or the WAL refused the write — in every case this tenure
                // is over and the scheduler must stop.
                tracing::warn!("coordinator: lease renewal failed ({e})");
                return TenureEnd::Lost;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_id_is_lease_charset_safe() {
        let id = holder_id("Echo Prime (v2)!");
        assert!(id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')));
        assert!(id.starts_with("Echo-Prime--v2--"));
        let empty = holder_id("");
        assert!(empty.starts_with("entity-"));
    }
}
