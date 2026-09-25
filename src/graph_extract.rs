//! Background entity extraction for the archives this pulse ingests.
//!
//! Ingest writes episodes; entities and relationships only appear when an
//! archive is *extracted*, which costs model tokens. recall-echo's daemon
//! extracts in the background for an embedded store, but a pulse runs its
//! graph in server mode, where there is no daemon — so until now nothing
//! extracted at all, and the graph held thousands of episodes and no
//! entities. The pulse knows the moment an archive lands, so it asks for
//! exactly that archive here.
//!
//! ```text
//! boot catch-up (log > watermark) ─────────┐
//!                                          ▼
//! graph_ingest_archive ──enqueue(log)──▶ queue ──▶ one worker ──▶ recall-echo
//!                                         (≤64)     │               extract_archive
//!                                                   ├─ isolated?        skip
//!                                                   ├─ paused today?    skip
//!                                                   ├─ budget spent?    skip
//!                                                   └─ extract, charge the ledger
//! ```
//!
//! Discipline:
//!
//! - **Never blocks chat.** Enqueueing is a lock and a push; the work runs on
//!   its own task.
//! - **One at a time.** A single worker drains the queue; a second archive
//!   waits for the first.
//! - **One attempt per archive.** A failure is logged and the archive stays
//!   pending in the store, where a backfill (`recall-echo graph extract`)
//!   finds it. Three failures in a row pause extraction for the rest of the
//!   UTC day, so a broken provider costs three attempts, not one per archive.
//! - **Bounded spend.** A per-day token cap, recorded on disk so a restart
//!   does not reset it.
//! - **Nothing archived is missed.** In practice a pulse archives almost
//!   only at shutdown, where extraction would be killed mid-archive, so it
//!   is skipped there. A watermark in the same ledger — the highest archive
//!   ever queued — lets each boot queue what arrived since (the archives
//!   the store still has pending, oldest first). Queueing raises it, so a
//!   crash never pays for an archive twice. The first boot only sets it: the
//!   historical backlog is a backfill decision, not a side effect of a deploy.
//! - **The pulse's own CLI environment.** The extraction CLI is spawned with
//!   the same allowlisted environment a chat turn gets (its login included,
//!   nothing else), from a neutral working directory so it never loads the
//!   pulse's own instructions or hooks.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{NaiveDate, Utc};
use recall_echo::archive_extract::{
    ArchiveExtraction, CliOverrides, ExtractArchiveError, ExtractOutcome,
};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Archives waiting beyond the one in flight. More than this and the newest
/// is left for a backfill rather than queued without bound.
const MAX_QUEUED: usize = 64;
/// Failures in a row that pause extraction until the next UTC day.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// Longest one archive may take before it counts as a failure. The largest
/// archive measured on a live pulse took 11 minutes.
const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Spend and watermark ledger, in the pulse root beside the other runtime
/// state.
const LEDGER_FILE: &str = "graph_extraction.json";
/// Where the extraction CLI runs: nowhere an agent CLI finds project
/// instructions, hooks or rules to load.
const NEUTRAL_DIR: &str = "/";

// ── The work ────────────────────────────────────────────────────────────

/// What one extraction attempt resolved to.
pub type ExtractResult = Result<ExtractOutcome, ExtractArchiveError>;

/// Extracts one archive. Behind a trait so scheduling, budget and back-off
/// can be tested without a store or a model.
pub trait ExtractBackend: Send + Sync {
    fn extract(&self, log_number: u32) -> Pin<Box<dyn Future<Output = ExtractResult> + Send + '_>>;

    /// Log numbers the store still has awaiting extraction.
    fn pending(&self) -> Pin<Box<dyn Future<Output = Result<Vec<u32>, String>> + Send + '_>>;
}

/// The real backend: recall-echo, on this pulse's memory.
struct RecallBackend {
    memory_dir: PathBuf,
    overrides: CliOverrides,
}

impl ExtractBackend for RecallBackend {
    fn extract(&self, log_number: u32) -> Pin<Box<dyn Future<Output = ExtractResult> + Send + '_>> {
        Box::pin(recall_echo::archive_extract::extract_archive(
            &self.memory_dir,
            log_number,
            &self.overrides,
        ))
    }

    fn pending(&self) -> Pin<Box<dyn Future<Output = Result<Vec<u32>, String>> + Send + '_>> {
        Box::pin(async move {
            let graph = recall_echo::graph::GraphMemory::open(&self.memory_dir.join("graph"))
                .await
                .map_err(|e| format!("graph open: {e}"))?;
            let logs = graph
                .unextracted_log_numbers()
                .await
                .map_err(|e| format!("pending scan: {e}"))?;
            Ok(logs
                .into_iter()
                .filter_map(|log| u32::try_from(log).ok())
                .collect())
        })
    }
}

/// How the extraction CLI is spawned for this pulse.
///
/// The environment is always the pulse's allowlisted child environment —
/// for the adapter that drives the CLI recall-echo is configured with, so a
/// pulse chatting on one CLI and extracting with another still hands each
/// only its own credentials. The binary is pinned to `[llm] cli_bin` only
/// when extraction uses the very CLI the pulse chats with; otherwise
/// recall-echo locates it.
fn cli_overrides(config: &Config, root_dir: &Path, memory_dir: &Path) -> CliOverrides {
    let recall_provider = recall_echo::config::load(memory_dir).llm.provider;
    let adapter =
        crate::cli_provider::adapters::by_recall_echo_provider(&recall_provider.to_string());
    let chat_adapter = config.llm.cli_adapter();
    let command = adapter
        .as_ref()
        .filter(|adapter| chat_adapter == Some(adapter.name()))
        .and_then(|_| {
            config
                .llm
                .cli_bin
                .clone()
                .or_else(|| std::env::var(crate::cli_provider::CLI_BIN_ENV).ok())
        })
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    CliOverrides {
        command,
        env: Some(crate::cli_provider::child_env(root_dir, adapter.as_deref())),
        current_dir: Some(PathBuf::from(NEUTRAL_DIR)),
    }
}

// ── Ledger ───────────────────────────────────────────────────────────────

/// What `graph_extraction.json` holds. Every field is optional on read, so
/// a ledger written before the watermark existed (`{day, tokens}`) still
/// loads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct LedgerState {
    /// The UTC day `tokens` was spent on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    day: Option<NaiveDate>,
    #[serde(default)]
    tokens: u64,
    /// The highest archive log number ever queued for extraction. Archives
    /// above it are what the next boot's catch-up looks for. `None` until
    /// the first boot sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark: Option<u32>,
}

/// The on-disk ledger: one small JSON file, read-modify-written under a lock
/// and replaced atomically, so a crash leaves the old state or the new one.
struct Ledger {
    path: PathBuf,
    write: Mutex<()>,
}

impl Ledger {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            write: Mutex::new(()),
        }
    }

    /// The current state. Missing reads as empty; unreadable reads as empty
    /// and says so — the next write replaces it.
    fn read(&self) -> LedgerState {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return LedgerState::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!(
                "graph: extraction ledger {} is unreadable ({e}) — treating it as empty",
                self.path.display()
            );
            LedgerState::default()
        })
    }

    /// Apply `change` to the stored state and persist it. Returns the new state.
    fn update(&self, change: impl FnOnce(&mut LedgerState)) -> LedgerState {
        let _held = self
            .write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self.read();
        change(&mut state);
        if let Err(e) = write_atomically(&self.path, &state) {
            tracing::warn!(
                "graph: cannot write extraction ledger {}: {e}",
                self.path.display()
            );
        }
        state
    }

    fn watermark(&self) -> Option<u32> {
        self.read().watermark
    }

    /// Raise the watermark to `log_number`; never lowers it.
    fn raise_watermark(&self, log_number: u32) {
        if self.watermark().is_some_and(|w| w >= log_number) {
            return;
        }
        self.update(|state| {
            state.watermark = Some(state.watermark.map_or(log_number, |w| w.max(log_number)));
        });
    }
}

fn write_atomically(path: &Path, state: &LedgerState) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(state)?)?;
    std::fs::rename(&tmp, path)
}

// ── Budget ───────────────────────────────────────────────────────────────

/// A per-day token cap, recorded in the ledger so a restart cannot reset it.
struct DailyBudget {
    limit: u64,
}

impl DailyBudget {
    /// Tokens spent on `day`; a stale day reads as nothing spent.
    fn spent(&self, ledger: &Ledger, day: NaiveDate) -> u64 {
        let state = ledger.read();
        if state.day == Some(day) {
            state.tokens
        } else {
            0
        }
    }

    /// True when `day` has no budget left.
    fn exhausted(&self, ledger: &Ledger, day: NaiveDate) -> bool {
        self.limit > 0 && self.spent(ledger, day) >= self.limit
    }

    /// Add `tokens` to `day` and return the day's new total.
    fn charge(&self, ledger: &Ledger, day: NaiveDate, tokens: u64) -> u64 {
        ledger
            .update(|state| {
                if state.day != Some(day) {
                    state.day = Some(day);
                    state.tokens = 0;
                }
                state.tokens = state.tokens.saturating_add(tokens);
            })
            .tokens
    }

    fn describe(&self, spent: u64) -> String {
        if self.limit == 0 {
            format!("{spent} tokens today, no cap")
        } else {
            format!("{spent} of {} tokens today", self.limit)
        }
    }
}

// ── Catch-up ─────────────────────────────────────────────────────────────

/// Log numbers of the archives in `dir` (`conversation-<N>.md`), ascending.
fn archive_logs(dir: &Path) -> Vec<u32> {
    let mut logs: Vec<u32> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_prefix("conversation-")?
                .strip_suffix(".md")?
                .parse()
                .ok()
        })
        .collect();
    logs.sort_unstable();
    logs
}

// ── Scheduling state ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Queue {
    pending: VecDeque<u32>,
    /// Catch-up archives that did not fit in `pending`; moved in, oldest
    /// first, as slots free up.
    backlog: VecDeque<u32>,
    /// A worker is draining the queue.
    running: bool,
    /// The boot catch-up has not finished queueing yet.
    catching_up: bool,
    consecutive_failures: u32,
    /// Extraction is paused for the rest of this day.
    paused_on: Option<NaiveDate>,
    /// The day the "budget spent" line was last logged, so it is said once.
    budget_noted_on: Option<NaiveDate>,
}

/// Why an archive is not extracted now.
enum Refusal {
    /// Log this reason.
    Say(String),
    /// The reason was already logged today.
    Quiet,
}

/// Source of "today", in UTC. Injectable so day rollover can be tested.
type Today = Arc<dyn Fn() -> NaiveDate + Send + Sync>;

struct Inner {
    root_dir: PathBuf,
    ledger: Ledger,
    budget: DailyBudget,
    backend: Arc<dyn ExtractBackend>,
    runtime: tokio::runtime::Handle,
    today: Today,
    archive_timeout: Duration,
    queue: Mutex<Queue>,
}

/// Queues this pulse's freshly ingested archives for extraction and drains
/// them one at a time in the background.
#[derive(Clone)]
pub struct GraphExtractor {
    inner: Arc<Inner>,
}

impl GraphExtractor {
    /// The extractor for a pulse, or `None` when `[graph]` turns extraction
    /// off. Must be called inside the runtime the work should run on — the
    /// callers that enqueue sometimes run in a short-lived runtime of their
    /// own, which would take a task spawned there down with it.
    pub fn for_pulse(config: &Config, root_dir: &Path) -> Option<Self> {
        let graph = &config.graph;
        if !(graph.enabled && graph.auto_ingest && graph.extract) {
            return None;
        }
        let memory_dir = root_dir.join("memory");
        let backend = RecallBackend {
            overrides: cli_overrides(config, root_dir, &memory_dir),
            memory_dir,
        };
        tracing::info!(
            "graph: extraction on — {}",
            if graph.extract_daily_token_budget == 0 {
                "no daily token cap".to_string()
            } else {
                format!("{} tokens per UTC day", graph.extract_daily_token_budget)
            }
        );
        let extractor = Self::new(
            root_dir,
            graph.extract_daily_token_budget,
            Arc::new(backend),
            tokio::runtime::Handle::current(),
        );
        extractor.start_catch_up(&crate::session::conversations_dir(root_dir));
        Some(extractor)
    }

    fn new(
        root_dir: &Path,
        daily_token_budget: u64,
        backend: Arc<dyn ExtractBackend>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                root_dir: root_dir.to_path_buf(),
                ledger: Ledger::new(root_dir.join(LEDGER_FILE)),
                budget: DailyBudget {
                    limit: daily_token_budget,
                },
                backend,
                runtime,
                today: Arc::new(|| Utc::now().date_naive()),
                archive_timeout: ARCHIVE_TIMEOUT,
                queue: Mutex::new(Queue::default()),
            }),
        }
    }

    /// Queue archive `log_number` for extraction. Returns at once; the
    /// archive is extracted in the background, after any already queued.
    /// Queueing it raises the watermark, so no later boot queues it again.
    pub fn enqueue(&self, log_number: u32) {
        let start_worker = {
            let mut queue = self.inner.lock_queue();
            if queue.pending.contains(&log_number) {
                return;
            }
            if queue.pending.len() >= MAX_QUEUED {
                tracing::info!(
                    "graph: extraction queue full ({MAX_QUEUED}) — conversation-{log_number:03} \
                     left pending for a backfill"
                );
                return;
            }
            queue.pending.push_back(log_number);
            !std::mem::replace(&mut queue.running, true)
        };
        self.inner.ledger.raise_watermark(log_number);
        if start_worker {
            self.inner.spawn_worker();
        }
    }

    /// Queue what this pulse archived since extraction last looked — in
    /// practice the archives written at the previous shutdown, which are
    /// ingested on the way out but never extracted there.
    ///
    /// The watermark is read now, before anything can be enqueued; the scan
    /// and the store query run in the background, so startup never waits.
    /// With no watermark yet, it is set to the newest archive on disk and
    /// nothing is queued: the historical backlog is a backfill decision, not
    /// something a deploy should start spending on.
    fn start_catch_up(&self, conversations_dir: &Path) {
        let highest = archive_logs(conversations_dir).last().copied().unwrap_or(0);
        let Some(watermark) = self.inner.ledger.watermark() else {
            self.inner.ledger.raise_watermark(highest);
            tracing::info!(
                "graph: catch-up — first run, watermark set to log {highest}; earlier archives \
                 are left to a backfill"
            );
            return;
        };
        if highest <= watermark {
            tracing::info!("graph: catch-up — 0 archive(s) since log {watermark} queued");
            return;
        }
        self.inner.lock_queue().catching_up = true;
        let inner = Arc::clone(&self.inner);
        let dir = conversations_dir.to_path_buf();
        self.inner
            .runtime
            .spawn(async move { inner.catch_up(&dir, watermark).await });
    }
}

impl Inner {
    fn lock_queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The next archive, or `None` — in which case the worker has been
    /// marked stopped, under the same lock an enqueue takes. Tops the queue
    /// up from the catch-up backlog as slots free.
    fn next_job(&self) -> Option<u32> {
        let (next, raised) = {
            let mut queue = self.lock_queue();
            let raised = refill(&mut queue);
            let next = queue.pending.pop_front();
            if next.is_none() {
                queue.running = false;
            }
            (next, raised)
        };
        if let Some(log_number) = raised {
            self.ledger.raise_watermark(log_number);
        }
        next
    }

    fn spawn_worker(self: &Arc<Self>) {
        self.runtime.spawn(drain(Arc::clone(self)));
    }

    /// Queue every archive above `watermark` that the store still has
    /// pending, oldest first. Whatever does not fit in the queue waits in
    /// the backlog.
    async fn catch_up(self: Arc<Self>, dir: &Path, watermark: u32) {
        let candidates: Vec<u32> = archive_logs(dir)
            .into_iter()
            .filter(|log| *log > watermark)
            .collect();
        let found = match self.backend.pending().await {
            Ok(pending) => {
                let pending: std::collections::HashSet<u32> = pending.into_iter().collect();
                candidates
                    .into_iter()
                    .filter(|log| pending.contains(log))
                    .collect()
            }
            Err(e) => {
                // Extraction itself skips what has nothing pending, for free.
                tracing::info!(
                    "graph: catch-up cannot list pending archives ({e}) — queueing every \
                     archive since log {watermark}"
                );
                candidates
            }
        };
        let count = found.len();
        let (raised, start_worker) = {
            let mut queue = self.lock_queue();
            queue.catching_up = false;
            for log_number in found {
                if !queue.pending.contains(&log_number) && !queue.backlog.contains(&log_number) {
                    queue.backlog.push_back(log_number);
                }
            }
            let raised = refill(&mut queue);
            let start = !queue.pending.is_empty() && !std::mem::replace(&mut queue.running, true);
            (raised, start)
        };
        if let Some(log_number) = raised {
            self.ledger.raise_watermark(log_number);
        }
        tracing::info!("graph: catch-up — {count} archive(s) since log {watermark} queued");
        if start_worker {
            self.spawn_worker();
        }
    }

    /// Why `log_number` must not be extracted right now, if it must not.
    fn refusal(&self, today: NaiveDate) -> Option<Refusal> {
        if crate::server::isolation::is_active(&self.root_dir) {
            return Some(Refusal::Say("the pulse is isolated".into()));
        }
        let mut queue = self.lock_queue();
        if queue.paused_on == Some(today) {
            // Said once, when the pause began.
            return Some(Refusal::Quiet);
        }
        if !self.budget.exhausted(&self.ledger, today) {
            return None;
        }
        if queue.budget_noted_on == Some(today) {
            return Some(Refusal::Quiet);
        }
        queue.budget_noted_on = Some(today);
        Some(Refusal::Say(format!(
            "daily token budget spent ({}); further archives today are left pending quietly",
            self.budget.describe(self.budget.spent(&self.ledger, today))
        )))
    }

    async fn process(&self, log_number: u32) {
        let today = (self.today)();
        match self.refusal(today) {
            None => {}
            Some(Refusal::Quiet) => {
                tracing::debug!(
                    "graph: extraction skipped for conversation-{log_number:03}; left pending"
                );
                return;
            }
            Some(Refusal::Say(reason)) => {
                tracing::info!(
                    "graph: extraction skipped for conversation-{log_number:03} — {reason}; \
                     left pending"
                );
                return;
            }
        }

        let result = tokio::time::timeout(self.archive_timeout, self.backend.extract(log_number))
            .await
            .unwrap_or_else(|_| {
                Err(ExtractArchiveError::Recall(
                    recall_echo::error::RecallError::Other(format!(
                        "timed out after {}s",
                        self.archive_timeout.as_secs()
                    )),
                ))
            });

        let spent = match &result {
            Ok(ExtractOutcome::Extracted(extraction)) => extraction.total_tokens(),
            Ok(ExtractOutcome::NothingPending) => 0,
            Err(err) => err.tokens_spent(),
        };
        let spent_today = if spent > 0 {
            self.budget.charge(&self.ledger, today, spent)
        } else {
            self.budget.spent(&self.ledger, today)
        };
        self.report(log_number, today, result, spent_today);
    }

    fn report(&self, log_number: u32, today: NaiveDate, result: ExtractResult, spent_today: u64) {
        let budget = self.budget.describe(spent_today);
        match result {
            Ok(ExtractOutcome::Extracted(extraction)) => {
                self.lock_queue().consecutive_failures = 0;
                log_extracted(log_number, &extraction, &budget);
            }
            Ok(ExtractOutcome::NothingPending) => tracing::info!(
                "graph: extraction for conversation-{log_number:03} not needed — nothing pending"
            ),
            Err(ExtractArchiveError::ProviderUnavailable(reason)) => tracing::info!(
                "graph: extraction skipped for conversation-{log_number:03} — {reason}; left pending"
            ),
            Err(err) => {
                tracing::warn!(
                    "graph: extraction failed for conversation-{log_number:03}: {err} ({budget}); \
                     left pending"
                );
                self.record_failure(today);
            }
        }
    }

    fn record_failure(&self, today: NaiveDate) {
        let mut queue = self.lock_queue();
        queue.consecutive_failures += 1;
        if queue.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            queue.consecutive_failures = 0;
            queue.paused_on = Some(today);
            tracing::warn!(
                "graph: extraction failed {MAX_CONSECUTIVE_FAILURES} times in a row — paused until \
                 the next UTC day; {} queued archive(s) left pending",
                queue.pending.len()
            );
        }
    }
}

fn log_extracted(log_number: u32, extraction: &ArchiveExtraction, budget: &str) {
    tracing::info!(
        "graph: extracted conversation-{log_number:03} — +{} entities, {} merged, {} relationships, \
         {} tokens ({budget})",
        extraction.entities_created,
        extraction.entities_merged,
        extraction.relationships_created,
        extraction.total_tokens(),
    );
    if let Some(first) = extraction.warnings.first() {
        tracing::info!(
            "graph: extraction of conversation-{log_number:03} had {} warning(s); first: {first}",
            extraction.warnings.len()
        );
    }
}

/// Move backlog into free queue slots, oldest first. Returns the highest log
/// number moved, for the watermark.
fn refill(queue: &mut Queue) -> Option<u32> {
    let mut raised = None;
    while queue.pending.len() < MAX_QUEUED {
        let Some(log_number) = queue.backlog.pop_front() else {
            break;
        };
        if !queue.pending.contains(&log_number) {
            queue.pending.push_back(log_number);
            raised = Some(raised.map_or(log_number, |r: u32| r.max(log_number)));
        }
    }
    raised
}

/// Drain the queue, one archive at a time, until it is empty.
async fn drain(inner: Arc<Inner>) {
    let _unwedge = UnwedgeOnPanic(Arc::clone(&inner));
    while let Some(log_number) = inner.next_job() {
        inner.process(log_number).await;
    }
}

/// A worker that panics must not leave `running` set, or nothing would ever
/// be extracted again. Only on panic: a worker that ended normally already
/// cleared it under the lock, and clearing it again here could release a
/// worker that started since.
struct UnwedgeOnPanic(Arc<Inner>);

impl Drop for UnwedgeOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.lock_queue().running = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A backend that answers from a script, counts calls, and tracks how
    /// many run at once.
    struct Scripted {
        answers: Mutex<VecDeque<ExtractResult>>,
        /// What `pending()` answers; empty means "everything is pending".
        pending: Mutex<Option<Result<Vec<u32>, String>>>,
        seen: Mutex<Vec<u32>>,
        calls: AtomicUsize,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        delay: Duration,
    }

    impl Scripted {
        fn new(answers: Vec<ExtractResult>, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into()),
                pending: Mutex::new(None),
                seen: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                delay,
            })
        }

        fn with_pending(self: Arc<Self>, pending: Result<Vec<u32>, String>) -> Arc<Self> {
            *self.pending.lock().unwrap() = Some(pending);
            self
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn seen(&self) -> Vec<u32> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl ExtractBackend for Scripted {
        fn pending(&self) -> Pin<Box<dyn Future<Output = Result<Vec<u32>, String>> + Send + '_>> {
            Box::pin(async move {
                self.pending
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| Ok((0..10_000).collect()))
            })
        }

        fn extract(&self, log: u32) -> Pin<Box<dyn Future<Output = ExtractResult> + Send + '_>> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(log);
                self.calls.fetch_add(1, Ordering::SeqCst);
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(self.delay).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                self.answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Ok(ExtractOutcome::NothingPending))
            })
        }
    }

    fn extracted(tokens: u64) -> ExtractResult {
        Ok(ExtractOutcome::Extracted(ArchiveExtraction {
            entities_created: 3,
            relationships_created: 2,
            measured_tokens: tokens,
            ..ArchiveExtraction::default()
        }))
    }

    fn failed() -> ExtractResult {
        Err(ExtractArchiveError::AllChunksFailed {
            chunks: 1,
            first: "claude exited 1: (stdout) Not logged in".into(),
            tokens: 10,
        })
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, d).unwrap()
    }

    fn extractor(
        root: &Path,
        budget: u64,
        backend: Arc<Scripted>,
        today: Arc<Mutex<NaiveDate>>,
    ) -> GraphExtractor {
        let mut extractor =
            GraphExtractor::new(root, budget, backend, tokio::runtime::Handle::current());
        let inner = Arc::get_mut(&mut extractor.inner).unwrap();
        inner.today = Arc::new(move || *today.lock().unwrap());
        extractor
    }

    fn spent(extractor: &GraphExtractor, on: NaiveDate) -> u64 {
        extractor.inner.budget.spent(&extractor.inner.ledger, on)
    }

    fn watermark(extractor: &GraphExtractor) -> Option<u32> {
        extractor.inner.ledger.watermark()
    }

    /// `archives/conversations` under `root`, holding `conversation-<N>.md`
    /// for every `N` in `logs`.
    fn archives(root: &Path, logs: impl IntoIterator<Item = u32>) -> PathBuf {
        let dir = root.join("archives").join("conversations");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("INDEX.md"), "# index\n").unwrap();
        for log in logs {
            std::fs::write(dir.join(format!("conversation-{log:03}.md")), "x").unwrap();
        }
        dir
    }

    /// Wait until the catch-up has queued and the worker has drained the
    /// queue and stopped.
    async fn settle(extractor: &GraphExtractor) {
        for _ in 0..500 {
            {
                let queue = extractor.inner.lock_queue();
                if !queue.running && !queue.catching_up {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("extraction worker never settled");
    }

    #[tokio::test]
    async fn archives_are_extracted_one_at_a_time_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Scripted::new(
            vec![extracted(100), extracted(100), extracted(100)],
            Duration::from_millis(30),
        );
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);

        extractor.enqueue(1);
        extractor.enqueue(2);
        extractor.enqueue(2);
        extractor.enqueue(3);
        settle(&extractor).await;

        assert_eq!(backend.calls(), 3, "the duplicate is queued once");
        assert_eq!(backend.max_in_flight.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn enqueue_returns_before_the_extraction_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Scripted::new(vec![extracted(1)], Duration::from_secs(2));
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);

        let started = std::time::Instant::now();
        extractor.enqueue(1);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn spend_is_charged_and_the_cap_stops_further_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Scripted::new(
            vec![extracted(600), extracted(600), extracted(600)],
            Duration::ZERO,
        );
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 1_000, Arc::clone(&backend), Arc::clone(&today));

        for log in 1..=3 {
            extractor.enqueue(log);
            settle(&extractor).await;
        }

        // 600, then 1200 (the overrun of one archive), then refused.
        assert_eq!(backend.calls(), 2);
        assert_eq!(spent(&extractor, day(25)), 1_200);
    }

    #[tokio::test]
    async fn the_cap_resets_on_the_next_utc_day_and_survives_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let today = Arc::new(Mutex::new(day(25)));
        let backend = Scripted::new(vec![extracted(1_500)], Duration::ZERO);
        let first = extractor(tmp.path(), 1_000, Arc::clone(&backend), Arc::clone(&today));
        first.enqueue(1);
        settle(&first).await;

        // A restart reads the ledger: still spent today.
        let backend = Scripted::new(vec![extracted(10), extracted(10)], Duration::ZERO);
        let restarted = extractor(tmp.path(), 1_000, Arc::clone(&backend), Arc::clone(&today));
        restarted.enqueue(2);
        settle(&restarted).await;
        assert_eq!(backend.calls(), 0);

        *today.lock().unwrap() = day(26);
        restarted.enqueue(3);
        settle(&restarted).await;
        assert_eq!(backend.calls(), 1);
        assert_eq!(spent(&restarted, day(26)), 10);
    }

    #[tokio::test]
    async fn a_zero_budget_means_no_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Scripted::new(
            (0..3).map(|_| extracted(u64::MAX / 4)).collect(),
            Duration::ZERO,
        );
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);
        for log in 1..=3 {
            extractor.enqueue(log);
            settle(&extractor).await;
        }
        assert_eq!(backend.calls(), 3);
    }

    /// A broken provider costs three attempts, then nothing until tomorrow.
    #[tokio::test]
    async fn repeated_failures_pause_extraction_until_the_next_day() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Scripted::new(
            vec![failed(), failed(), failed(), extracted(5)],
            Duration::ZERO,
        );
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), Arc::clone(&today));

        for log in 1..=6 {
            extractor.enqueue(log);
        }
        settle(&extractor).await;
        assert_eq!(backend.calls(), 3);
        assert_eq!(spent(&extractor, day(25)), 30, "failed calls are charged");

        *today.lock().unwrap() = day(26);
        extractor.enqueue(7);
        settle(&extractor).await;
        assert_eq!(backend.calls(), 4);
    }

    /// A provider that cannot be located costs nothing and trips nothing:
    /// fixing the config is enough, no restart and no day boundary needed.
    #[tokio::test]
    async fn a_missing_provider_is_skipped_without_pausing() {
        let tmp = tempfile::tempdir().unwrap();
        let unavailable = || {
            Err(ExtractArchiveError::ProviderUnavailable(
                "config: grok not found on PATH".into(),
            ))
        };
        let backend = Scripted::new(
            vec![
                unavailable(),
                unavailable(),
                unavailable(),
                unavailable(),
                extracted(5),
            ],
            Duration::ZERO,
        );
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);

        for log in 1..=5 {
            extractor.enqueue(log);
        }
        settle(&extractor).await;

        assert_eq!(backend.calls(), 5, "never paused");
        assert_eq!(spent(&extractor, day(25)), 5);
        assert_eq!(extractor.inner.lock_queue().paused_on, None);
    }

    #[tokio::test]
    async fn a_success_resets_the_failure_count() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Scripted::new(
            vec![
                failed(),
                failed(),
                extracted(1),
                failed(),
                failed(),
                extracted(1),
            ],
            Duration::ZERO,
        );
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);
        for log in 1..=6 {
            extractor.enqueue(log);
        }
        settle(&extractor).await;
        assert_eq!(backend.calls(), 6);
    }

    #[tokio::test]
    async fn a_hung_extraction_times_out_as_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Scripted::new(vec![extracted(1)], Duration::from_secs(60));
        let today = Arc::new(Mutex::new(day(25)));
        let mut extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);
        Arc::get_mut(&mut extractor.inner).unwrap().archive_timeout = Duration::from_millis(50);

        extractor.enqueue(1);
        settle(&extractor).await;
        assert_eq!(extractor.inner.lock_queue().consecutive_failures, 1);
    }

    #[tokio::test]
    async fn the_queue_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Scripted::new(Vec::new(), Duration::from_millis(200));
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);

        for log in 0..(MAX_QUEUED as u32 + 20) {
            extractor.enqueue(log);
        }
        assert!(extractor.inner.lock_queue().pending.len() <= MAX_QUEUED);
    }

    #[test]
    fn an_unreadable_ledger_counts_from_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(tmp.path().join(LEDGER_FILE));
        let budget = DailyBudget { limit: 10 };
        std::fs::write(&ledger.path, "not json").unwrap();
        assert_eq!(budget.spent(&ledger, day(25)), 0);
        assert_eq!(budget.charge(&ledger, day(25), 4), 4);
        assert_eq!(budget.spent(&ledger, day(25)), 4);
        assert!(!budget.exhausted(&ledger, day(25)));
        budget.charge(&ledger, day(25), 6);
        assert!(budget.exhausted(&ledger, day(25)));
    }

    /// A ledger written before the watermark existed still carries today's
    /// spend, and has no watermark.
    #[test]
    fn a_pre_watermark_ledger_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(tmp.path().join(LEDGER_FILE));
        std::fs::write(&ledger.path, r#"{"day":"2026-09-25","tokens":1234}"#).unwrap();
        assert_eq!(DailyBudget { limit: 0 }.spent(&ledger, day(25)), 1_234);
        assert_eq!(ledger.watermark(), None);

        ledger.raise_watermark(7);
        ledger.raise_watermark(5);
        let state = ledger.read();
        assert_eq!(state.watermark, Some(7), "the watermark never goes down");
        assert_eq!(state.tokens, 1_234, "raising it keeps the spend");
    }

    #[test]
    fn archive_logs_reads_only_conversation_files_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = archives(tmp.path(), [10, 2, 1000]);
        std::fs::write(dir.join("conversation-x.md"), "").unwrap();
        std::fs::write(dir.join("notes.md"), "").unwrap();
        assert_eq!(archive_logs(&dir), vec![2, 10, 1000]);
        assert_eq!(archive_logs(&tmp.path().join("absent")), Vec::<u32>::new());
    }

    /// First boot: the historical backlog is not pulled in.
    #[tokio::test]
    async fn a_first_run_sets_the_watermark_to_the_newest_archive_and_queues_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = archives(tmp.path(), 1..=40);
        let backend = Scripted::new(Vec::new(), Duration::ZERO);
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);

        extractor.start_catch_up(&dir);
        settle(&extractor).await;

        assert_eq!(watermark(&extractor), Some(40));
        assert_eq!(backend.calls(), 0);
    }

    #[tokio::test]
    async fn a_first_run_with_no_archives_starts_at_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Scripted::new(vec![extracted(1)], Duration::ZERO);
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);

        extractor.start_catch_up(&tmp.path().join("archives/conversations"));
        assert_eq!(watermark(&extractor), Some(0));

        // The pulse's very first archive is then above the watermark.
        let dir = archives(tmp.path(), [1]);
        let restarted = self::extractor(
            tmp.path(),
            0,
            Arc::clone(&backend),
            Arc::new(Mutex::new(day(25))),
        );
        restarted.start_catch_up(&dir);
        settle(&restarted).await;
        assert_eq!(backend.seen(), vec![1]);
    }

    /// Boot: only archives above the watermark that the store still has
    /// pending, oldest first — an extracted one, or one that was never
    /// ingested (comms archives are not), costs nothing.
    #[tokio::test]
    async fn catch_up_queues_only_pending_archives_above_the_watermark_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = archives(tmp.path(), 1..=8);
        Ledger::new(tmp.path().join(LEDGER_FILE)).raise_watermark(3);
        let backend = Scripted::new(Vec::new(), Duration::ZERO).with_pending(Ok(vec![8, 2, 5, 7]));
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);

        extractor.start_catch_up(&dir);
        settle(&extractor).await;

        assert_eq!(backend.seen(), vec![5, 7, 8]);
        assert_eq!(watermark(&extractor), Some(8));
    }

    /// The watermark is on disk: a restart re-spends nothing, and still
    /// finds what arrived since.
    #[tokio::test]
    async fn the_watermark_survives_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = archives(tmp.path(), 1..=5);
        Ledger::new(tmp.path().join(LEDGER_FILE)).raise_watermark(2);
        let backend = Scripted::new(Vec::new(), Duration::ZERO);
        let first = extractor(
            tmp.path(),
            0,
            Arc::clone(&backend),
            Arc::new(Mutex::new(day(25))),
        );
        first.start_catch_up(&dir);
        settle(&first).await;
        assert_eq!(backend.seen(), vec![3, 4, 5]);

        // Restart with nothing new: nothing queued.
        let restarted = extractor(
            tmp.path(),
            0,
            Arc::clone(&backend),
            Arc::new(Mutex::new(day(25))),
        );
        restarted.start_catch_up(&dir);
        settle(&restarted).await;
        assert_eq!(backend.calls(), 3);

        // Archives written at the last shutdown are the next boot's work.
        archives(tmp.path(), [6, 7]);
        let again = extractor(
            tmp.path(),
            0,
            Arc::clone(&backend),
            Arc::new(Mutex::new(day(26))),
        );
        again.start_catch_up(&dir);
        settle(&again).await;
        assert_eq!(backend.seen(), vec![3, 4, 5, 6, 7]);
    }

    /// Queueing raises the watermark at once — before the extraction runs —
    /// so a crash mid-archive cannot make the next boot pay for it again.
    #[tokio::test]
    async fn queueing_raises_the_watermark_before_extracting() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Scripted::new(vec![extracted(1)], Duration::from_secs(5));
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);

        extractor.enqueue(12);
        assert_eq!(watermark(&extractor), Some(12));
        extractor.enqueue(9);
        assert_eq!(watermark(&extractor), Some(12));
    }

    /// More than the queue holds: the rest waits in the backlog and is
    /// drained behind it, still one at a time, still in order.
    #[tokio::test]
    async fn a_catch_up_larger_than_the_queue_is_drained_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let total = MAX_QUEUED as u32 * 2 + 5;
        let dir = archives(tmp.path(), 1..=total);
        Ledger::new(tmp.path().join(LEDGER_FILE)).raise_watermark(0);
        let backend = Scripted::new(Vec::new(), Duration::ZERO);
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);

        extractor.start_catch_up(&dir);
        settle(&extractor).await;

        assert_eq!(backend.seen(), (1..=total).collect::<Vec<_>>());
        assert_eq!(backend.max_in_flight.load(Ordering::SeqCst), 1);
        assert_eq!(watermark(&extractor), Some(total));
    }

    /// Catch-up spends under the same cap as everything else.
    #[tokio::test]
    async fn catch_up_respects_the_daily_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = archives(tmp.path(), 1..=5);
        Ledger::new(tmp.path().join(LEDGER_FILE)).raise_watermark(0);
        let backend = Scripted::new((0..5).map(|_| extracted(600)).collect(), Duration::ZERO);
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 1_000, Arc::clone(&backend), today);

        extractor.start_catch_up(&dir);
        settle(&extractor).await;

        assert_eq!(backend.calls(), 2);
    }

    /// A store that cannot be asked is not a reason to skip the catch-up:
    /// extraction itself costs nothing on an archive with nothing pending.
    #[tokio::test]
    async fn an_unreachable_store_queues_every_archive_above_the_watermark() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = archives(tmp.path(), 1..=4);
        Ledger::new(tmp.path().join(LEDGER_FILE)).raise_watermark(2);
        let backend = Scripted::new(Vec::new(), Duration::ZERO)
            .with_pending(Err("graph open: connection refused".into()));
        let today = Arc::new(Mutex::new(day(25)));
        let extractor = extractor(tmp.path(), 0, Arc::clone(&backend), today);

        extractor.start_catch_up(&dir);
        settle(&extractor).await;

        assert_eq!(backend.seen(), vec![3, 4]);
    }

    /// The extraction CLI sees the pulse's allowlisted environment, runs
    /// from a neutral directory, and gets `cli_bin` only when it is the same
    /// CLI the pulse chats with.
    #[test]
    fn overrides_follow_the_adapter_recall_echo_is_configured_with() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        let chat_adapter = crate::cli_provider::adapters::NAMES[1];
        let adapter = crate::cli_provider::adapters::by_name(chat_adapter).unwrap();
        let recall_provider = adapter.integration().recall_echo_provider();
        std::fs::write(
            memory.join(".recall-echo.toml"),
            format!("[llm]\nprovider = \"{recall_provider}\"\n"),
        )
        .unwrap();

        let mut config = crate::config::test_support::minimal_config();
        config.llm.provider = "cli".into();
        config.llm.adapter = Some(chat_adapter.into());
        config.llm.cli_bin = Some("/opt/agent/bin/cli".into());

        let overrides = cli_overrides(&config, tmp.path(), &memory);
        assert_eq!(overrides.command, Some(PathBuf::from("/opt/agent/bin/cli")));
        assert_eq!(overrides.current_dir, Some(PathBuf::from(NEUTRAL_DIR)));
        let env = overrides.env.expect("an explicit environment");
        assert!(env.iter().any(
            |(k, v)| k == crate::cli_provider::RECALL_ECHO_HOME && v == tmp.path().as_os_str()
        ));

        // Extraction on a different CLI than chat: search for its binary.
        config.llm.adapter = Some(crate::cli_provider::adapters::NAMES[2].into());
        assert_eq!(cli_overrides(&config, tmp.path(), &memory).command, None);
    }

    #[test]
    fn every_adapter_round_trips_through_its_recall_echo_provider() {
        for name in crate::cli_provider::adapters::NAMES {
            let adapter = crate::cli_provider::adapters::by_name(name).unwrap();
            let provider = adapter.integration().recall_echo_provider();
            let back = crate::cli_provider::adapters::by_recall_echo_provider(provider).unwrap();
            assert_eq!(back.name(), name);
        }
        assert!(crate::cli_provider::adapters::by_recall_echo_provider("anthropic").is_none());
    }
}
