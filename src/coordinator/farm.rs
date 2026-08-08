//! Subtask farming (coordinator spec, Stage 3).
//!
//! Delegation of bounded work to child executions, built ENTIRELY on the
//! Stage 0 substrate — leases bound each child's tenure, fencing tokens
//! reject a reclaimed child's late result. No new ownership primitive: if
//! farming needed one, Stage 0 was incomplete (spec decision 6).
//!
//! Mechanics per subtask:
//! - each attempt acquires the lease `farm-{farm}-s{index}` (index-keyed:
//!   LLM-chosen subtask ids never collide or overflow at the lease layer)
//!   under an attempt-scoped holder, minting a strictly higher fencing
//!   token;
//! - the result store holds one `FencedResource` guard per subtask, raised
//!   TO the current attempt's token before the child runs — a stalled
//!   predecessor's commit is rejected in every interleaving — and a
//!   controller only trusts a commit made under its OWN token;
//! - a child that outlives its lease ttl is reclaimed: the next attempt's
//!   acquire (over the expired lease) mints a higher token, the guard is
//!   raised, and the old child is aborted (`kill_on_drop` reaps the CLI
//!   subprocess). Its late result, if it fires first, presents a stale
//!   token and is fenced.
//!
//! Lifetime discipline: every spawned task (controllers and children) sits
//! behind an abort-on-drop guard, so dropping `run_farm`'s future — tenure
//! loss, shutdown, the wall-budget deadline — reaps the whole farm,
//! provider subprocesses included. Leases a killed farm leaves behind are
//! released by the coordinator's stale-claim sweep at the next leadership
//! acquisition.
//!
//! Budgets: `wall_budget` (default 20min) keeps a farm inside its parent
//! task's 30min lease; `subtask_ttl` (default 16min) sits above the
//! provider's 15min subprocess timeout so a slow-but-legitimate child is
//! not reclaimed while its subprocess is still within budget. The lease
//! stall path depends on a non-decreasing wall clock (documented Stage 0
//! precondition).
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

/// Execution knobs. Defaults are production values; tests shrink the
/// durations and may disable reclaim-abort to let a stalled child run long
/// enough to prove its late commit is fenced.
#[derive(Debug, Clone)]
pub struct FarmCaps {
    pub max_concurrency: usize,
    pub max_attempts: u32,
    /// Per-attempt lease ttl. Must exceed the provider's own subprocess
    /// timeout (15min) or legitimately slow children get reclaimed mid-work.
    pub subtask_ttl: Duration,
    /// Whole-farm wall budget — keeps a farm inside its parent's lease.
    pub wall_budget: Duration,
    /// Abort a reclaimed child (production). `false` only in tests that
    /// deliberately let the stale child finish and hit the fence.
    pub reclaim_abort: bool,
}

impl Default for FarmCaps {
    fn default() -> Self {
        Self {
            max_concurrency: 3,
            max_attempts: 2,
            subtask_ttl: Duration::from_secs(16 * 60),
            wall_budget: Duration::from_secs(20 * 60),
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
    /// Subtasks that failed every attempt (or were cut off by the budget).
    pub failed: Vec<String>,
    /// Late commits from reclaimed children rejected by fencing — the spec's
    /// Stage 3 exit criterion made observable.
    pub late_fenced: u32,
    /// Reclaims performed (stalled children superseded).
    pub reclaims: u32,
}

/// A spawned task aborted when dropped — nothing in a farm outlives the
/// farm's future.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Fenced result store: the ONLY place child output lands, guarded per
/// subtask by a Stage 0 `FencedResource`. Results carry the committing
/// token so a controller can distinguish its own attempt's commit.
struct FarmResults {
    #[allow(clippy::type_complexity)]
    inner: Mutex<HashMap<String, (FencedResource, Option<(FencingToken, String)>)>>,
    late_fenced: std::sync::atomic::AtomicU32,
}

impl FarmResults {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            late_fenced: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Raise the subtask's fence to at least `token` — called at every
    /// attempt's acquire, BEFORE the child runs. Never lowers: a stale or
    /// duplicate caller cannot un-fence a newer epoch.
    async fn raise_fence(&self, sub_id: &str, token: FencingToken) {
        let mut inner = self.inner.lock().await;
        let entry = inner
            .entry(sub_id.to_string())
            .or_insert_with(|| (FencedResource::new(), None));
        let target = entry.0.high_water().map_or(token, |high| high.max(token));
        entry.0 = FencedResource::at(target);
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
                entry.1 = Some((token, text));
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

    /// Take the result iff it was committed under `token` — a controller
    /// only trusts its own attempt's commit.
    async fn take_if_token(&self, sub_id: &str, token: FencingToken) -> Option<String> {
        let mut inner = self.inner.lock().await;
        let entry = inner.get_mut(sub_id)?;
        match &entry.1 {
            Some((t, _)) if *t == token => entry.1.take().map(|(_, text)| text),
            _ => None,
        }
    }
}

/// Run a farm to completion (or its wall budget). The executor is one child
/// attempt: it receives the subtask spec and returns the child's text.
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

    let mut outcome = FarmOutcome {
        results: Vec::new(),
        failed: Vec::new(),
        late_fenced: 0,
        reclaims: 0,
    };

    // Dedup here too (the direct API must not rely on the marker path's
    // validation): a duplicate id would share a results key across distinct
    // lease resources and confuse token scoping.
    let mut seen = std::collections::HashSet::new();
    let mut subtasks: Vec<SubtaskSpec> = Vec::new();
    for sub in spec.subtasks.into_iter().take(MAX_SUBTASKS) {
        if seen.insert(sub.id.clone()) {
            subtasks.push(sub);
        } else {
            tracing::warn!(
                "farm '{farm_id}': duplicate subtask id '{}' dropped",
                sub.id
            );
            outcome.failed.push(sub.id);
        }
    }

    let mut controllers: Vec<(AbortOnDrop<ControllerOutput>, SubtaskSpec)> = Vec::new();
    for (index, sub) in subtasks.into_iter().enumerate() {
        let leases = Arc::clone(&leases);
        let results = Arc::clone(&results);
        let semaphore = Arc::clone(&semaphore);
        let reclaims = Arc::clone(&reclaims);
        let executor = executor.clone();
        let caps = caps.clone();
        let root_dir = root_dir.clone();
        let tenure_holder = tenure_holder.to_string();
        let farm_id = farm_id.clone();
        let sub_for_task = sub.clone();

        let handle = tokio::spawn(async move {
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
                index,
                sub_for_task,
            )
            .await
        });
        controllers.push((AbortOnDrop(handle), sub));
    }

    // Join under the wall budget. Past the deadline every remaining
    // controller — and, via its guards, every remaining child — is aborted
    // by drop: the farm cannot outrun its parent's lease.
    let deadline = tokio::time::Instant::now() + caps.wall_budget;
    for (mut controller, sub) in controllers {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let joined = tokio::time::timeout(remaining, &mut controller.0).await;
        let output = match joined {
            Ok(join_result) => join_result.ok().flatten(),
            Err(_) => {
                tracing::warn!(
                    "farm '{farm_id}': wall budget {:?} exhausted — aborting '{}'",
                    caps.wall_budget,
                    sub.id
                );
                None
            }
        };
        match output {
            Some((attempts, text)) => outcome.results.push(SubtaskResult {
                sub_id: sub.id.clone(),
                text,
                attempts,
            }),
            None => outcome.failed.push(sub.id.clone()),
        }
    }

    outcome.late_fenced = results
        .late_fenced
        .load(std::sync::atomic::Ordering::Relaxed);
    outcome.reclaims = reclaims.load(std::sync::atomic::Ordering::Relaxed);
    outcome
}

type ControllerOutput = Option<(u32, String)>;

/// Drive one subtask through its attempts. Returns the attempt count and
/// the committed text — taken from the store only under this attempt's own
/// token, so a stale predecessor's commit can never masquerade as ours.
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
    index: usize,
    sub: SubtaskSpec,
) -> ControllerOutput
where
    F: Fn(SubtaskSpec) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    // Index-keyed: LLM-chosen ids can neither collide nor overflow the
    // lease id budget once composed with the farm id.
    let resource = format!("farm-{farm_id}-s{index}");
    let mut stalled_child: Option<AbortOnDrop<bool>> = None;

    for attempt in 1..=caps.max_attempts {
        // Shed with everything else: a farm never outlives isolation entry.
        if crate::server::isolation::is_active(&root_dir) {
            tracing::warn!(
                "farm '{farm_id}': ISOLATION active — abandoning '{}'",
                sub.id
            );
            return None;
        }

        // Acquire (attempt 1) or reclaim-by-expiry (attempt >1: the previous
        // child stalled past its ttl; this acquire mints a higher token).
        let holder = format!("{tenure_holder}-a{attempt}");
        let acquired = {
            leases
                .lock()
                .await
                .acquire(&resource, &holder, caps.subtask_ttl, Utc::now())
        };
        let lease = match acquired {
            Ok(lease) => lease,
            Err(e) => {
                tracing::warn!("farm '{farm_id}': claim failed on '{}': {e}", sub.id);
                return None;
            }
        };

        // Raise the fence BEFORE the child runs: from this instant any
        // commit bearing an older token — a stalled predecessor waking up —
        // is rejected, regardless of interleaving.
        results.raise_fence(&sub.id, lease.fencing_token).await;
        if attempt > 1 {
            reclaims.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if caps.reclaim_abort {
                // Guard drop aborts the child; kill_on_drop reaps the CLI.
                stalled_child = None;
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
            AbortOnDrop(tokio::spawn(async move {
                let _permit = permit;
                match executor(sub.clone()).await {
                    Ok(text) => results.commit(&sub.id, token, text).await,
                    Err(e) => {
                        tracing::warn!("farm child '{}' failed: {e}", sub.id);
                        false
                    }
                }
            }))
        };

        // Wait out this attempt's lease window. select (not timeout) so the
        // guard survives a stall — the handle must stay reachable for the
        // reclaim abort, and abort-on-drop must cover farm teardown.
        let finished: Option<bool> = tokio::select! {
            joined = &mut child.0 => Some(joined.unwrap_or(false)),
            () = tokio::time::sleep(caps.subtask_ttl) => None,
        };

        match finished {
            Some(committed) => {
                let _ = { leases.lock().await.release(&resource, &holder, Utc::now()) };
                if committed {
                    // Only OUR token's commit counts — a stale predecessor's
                    // text under an older token is invisible here.
                    if let Some(text) = results.take_if_token(&sub.id, lease.fencing_token).await {
                        // Test determinism: with reclaim-abort disabled, a
                        // still-running stale child gets its lease window to
                        // fire (and be fenced) before the farm reports.
                        if let Some(mut old) = stalled_child.take() {
                            if !caps.reclaim_abort {
                                let _ = tokio::time::timeout(caps.subtask_ttl, &mut old.0).await;
                            }
                        }
                        return Some((attempt, text));
                    }
                }
                // Executor error (or a commit that wasn't ours): retry on
                // the next attempt — the lease was released, the watermark
                // still climbs.
            }
            None => {
                // Stalled past the ttl: keep the guard; the NEXT attempt's
                // acquire reclaims by expiry and raises the fence before
                // (optionally) aborting it. Do not release — expiry is the
                // reclaim signal. If this was the last attempt, the guard's
                // drop reaps the child on return.
                tracing::warn!(
                    "farm '{farm_id}': child on '{}' exceeded ttl {:?} — reclaiming",
                    sub.id,
                    caps.subtask_ttl
                );
                stalled_child = Some(child);
                continue;
            }
        }
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
        composed.push_str(&format!("## {}\n{}\n\n", r.sub_id, r.text));
    }
    if !outcome.failed.is_empty() {
        composed.push_str(&format!("(failed subtasks: {:?})\n", outcome.failed));
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
            wall_budget: Duration::from_secs(30),
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
            caps(10_000, true),
            |sub: SubtaskSpec| async move { Ok(format!("done:{}", sub.id)) },
        )
        .await;

        assert_eq!(outcome.results.len(), 3);
        assert!(outcome.failed.is_empty());
        assert_eq!(outcome.late_fenced, 0);
        let mut texts: Vec<_> = outcome.results.iter().map(|r| r.text.clone()).collect();
        texts.sort();
        assert_eq!(texts, vec!["done:a", "done:b", "done:c"]);
        // Tokens were minted per index-keyed subtask resource.
        let table = leases.lock().await;
        for i in 0..3 {
            assert!(table.table().watermark(&format!("farm-f1-s{i}")).is_some());
        }
    }

    /// AC21 + AC22 (spec Stage 3 exit): a stalled child is reclaimed under a
    /// strictly higher token; its LATE result is rejected by fencing —
    /// observable in the outcome — and exactly one result survives: the
    /// successor's.
    #[tokio::test]
    async fn reclaimed_child_late_result_is_fenced_and_parent_uncorrupted() {
        let dir = tempfile::tempdir().unwrap();
        let leases = shared_leases(dir.path());

        // Attempt 1 stalls past the 1s ttl and finishes at 1.8s; attempt 2
        // completes promptly. reclaim_abort=false keeps the stale child
        // alive, and the controller waits for it to hit the fence before the
        // farm reports — so late_fenced is asserted, not assumed.
        let attempt = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let attempt_c = Arc::clone(&attempt);
        let outcome = run_farm(
            Arc::clone(&leases),
            "tenure-1",
            dir.path().to_path_buf(),
            spec(&[("slow", "p")]),
            caps(1_000, false),
            move |_sub: SubtaskSpec| {
                let n = attempt_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n == 0 {
                        tokio::time::sleep(Duration::from_millis(1_800)).await;
                        Ok("STALE result from reclaimed child".to_string())
                    } else {
                        Ok("fresh result".to_string())
                    }
                }
            },
        )
        .await;

        assert_eq!(outcome.results.len(), 1, "exactly one result");
        assert_eq!(outcome.results[0].text, "fresh result");
        assert_eq!(outcome.results[0].attempts, 2);
        assert_eq!(outcome.reclaims, 1);
        assert_eq!(
            outcome.late_fenced, 1,
            "the stale commit must hit the fence"
        );
        // Token strictly increased across the reclaim.
        let table = leases.lock().await;
        assert!(table.table().watermark("farm-f1-s0").unwrap() >= FencingToken(2));
    }

    /// Store-level pin of the exact interleaving, incl. that a fence can
    /// never be lowered and a controller only takes its own token's result.
    #[tokio::test]
    async fn late_commit_is_observably_fenced_and_token_scoped() {
        let results = Arc::new(FarmResults::new());

        results.raise_fence("s", FencingToken(1)).await;
        results.raise_fence("s", FencingToken(2)).await;
        // Attempting to lower the fence is a no-op.
        results.raise_fence("s", FencingToken(1)).await;
        assert!(!results.commit("s", FencingToken(1), "stale".into()).await);
        assert!(results.commit("s", FencingToken(2), "fresh".into()).await);
        // A controller holding token 1 cannot take token 2's result.
        assert!(results.take_if_token("s", FencingToken(1)).await.is_none());
        assert_eq!(
            results.take_if_token("s", FencingToken(2)).await.as_deref(),
            Some("fresh")
        );
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

    /// The wall budget aborts a farm that would outrun its parent's lease.
    #[tokio::test]
    async fn wall_budget_bounds_the_farm() {
        let dir = tempfile::tempdir().unwrap();
        let leases = shared_leases(dir.path());
        let mut c = caps(10_000, true);
        c.wall_budget = Duration::from_millis(300);
        let outcome = run_farm(
            leases,
            "tenure-1",
            dir.path().to_path_buf(),
            spec(&[("a", "p")]),
            c,
            |_sub: SubtaskSpec| async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok("too late".to_string())
            },
        )
        .await;
        assert!(outcome.results.is_empty());
        assert_eq!(outcome.failed, vec!["a".to_string()]);
    }

    /// Duplicate subtask ids are dropped (kept once), not silently collided.
    #[tokio::test]
    async fn duplicate_subtask_ids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let leases = shared_leases(dir.path());
        let outcome = run_farm(
            leases,
            "tenure-1",
            dir.path().to_path_buf(),
            spec(&[("a", "p1"), ("a", "p2")]),
            caps(5_000, true),
            |sub: SubtaskSpec| async move { Ok(format!("done:{}", sub.prompt)) },
        )
        .await;
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].text, "done:p1");
        assert_eq!(outcome.failed, vec!["a".to_string()]);
    }
}
