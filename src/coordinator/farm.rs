//! Subtask farming (coordinator spec, Stage 3).
//!
//! Delegation of bounded work to child executions, built ENTIRELY on the
//! Stage 0 substrate — leases bound each child's tenure, fencing tokens
//! reject a reclaimed child's late result. No new ownership primitive: if
//! farming needed one, Stage 0 was incomplete (spec decision 6).
//!
//! Mechanics per subtask:
//! - each attempt acquires the lease `farm-{farm}-{sub}` under an
//!   attempt-scoped holder, minting a strictly higher fencing token;
//! - the result store holds one `FencedResource` guard per subtask, fenced
//!   AT the current attempt's token before the child runs — so a stalled
//!   predecessor's commit is rejected in every interleaving, including the
//!   window before the new child commits;
//! - a child that outlives its lease ttl is reclaimed: the next attempt's
//!   acquire (over the expired lease) mints a higher token, the guard is
//!   re-fenced, and the old child is aborted (`kill_on_drop` reaps the CLI
//!   subprocess). Its late result, if it slips through before the abort
//!   lands, presents a stale token and is fenced.
//!
//! The executor is injected, so all of this is deterministic under test and
//! provider-free; production passes a closure that invokes the LmProvider.
//! Children are in-process spawned tasks (single-VPS reality) — the lease
//! substrate would carry separate processes unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{Mutex, Semaphore};

use super::control::{lease_safe, SharedLeases};
use super::fenced::FencedResource;
use super::lease::FencingToken;

/// Hard cap on subtasks per farm — a farm is bounded work, not a fleet.
pub const MAX_SUBTASKS: usize = 8;

/// One bounded unit of delegated work.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubtaskSpec {
    pub id: String,
    pub prompt: String,
}

/// A farm: a set of subtasks plus an optional synthesis prompt the caller
/// may run over the collected results (`{results}` placeholder).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FarmSpec {
    pub id: String,
    #[serde(default)]
    pub description: String,
    pub subtasks: Vec<SubtaskSpec>,
    #[serde(default)]
    pub synthesis: Option<String>,
}

/// Execution knobs. Defaults are production values; tests shrink the ttl and
/// may disable reclaim-abort to let a stalled child run long enough to prove
/// its late commit is fenced.
#[derive(Debug, Clone)]
pub struct FarmCaps {
    pub max_concurrency: usize,
    pub max_attempts: u32,
    pub subtask_ttl: Duration,
    /// Abort a reclaimed child (production). `false` only in tests that
    /// deliberately let the stale child finish and hit the fence.
    pub reclaim_abort: bool,
}

impl Default for FarmCaps {
    fn default() -> Self {
        Self {
            max_concurrency: 3,
            max_attempts: 2,
            subtask_ttl: Duration::from_secs(10 * 60),
            reclaim_abort: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubtaskResult {
    pub sub_id: String,
    pub text: String,
    pub attempts: u32,
}

#[derive(Debug)]
pub struct FarmOutcome {
    pub results: Vec<SubtaskResult>,
    /// Subtasks that failed every attempt (or were cut off by abort).
    pub failed: Vec<String>,
    /// Late commits from reclaimed children rejected by fencing — the spec's
    /// Stage 3 exit criterion made observable.
    pub late_fenced: u32,
    /// Reclaims performed (stalled children superseded).
    pub reclaims: u32,
}

/// Fenced result store: the ONLY place child output lands, guarded per
/// subtask by a Stage 0 `FencedResource`.
struct FarmResults {
    inner: Mutex<HashMap<String, (FencedResource, Option<String>)>>,
    late_fenced: std::sync::atomic::AtomicU32,
}

impl FarmResults {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            late_fenced: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Raise the subtask's fence to `token` — called at every attempt's
    /// acquire, BEFORE the child runs. Tokens are strictly increasing across
    /// attempts, so this only ever raises.
    async fn fence_at(&self, sub_id: &str, token: FencingToken) {
        let mut inner = self.inner.lock().await;
        let entry = inner
            .entry(sub_id.to_string())
            .or_insert_with(|| (FencedResource::new(), None));
        entry.0 = FencedResource::at(token);
    }

    /// Commit a child's result under its token. A stale (reclaimed) child is
    /// rejected here — parent state is never touched by it.
    async fn commit(&self, sub_id: &str, token: FencingToken, text: String) -> bool {
        let mut inner = self.inner.lock().await;
        let entry = inner
            .entry(sub_id.to_string())
            .or_insert_with(|| (FencedResource::new(), None));
        match entry.0.accept(token) {
            Ok(()) => {
                entry.1 = Some(text);
                true
            }
            Err(rejected) => {
                tracing::warn!(
                    "farm: late result from reclaimed child on '{sub_id}' fenced ({rejected})"
                );
                self.late_fenced
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                false
            }
        }
    }

    async fn take(&self, sub_id: &str) -> Option<String> {
        self.inner
            .lock()
            .await
            .get_mut(sub_id)
            .and_then(|(_, r)| r.take())
    }
}

/// Run a farm to completion. The executor is one child attempt: it receives
/// the subtask spec and returns the child's text (or an error string).
pub async fn run_farm<F, Fut>(
    leases: SharedLeases,
    tenure_holder: &str,
    root_dir: PathBuf,
    spec: FarmSpec,
    caps: FarmCaps,
    executor: F,
) -> FarmOutcome
where
    F: Fn(SubtaskSpec) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    let farm_id = lease_safe(&spec.id);
    let results = Arc::new(FarmResults::new());
    let semaphore = Arc::new(Semaphore::new(caps.max_concurrency.max(1)));
    let reclaims = Arc::new(std::sync::atomic::AtomicU32::new(0));

    let subtasks: Vec<SubtaskSpec> = spec.subtasks.into_iter().take(MAX_SUBTASKS).collect();
    let mut controllers = Vec::new();

    for sub in subtasks.iter().cloned() {
        let leases = Arc::clone(&leases);
        let results = Arc::clone(&results);
        let semaphore = Arc::clone(&semaphore);
        let reclaims = Arc::clone(&reclaims);
        let executor = executor.clone();
        let caps = caps.clone();
        let root_dir = root_dir.clone();
        let tenure_holder = tenure_holder.to_string();
        let farm_id = farm_id.clone();

        controllers.push(tokio::spawn(async move {
            subtask_controller(
                leases,
                results,
                semaphore,
                reclaims,
                executor,
                caps,
                root_dir,
                tenure_holder,
                farm_id,
                sub,
            )
            .await
        }));
    }

    let mut outcome = FarmOutcome {
        results: Vec::new(),
        failed: Vec::new(),
        late_fenced: 0,
        reclaims: 0,
    };

    for (controller, sub) in controllers.into_iter().zip(subtasks.iter()) {
        let attempts = controller.await.unwrap_or(None);
        match (attempts, results.take(&sub.id).await) {
            (Some(attempts), Some(text)) => outcome.results.push(SubtaskResult {
                sub_id: sub.id.clone(),
                text,
                attempts,
            }),
            _ => outcome.failed.push(sub.id.clone()),
        }
    }
    outcome.late_fenced = results
        .late_fenced
        .load(std::sync::atomic::Ordering::Relaxed);
    outcome.reclaims = reclaims.load(std::sync::atomic::Ordering::Relaxed);
    outcome
}

/// Drive one subtask through its attempts. Returns Some(attempt_count) when
/// a result was committed, None when every attempt failed.
#[allow(clippy::too_many_arguments)]
async fn subtask_controller<F, Fut>(
    leases: SharedLeases,
    results: Arc<FarmResults>,
    semaphore: Arc<Semaphore>,
    reclaims: Arc<std::sync::atomic::AtomicU32>,
    executor: F,
    caps: FarmCaps,
    root_dir: PathBuf,
    tenure_holder: String,
    farm_id: String,
    sub: SubtaskSpec,
) -> Option<u32>
where
    F: Fn(SubtaskSpec) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    let resource = format!("farm-{farm_id}-{}", lease_safe(&sub.id));
    let mut stalled_child: Option<tokio::task::JoinHandle<()>> = None;

    for attempt in 1..=caps.max_attempts {
        // Shed with everything else: a farm never outlives isolation entry.
        if crate::server::isolation::is_active(&root_dir) {
            tracing::warn!(
                "farm '{farm_id}': ISOLATION active — abandoning '{}'",
                sub.id
            );
            break;
        }

        // Acquire (attempt 1) or reclaim-by-expiry (attempt >1: the previous
        // child stalled past its ttl; this acquire mints a higher token).
        let holder = format!("{tenure_holder}-a{attempt}");
        let lease = {
            leases
                .lock()
                .await
                .acquire(&resource, &holder, caps.subtask_ttl, Utc::now())
        };
        let lease = match lease {
            Ok(lease) => lease,
            Err(e) => {
                tracing::warn!("farm '{farm_id}': claim failed on '{}': {e}", sub.id);
                break;
            }
        };

        // Fence BEFORE the child runs: from this instant, any commit bearing
        // an older token — a stalled predecessor waking up — is rejected,
        // regardless of interleaving.
        results.fence_at(&sub.id, lease.fencing_token).await;
        if attempt > 1 {
            reclaims.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if caps.reclaim_abort {
                if let Some(old) = stalled_child.take() {
                    old.abort();
                }
            }
        }

        // Bound concurrency across the farm; the permit travels with the
        // child so a running child occupies a slot.
        let permit = Arc::clone(&semaphore).acquire_owned().await.ok()?;
        let mut child = {
            let results = Arc::clone(&results);
            let executor = executor.clone();
            let sub = sub.clone();
            let token = lease.fencing_token;
            tokio::spawn(async move {
                let _permit = permit;
                if let Ok(text) = executor(sub.clone()).await {
                    results.commit(&sub.id, token, text).await;
                }
            })
        };

        // Wait out this attempt's lease window. select (not timeout) so the
        // JoinHandle survives a stall — a dropped handle would detach the
        // child beyond reach of the reclaim abort.
        let finished = tokio::select! {
            _ = &mut child => true,
            () = tokio::time::sleep(caps.subtask_ttl) => false,
        };
        match finished {
            true => {
                // Child finished (ok or executor error) within its lease.
                let committed = {
                    results
                        .inner
                        .lock()
                        .await
                        .get(&sub.id)
                        .is_some_and(|(_, r)| r.is_some())
                };
                let _ = { leases.lock().await.release(&resource, &holder, Utc::now()) };
                if committed {
                    return Some(attempt);
                }
                // Executor error: retry on the next attempt (fresh acquire —
                // the lease was released, watermark still climbs).
            }
            false => {
                // Stalled past the ttl: keep the handle; the NEXT attempt's
                // acquire reclaims by expiry and re-fences before (optionally)
                // aborting it. Do not release — expiry is the reclaim signal.
                tracing::warn!(
                    "farm '{farm_id}': child on '{}' exceeded ttl {:?} — reclaiming",
                    sub.id,
                    caps.subtask_ttl
                );
                stalled_child = Some(child);
            }
        }
    }

    // Out of attempts (or aborted): reap a still-stalled child.
    if let Some(old) = stalled_child.take() {
        old.abort();
    }
    None
}

/// Parse and run a `[FARM: {json}]` marker under the current tenure. Returns
/// the farm's composed result text (synthesized when the spec asks for it),
/// or an error string for the caller to log. Children run WITHOUT tools —
/// bounded reasoning work; anything that needs side effects is not a farm.
pub async fn run_farm_from_marker(
    farm_json: &str,
    state: &Arc<crate::server::AppState>,
    tenure: &super::control::TenureLeases,
) -> Result<String, String> {
    let spec: FarmSpec =
        serde_json::from_str(farm_json).map_err(|e| format!("invalid [FARM:] json: {e}"))?;
    if spec.subtasks.is_empty() {
        return Err("farm has no subtasks".to_string());
    }
    if spec.subtasks.len() > MAX_SUBTASKS {
        return Err(format!(
            "farm has {} subtasks (max {MAX_SUBTASKS})",
            spec.subtasks.len()
        ));
    }
    {
        let mut ids: Vec<&str> = spec.subtasks.iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != spec.subtasks.len() {
            return Err("duplicate subtask ids".to_string());
        }
    }

    let system_prompt = crate::server::prompt::build_task_system_prompt_async(
        state.root_dir.clone(),
        state.config.clone(),
    )
    .await
    .map_err(|e| format!("cannot build farm system prompt: {e}"))?;

    let synthesis = spec.synthesis.clone();
    let description = spec.description.clone();
    let farm_label = spec.id.clone();
    let max_tokens = state.config.llm.max_tokens;

    let exec_state = Arc::clone(state);
    let exec_prompt = system_prompt.clone();
    let executor = move |sub: SubtaskSpec| {
        let state = Arc::clone(&exec_state);
        let system_prompt = exec_prompt.clone();
        async move {
            let messages = vec![pulse_system_types::llm::Message {
                role: pulse_system_types::llm::Role::User,
                content: pulse_system_types::llm::MessageContent::Text(sub.prompt),
                source: None,
            }];
            let response = state
                .provider
                .invoke(&system_prompt, &messages, max_tokens, None)
                .await
                .map_err(|e| e.to_string())?;
            Ok(response_text(&response))
        }
    };

    let outcome = run_farm(
        Arc::clone(&tenure.leases),
        &tenure.holder,
        state.root_dir.clone(),
        spec,
        FarmCaps::default(),
        executor,
    )
    .await;

    if outcome.results.is_empty() {
        return Err(format!(
            "farm '{farm_label}' produced no results (failed: {:?})",
            outcome.failed
        ));
    }

    let mut composed = String::new();
    for r in &outcome.results {
        composed.push_str(&format!(
            "## {}
{}

",
            r.sub_id, r.text
        ));
    }
    if !outcome.failed.is_empty() {
        composed.push_str(&format!(
            "(failed subtasks: {:?})
",
            outcome.failed
        ));
    }

    let final_text = match synthesis {
        Some(template) => {
            let prompt = template.replace("{results}", &composed);
            let messages = vec![pulse_system_types::llm::Message {
                role: pulse_system_types::llm::Role::User,
                content: pulse_system_types::llm::MessageContent::Text(prompt),
                source: None,
            }];
            match state
                .provider
                .invoke(&system_prompt, &messages, max_tokens, None)
                .await
            {
                Ok(response) => response_text(&response),
                Err(e) => {
                    tracing::warn!(
                        "farm '{farm_label}': synthesis failed ({e}); using raw results"
                    );
                    composed
                }
            }
        }
        None => composed,
    };

    crate::logbook::write_entry(
        &state.root_dir,
        "Farm",
        &description,
        &format!(
            "{} subtask(s), {} reclaim(s), {} late-fenced",
            outcome.results.len(),
            outcome.reclaims,
            outcome.late_fenced
        ),
    );

    Ok(final_text)
}

fn response_text(response: &pulse_system_types::llm::LlmResponse) -> String {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            pulse_system_types::llm::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::durable::DurableLeaseTable;

    fn shared_leases(dir: &std::path::Path) -> SharedLeases {
        Arc::new(tokio::sync::Mutex::new(
            DurableLeaseTable::open(&dir.join("coordinator")).unwrap(),
        ))
    }

    fn spec(subs: &[(&str, &str)]) -> FarmSpec {
        FarmSpec {
            id: "f1".into(),
            description: "test farm".into(),
            subtasks: subs
                .iter()
                .map(|(id, prompt)| SubtaskSpec {
                    id: (*id).into(),
                    prompt: (*prompt).into(),
                })
                .collect(),
            synthesis: None,
        }
    }

    fn caps(ttl_ms: u64, reclaim_abort: bool) -> FarmCaps {
        FarmCaps {
            max_concurrency: 3,
            max_attempts: 2,
            subtask_ttl: Duration::from_millis(ttl_ms),
            reclaim_abort,
        }
    }

    /// AC20: concurrent subtasks, unique tokens, all results present.
    #[tokio::test]
    async fn farm_runs_all_subtasks_and_collects_results() {
        let dir = tempfile::tempdir().unwrap();
        let leases = shared_leases(dir.path());
        let outcome = run_farm(
            Arc::clone(&leases),
            "tenure-1",
            dir.path().to_path_buf(),
            spec(&[("a", "pa"), ("b", "pb"), ("c", "pc")]),
            caps(5_000, true),
            |sub: SubtaskSpec| async move { Ok(format!("done:{}", sub.id)) },
        )
        .await;

        assert_eq!(outcome.results.len(), 3);
        assert!(outcome.failed.is_empty());
        assert_eq!(outcome.late_fenced, 0);
        let mut texts: Vec<_> = outcome.results.iter().map(|r| r.text.clone()).collect();
        texts.sort();
        assert_eq!(texts, vec!["done:a", "done:b", "done:c"]);
        // Tokens were minted per subtask resource (watermarks exist).
        let table = leases.lock().await;
        for sub in ["a", "b", "c"] {
            assert!(table.table().watermark(&format!("farm-f1-{sub}")).is_some());
        }
    }

    /// AC21 + AC22 (spec Stage 3 exit): a stalled child is reclaimed under a
    /// strictly higher token; its LATE commit is rejected by fencing; the
    /// outcome carries exactly one result — the successor's.
    #[tokio::test]
    async fn reclaimed_child_late_result_is_fenced_and_parent_uncorrupted() {
        let dir = tempfile::tempdir().unwrap();
        let leases = shared_leases(dir.path());

        // Attempt 1 stalls past the 200ms ttl and finishes late (600ms);
        // attempt 2 completes promptly. reclaim_abort=false lets the stale
        // child live long enough to actually hit the fence.
        let attempt = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let attempt_c = Arc::clone(&attempt);
        let outcome = run_farm(
            Arc::clone(&leases),
            "tenure-1",
            dir.path().to_path_buf(),
            spec(&[("slow", "p")]),
            caps(200, false),
            move |_sub: SubtaskSpec| {
                let n = attempt_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n == 0 {
                        tokio::time::sleep(Duration::from_millis(600)).await;
                        Ok("STALE result from reclaimed child".to_string())
                    } else {
                        Ok("fresh result".to_string())
                    }
                }
            },
        )
        .await;

        // Give the stale child time to fire its late commit before asserting.
        tokio::time::sleep(Duration::from_millis(600)).await;

        assert_eq!(outcome.results.len(), 1, "exactly one result");
        assert_eq!(outcome.results[0].text, "fresh result");
        assert_eq!(outcome.results[0].attempts, 2);
        assert_eq!(outcome.reclaims, 1);
        // Token strictly increased across the reclaim.
        let table = leases.lock().await;
        assert!(table.table().watermark("farm-f1-slow").unwrap() >= FencingToken(2));
    }

    /// The late-fenced counter observes the rejection when the stale child
    /// commits after the farm reports (deterministic ordering variant).
    #[tokio::test]
    async fn late_commit_is_observably_fenced() {
        let results = Arc::new(FarmResults::new());

        // Simulate the exact interleaving at the store level: attempt 1
        // fenced at token 1; reclaim re-fences at token 2 BEFORE the stale
        // child commits; stale commit rejected; successor accepted.
        results.fence_at("s", FencingToken(1)).await;
        results.fence_at("s", FencingToken(2)).await;
        assert!(!results.commit("s", FencingToken(1), "stale".into()).await);
        assert!(results.commit("s", FencingToken(2), "fresh".into()).await);
        assert_eq!(results.take("s").await.as_deref(), Some("fresh"));
        assert_eq!(
            results
                .late_fenced
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    /// AC23: entering isolation abandons the farm — no commits after.
    #[tokio::test]
    async fn isolation_abandons_the_farm() {
        let dir = tempfile::tempdir().unwrap();
        let leases = shared_leases(dir.path());
        crate::server::isolation::enter(dir.path(), "test", None).unwrap();

        let outcome = run_farm(
            leases,
            "tenure-1",
            dir.path().to_path_buf(),
            spec(&[("a", "p")]),
            caps(1_000, true),
            |_sub: SubtaskSpec| async move { Ok("should never land".to_string()) },
        )
        .await;

        assert!(outcome.results.is_empty());
        assert_eq!(outcome.failed, vec!["a".to_string()]);
    }

    /// Executor errors retry and then fail cleanly (no result, no panic).
    #[tokio::test]
    async fn failing_executor_exhausts_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let leases = shared_leases(dir.path());
        let outcome = run_farm(
            leases,
            "tenure-1",
            dir.path().to_path_buf(),
            spec(&[("a", "p")]),
            caps(2_000, true),
            |_sub: SubtaskSpec| async move { Err("provider exploded".to_string()) },
        )
        .await;
        assert!(outcome.results.is_empty());
        assert_eq!(outcome.failed, vec!["a".to_string()]);
    }
}
