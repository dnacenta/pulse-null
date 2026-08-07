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

/// The process-wide lease table. One WAL lock per process, so every consumer
/// (leadership loop, task loops, intent drain) shares this handle.
pub type SharedLeases = Arc<tokio::sync::Mutex<DurableLeaseTable>>;

/// A tenure's claim-taking identity: the shared lease table plus the
/// tenure-scoped holder id every claim of this tenure is made under.
#[derive(Clone)]
pub struct TenureLeases {
    pub leases: SharedLeases,
    pub holder: String,
}

/// Sanitize an arbitrary string into the lease id charset.
pub(crate) fn lease_safe(input: &str) -> String {
    let sanitized: String = input
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
    if sanitized.is_empty() {
        "unnamed".to_string()
    } else {
        sanitized
    }
}

/// Handle to the coordinator's leadership loop.
pub struct Coordinator {
    shutdown_tx: watch::Sender<bool>,
    handle: Option<JoinHandle<()>>,
    /// The current tenure's scheduler tasks. Owned here — not on the
    /// leadership loop's stack — so they can be aborted even if the loop
    /// itself dies abruptly (panic, abort, drop): a stale tenure must never
    /// keep acting.
    scheduler_handles: SharedHandles,
}

type SharedHandles = Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>;

fn abort_all(handles: &SharedHandles) {
    let mut guard = handles.lock().unwrap_or_else(|p| p.into_inner());
    for handle in guard.drain(..) {
        handle.abort();
    }
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
        let scheduler_handles: SharedHandles = Arc::new(std::sync::Mutex::new(Vec::new()));
        if !state.config.scheduler.enabled {
            tracing::info!("Scheduler disabled in config — coordinator not started");
            return Self {
                shutdown_tx,
                handle: None,
                scheduler_handles,
            };
        }
        let handle = tokio::spawn(leadership_loop(
            state,
            schedule,
            intent_queue,
            shutdown_rx,
            Arc::clone(&scheduler_handles),
        ));
        Self {
            shutdown_tx,
            handle: Some(handle),
            scheduler_handles,
        }
    }

    /// Simulate a coordinator wedge/crash: the leadership loop dies without
    /// releasing the lease or stopping anything. Fail-open tests use this to
    /// prove the data plane doesn't care.
    #[cfg(test)]
    pub(crate) fn wedge_for_test(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }

    /// Abort the scheduler tasks and release the lease. Bounded: gives the
    /// loop 10s to wind down before letting go. Scheduler handles are
    /// aborted here too as a backstop — even if the loop is already dead.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(handle) = self.handle.take() {
            if tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .is_err()
            {
                tracing::warn!("coordinator: shutdown timed out after 10s");
            }
        }
        abort_all(&self.scheduler_handles);
    }
}

impl Drop for Coordinator {
    /// Dropping the coordinator (registry overwrite, early boot error) must
    /// not orphan a running scheduler. The watch sender's drop also resolves
    /// `changed()` in the loop, which treats it as shutdown.
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        abort_all(&self.scheduler_handles);
    }
}

enum TenureEnd {
    Shutdown,
    Lost,
    /// Isolation Mode entered — release everything and park until it exits.
    Isolated,
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
pub(crate) fn holder_id(entity_name: &str) -> String {
    format!("{}-{}", lease_safe(entity_name), std::process::id())
}

async fn leadership_loop(
    state: Arc<AppState>,
    schedule: Arc<RwLock<Schedule>>,
    intent_queue: Arc<RwLock<IntentQueue>>,
    mut shutdown_rx: watch::Receiver<bool>,
    scheduler_handles: SharedHandles,
) {
    let dir = state.root_dir.join("coordinator");
    let holder_base = holder_id(&state.config.entity.name);
    // Tenure-scoped holder ids: without the suffix, a predecessor tenure's
    // leftover claim would be indistinguishable from ours, and neither the
    // stale-claim sweep nor fenced completion could tell them apart.
    let mut tenure: u64 = 0;
    let mut failed_acquires: u32 = 0;
    let mut parked_logged = false;

    'open: loop {
        if *shutdown_rx.borrow() {
            return;
        }
        // Opening takes the WAL's exclusive file lock — held for as long as
        // this table lives, so it spans tenures and re-acquire attempts.
        // Shared: task loops and the intent drain claim their leases here too.
        let leases: SharedLeases = match DurableLeaseTable::open(&dir) {
            Ok(t) => Arc::new(tokio::sync::Mutex::new(t)),
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
            // Isolation Mode (spec Stage 2): the control plane is shed. Park —
            // no acquire, no scheduler — until the data plane exits isolation.
            // The marker is file-based and owned by the channel holder, so
            // this check works no matter what state the rest of us are in.
            if crate::server::isolation::is_active(&state.root_dir) {
                if !parked_logged {
                    tracing::warn!(
                        "coordinator: ISOLATION active — control plane parked (scheduler down)                          until /resume"
                    );
                    parked_logged = true;
                }
                if wait_or_shutdown(&mut shutdown_rx, RETRY_BACKOFF).await {
                    return;
                }
                continue;
            }
            if parked_logged {
                tracing::info!("coordinator: isolation exited — resuming control plane");
                parked_logged = false;
            }
            tenure += 1;
            let holder = format!("{holder_base}-t{tenure}");
            let acquired = {
                leases
                    .lock()
                    .await
                    .acquire(CONTROL_PLANE_RESOURCE, &holder, LEASE_TTL, Utc::now())
            };
            let lease = match acquired {
                Ok(lease) => lease,
                Err(DurableError::Lease(LeaseError::Held {
                    holder_id,
                    expires_at,
                    ..
                })) => {
                    // A crashed predecessor's lease runs out on its own; a
                    // live one keeps renewing. Either way: wait, retry.
                    failed_acquires += 1;
                    if failed_acquires.is_multiple_of(8) {
                        tracing::error!(
                            "coordinator: control plane STILL held by '{holder_id}' after \
                             {failed_acquires} attempts (until {expires_at}) — scheduler is down"
                        );
                    } else {
                        tracing::info!(
                            "coordinator: control plane held by '{holder_id}' until {expires_at}; waiting"
                        );
                    }
                    if wait_or_shutdown(&mut shutdown_rx, RETRY_BACKOFF).await {
                        return;
                    }
                    continue;
                }
                Err(e) => {
                    failed_acquires += 1;
                    if failed_acquires.is_multiple_of(8) {
                        tracing::error!(
                            "coordinator: lease acquire STILL failing after {failed_acquires} \
                             attempts ({e}) — scheduler is down"
                        );
                    } else {
                        tracing::warn!("coordinator: lease acquire failed ({e}); retrying");
                    }
                    if wait_or_shutdown(&mut shutdown_rx, RETRY_BACKOFF).await {
                        return;
                    }
                    continue;
                }
            };
            failed_acquires = 0;

            state
                .leadership
                .store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(
                "coordinator: control-plane leadership acquired as '{holder}' \
                 (token {}, ttl {LEASE_TTL:?})",
                lease.fencing_token
            );

            // Sweep claims left by dead tenures (a crash mid-task, a lost
            // tenure's aborted loops). Safe by construction: no scheduler is
            // running right now — this tenure hasn't started one, and the
            // previous tenure's tasks were aborted before we got here — so
            // any task-/intent- lease in the table is an orphan. Without the
            // sweep, a daily task whose run died mid-flight would have its
            // next fire refused (up to the 30m claim ttl — or the whole day,
            // since the cron decides when the retry happens).
            sweep_stale_claims(&leases, &holder).await;

            // Reconcile-on-reconnect (spec decision 2): other writers — the
            // CLI, the event listener, a prior wedged tenure — may have
            // changed control-plane state while we weren't leader. Re-read
            // the actual files; never assume the in-memory copies. (Evaluator
            // state and task health are re-loaded from disk inside the
            // scheduler itself, per tenure.)
            if let Err(e) = reconcile_control_plane(&state.root_dir, &schedule, &intent_queue).await
            {
                // Unknown control-plane state: fail closed on the control
                // plane only. Release and retry; the data plane keeps serving.
                tracing::error!("coordinator: reconcile failed ({e}); not starting scheduler");
                let _ = leases
                    .lock()
                    .await
                    .release(CONTROL_PLANE_RESOURCE, &holder, Utc::now());
                if wait_or_shutdown(&mut shutdown_rx, RETRY_BACKOFF).await {
                    return;
                }
                continue;
            }

            let started = match scheduler::start(
                Arc::clone(&state),
                Arc::clone(&schedule),
                Arc::clone(&intent_queue),
                Arc::clone(&leases),
                holder.clone(),
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
                    let _ =
                        leases
                            .lock()
                            .await
                            .release(CONTROL_PLANE_RESOURCE, &holder, Utc::now());
                    return;
                }
            };

            {
                let mut guard = scheduler_handles.lock().unwrap_or_else(|p| p.into_inner());
                *guard = started;
            }

            let tenure_end =
                renew_until_lost_or_shutdown(&leases, &holder, &state.root_dir, &mut shutdown_rx)
                    .await;
            state
                .leadership
                .store(false, std::sync::atomic::Ordering::Relaxed);
            tracing::info!("coordinator: ending tenure '{holder}', aborting scheduler task(s)");
            abort_all(&scheduler_handles);

            match tenure_end {
                TenureEnd::Shutdown => {
                    if let Err(e) =
                        leases
                            .lock()
                            .await
                            .release(CONTROL_PLANE_RESOURCE, &holder, Utc::now())
                    {
                        tracing::warn!("coordinator: lease release on shutdown failed: {e}");
                    }
                    return;
                }
                TenureEnd::Lost => {
                    // If the renewal only failed transiently (we are still
                    // the recorded holder), release now so the re-acquire
                    // doesn't wait out our own ttl.
                    let _ =
                        leases
                            .lock()
                            .await
                            .release(CONTROL_PLANE_RESOURCE, &holder, Utc::now());
                    tracing::warn!("coordinator: leadership lost; will re-acquire");
                }
                TenureEnd::Isolated => {
                    // Release so the lease is provably free while isolated
                    // (AC19), then loop back into the parking check.
                    let _ =
                        leases
                            .lock()
                            .await
                            .release(CONTROL_PLANE_RESOURCE, &holder, Utc::now());
                }
            }
        }
    }
}

/// Release every task-/intent- claim in the table. Called at leadership
/// acquisition, when provably no scheduler is running — every such claim
/// belongs to a dead tenure and would otherwise block that resource's next
/// fire for up to its remaining ttl.
async fn sweep_stale_claims(leases: &SharedLeases, tenure_holder: &str) {
    let mut table = leases.lock().await;
    let stale: Vec<_> = table
        .leases()
        .into_iter()
        .filter(|l| {
            l.holder_id != tenure_holder
                && (l.resource_id.starts_with("task-") || l.resource_id.starts_with("intent-"))
        })
        .collect();
    for lease in stale {
        match table.release(&lease.resource_id, &lease.holder_id, Utc::now()) {
            Ok(()) => tracing::warn!(
                "coordinator: swept stale claim '{}' held by dead tenure '{}'",
                lease.resource_id,
                lease.holder_id
            ),
            Err(e) => tracing::warn!(
                "coordinator: failed to sweep stale claim '{}': {e}",
                lease.resource_id
            ),
        }
    }
}

/// Replace the shared control-plane state with what is actually on disk.
async fn reconcile_control_plane(
    root_dir: &std::path::Path,
    schedule: &Arc<RwLock<Schedule>>,
    intent_queue: &Arc<RwLock<IntentQueue>>,
) -> Result<(), crate::errors::SchedulerError> {
    let fresh_schedule = Schedule::load(root_dir)?;
    let fresh_intents = IntentQueue::load(root_dir);
    *schedule.write().await = fresh_schedule;
    *intent_queue.write().await = fresh_intents;
    Ok(())
}

async fn renew_until_lost_or_shutdown(
    leases: &SharedLeases,
    holder: &str,
    root_dir: &std::path::Path,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> TenureEnd {
    loop {
        if wait_or_shutdown(shutdown_rx, RENEW_INTERVAL).await {
            return TenureEnd::Shutdown;
        }
        if crate::server::isolation::is_active(root_dir) {
            return TenureEnd::Isolated;
        }
        let renewed = {
            leases
                .lock()
                .await
                .renew(CONTROL_PLANE_RESOURCE, holder, LEASE_TTL, Utc::now())
        };
        match renewed {
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

    /// AC12: at leadership acquisition the shared state is replaced by disk
    /// truth — an edit made while the coordinator was down survives.
    #[tokio::test]
    async fn reconcile_replaces_shared_state_with_disk_truth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let schedule = Arc::new(RwLock::new(Schedule::load_or_init(root).unwrap()));
        let intents = Arc::new(RwLock::new(IntentQueue::load(root)));
        let task_id = schedule.read().await.tasks[0].task.id.clone();
        assert!(
            schedule
                .read()
                .await
                .find_task(&task_id)
                .unwrap()
                .task
                .enabled
        );

        // "CLI disabled the task while the coordinator was down."
        {
            let mut cli = Schedule::load(root).unwrap();
            cli.find_task_mut(&task_id).unwrap().task.enabled = false;
            cli.save(root).unwrap();
        }

        reconcile_control_plane(root, &schedule, &intents)
            .await
            .unwrap();
        assert!(
            !schedule
                .read()
                .await
                .find_task(&task_id)
                .unwrap()
                .task
                .enabled
        );
    }

    /// HIGH-1 regression: claims left by a dead tenure (crash mid-task,
    /// aborted loops) must not block the resource's next fire — the sweep at
    /// leadership acquisition releases them.
    #[tokio::test]
    async fn stale_claims_are_swept_at_leadership_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let leases: SharedLeases = Arc::new(tokio::sync::Mutex::new(
            DurableLeaseTable::open(dir.path()).unwrap(),
        ));

        // A dead tenure left claims behind (unexpired — mid side-effect
        // window when it died).
        {
            let mut t = leases.lock().await;
            t.acquire(
                "task-thinking-loop",
                "echo-1-t1",
                Duration::from_secs(1800),
                Utc::now(),
            )
            .unwrap();
            t.acquire(
                "intent-abc",
                "echo-1-t1",
                Duration::from_secs(1800),
                Utc::now(),
            )
            .unwrap();
            // Non-claim resources are never swept.
            t.acquire(
                CONTROL_PLANE_RESOURCE,
                "echo-1-t1",
                Duration::from_secs(90),
                Utc::now(),
            )
            .unwrap();
        }

        sweep_stale_claims(&leases, "echo-1-t2").await;

        let t = leases.lock().await;
        assert!(t.get("task-thinking-loop").is_none());
        assert!(t.get("intent-abc").is_none());
        assert!(t.get(CONTROL_PLANE_RESOURCE).is_some());
        drop(t);

        // The new tenure's fire claims immediately — no 30m wait.
        let claimed = leases.lock().await.acquire(
            "task-thinking-loop",
            "echo-1-t2",
            Duration::from_secs(1800),
            Utc::now(),
        );
        assert!(claimed.is_ok());
    }

    #[test]
    fn holder_id_is_lease_charset_safe() {
        let id = holder_id("Echo Prime (v2)!");
        assert!(id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')));
        assert!(id.starts_with("Echo-Prime--v2--"));
        let empty = holder_id("");
        assert!(empty.starts_with("unnamed-"));
    }
}
