//! One probe run (PN-144): gate, scratch, turns, scores, record, teardown.
//!
//! ```text
//!  gate ── refused ──▶ reason, nothing written
//!   │
//!  lock · truths · sweep · scratch · graph copy
//!   │      start row
//!   ▼
//!  for each set × drawn item × arm (a batched set asks all its items in
//!  one turn): item copy → system prompt (pulse / facts / none) → turn
//!                      → receipt, budget row → void scan of its tool calls
//!                      → rule scores → one item row per question
//!   │      quota error: stop, partial · first turn fails: stop, aborted
//!   ▼
//!  teardown (graph, scratch) · finish row
//! ```
//!
//! Probe turns never go through the daemon's turn path: no session, context
//! buffer, WAL or archive is touched. The runner writes only under the probe
//! user's private state (`record/probes/` there, not in the live home).

use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use pulse_system_types::llm::{Message, MessageContent, Role};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::cli_provider::adapter::CliAdapter;
use crate::cli_provider::sandbox::LandlockSupport;
use crate::cli_provider::{adapters, CapturedRun, CliProvider};
use crate::config::Config;
use crate::errors::QuotaError;

use super::facts;
use super::fence::{self, DenyList};
use super::graph::{self, GraphSource, ScratchGraph};
use super::observe;
use super::paired::{self, LoadedPairs};
use super::record::{self, Record, Stream, RECORD_VERSION};
use super::schedule::{self, GateInput, History, Refusal, RunStatus};
use super::score::{self, Outcome};
use super::scratch::{
    hex, sweep_under_host_lock, GraphTarget, ItemKind, ItemPlan, Scratch, ScratchSpec,
    SCRATCH_STALE_AFTER,
};
use super::sealed::{self, Registry};
use super::sets::{self, Arm, Delivery, Item, LoadedSet};
use super::truth::{TruthSources, Truths};

/// Longest answer text kept on an item row; the private evidence keeps it whole.
const ANSWER_MAX_BYTES: usize = 300;

/// What a run is asked to do.
#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    /// The scheduled trigger: run only if due.
    pub if_due: bool,
    /// Only these sets (by name or prefix); all when empty.
    pub only: Vec<String>,
    pub keep_scratch: bool,
}

/// Everything about the pulse and the world a run needs, resolved by the
/// caller (so tests can give a fixture pulse, a fixed clock and a fake CLI).
pub struct RunEnv<'a> {
    pub config: &'a Config,
    pub live_root: &'a Path,
    pub home: &'a Path,
    /// Where the probe record is rooted (`<dir>/record/probes`). The probe
    /// user's private state, not the live pulse home.
    pub record_root: &'a Path,
    pub scratch_parent: &'a Path,
    pub tz: Tz,
    pub now: DateTime<Utc>,
    pub landlock: LandlockSupport,
    pub recall_bin: &'a str,
    /// TCP ports a probe child may connect to besides the run's throwaway
    /// graph server (the vendor's HTTPS port).
    pub tcp_connect: &'a [u16],
    /// Live paths the probe child must not read; an install directory that
    /// would expose one refuses the run.
    pub deny: &'a DenyList,
    /// The live graph and how to copy it, when the pulse's graph is on a
    /// server.
    pub live_graph: Option<GraphSource>,
    /// Whether a variable is set (and non-empty) in the runner's environment.
    pub has_env: &'a dyn Fn(&str) -> bool,
    /// The digests the sealed item sets must match.
    pub registry: &'a Registry,
    /// Where the sealed sets are: the unit's credentials directory
    /// (`$CREDENTIALS_DIRECTORY`). `None` refuses the run.
    pub credentials: Option<&'a Path>,
    /// How long to wait for another pulse's run on this host to finish.
    pub host_lock_wait: std::time::Duration,
    /// `Ok` when the process is confined as the probe unit confines it
    /// ([`super::confine::require`]); a run that is not is refused.
    pub confinement: &'a dyn Fn() -> Result<(), String>,
    /// Set when the process is asked to stop (SIGTERM from systemd, SIGINT
    /// by hand): the run stops at once, tears down and writes its finish
    /// row.
    pub stop: &'a std::sync::atomic::AtomicBool,
    /// How long the run's turns may take, counted from when it holds its
    /// locks (the wait for the host lock is not in it; the unit's
    /// `TimeoutStartSec` covers both). No turn starts after it runs out.
    pub time_budget: std::time::Duration,
    /// The memory components a paired-fact answer depends on (the
    /// recall-echo version and graph mode): paired facts are grouped by
    /// these and the served model, not by the runtime's version.
    pub memory: &'a str,
}

/// How a run went.
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    Refused(String),
    Finished(RunSummary),
}

/// What a finished run spent and recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub run: String,
    pub status: RunStatus,
    pub reason: Option<String>,
    pub turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub items_recorded: u32,
    /// Things the owner must hear about (a vendor file changed in scratch).
    pub alerts: Vec<String>,
}

/// Why a run could not start (after the gate passed).
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{0}")]
    Setup(String),
    #[error("record: {0}")]
    Record(#[from] std::io::Error),
}

fn setup(e: impl std::fmt::Display) -> RunError {
    RunError::Setup(e.to_string())
}

/// Run the probes `request` asks for. Once the start row is written, every
/// way the run can stop (a quota, a failed turn, a setup or record error)
/// still tears the scratch and the throwaway graph down and writes the
/// finish row.
pub async fn run(env: &RunEnv<'_>, request: &RunRequest) -> Result<RunOutcome, RunError> {
    let started = std::time::Instant::now();
    sweep_stale_scratch(env.scratch_parent);
    let plan = match Plan::prepare(env, request).await? {
        Ok(plan) => plan,
        Err(refusal) => return Ok(RunOutcome::Refused(refusal)),
    };
    let scratch = Scratch::create(&ScratchSpec {
        live_root: env.live_root,
        live_home: env.home,
        pulse: &env.config.pulse.name,
        parent: env.scratch_parent,
        keep: request.keep_scratch,
    })
    .map_err(setup)?;
    let run_id = scratch.id().to_string();
    let cli_version = cli_version(env, &scratch, plan.adapter.as_ref(), &plan.read_roots).await;
    let lineage = Lineage::of(
        env.config,
        plan.adapter.as_ref(),
        env.memory,
        cli_version,
        plan.baseline,
    );
    plan.record.append(
        Stream::Runs,
        merged(
            json!({
                "v": RECORD_VERSION,
                "event": "start",
                "run": run_id,
                "at": env.now.to_rfc3339(),
                "pulse": env.config.pulse.name,
                "sets": plan.sets.iter().map(LoadedSet::label).collect::<Vec<_>>(),
                "paired_asks": plan.asks.len(),
                "planned_turns": plan.planned_turns,
                "if_due": request.if_due,
            }),
            lineage.fields(),
        ),
    )?;

    let mut tally = Tally::default();
    let graph_copy = graph_or_abort(env, &scratch, &mut tally).await;
    if tally.stopped.is_none() {
        let target = graph_copy
            .as_ref()
            .map_or(GraphTarget::Embedded, ScratchGraph::target);
        let mut tcp_connect = env.tcp_connect.to_vec();
        tcp_connect.extend(graph_copy.as_ref().map(ScratchGraph::port));
        let turns = TurnContext {
            env,
            record: &plan.record,
            scratch: &scratch,
            adapter: plan.adapter.as_ref(),
            graph: &target,
            tcp_connect: &tcp_connect,
            read_roots: &plan.read_roots,
            sources: &plan.sources,
            plan_truths: &plan.truths,
            started,
            deadline: std::time::Instant::now() + env.time_budget,
            graph_copy: graph_copy.as_ref(),
            lineage: &lineage,
            run_id: &run_id,
        };
        turns.ask_or_stop(&plan, &mut tally).await;
    }

    let graph_stats = tear_down(scratch, graph_copy, &mut tally).await;
    finish(env, &plan.record, &run_id, started, tally, graph_stats)
}

/// What a crashed run left goes first, refused or not, and only under the
/// host lock: the sweep removes stale scratch and kills its throwaway
/// server, so no other run may be live while it does.
fn sweep_stale_scratch(scratch_parent: &Path) {
    for stale in sweep_under_host_lock(scratch_parent, SCRATCH_STALE_AFTER) {
        tracing::info!("removed stale probe scratch {}", stale.display());
    }
}

/// The run's graph copy ([`prepare_graph`]); a copy that fails aborts the
/// run before any turn.
async fn graph_or_abort(
    env: &RunEnv<'_>,
    scratch: &Scratch,
    tally: &mut Tally,
) -> Option<ScratchGraph> {
    match prepare_graph(env, scratch).await {
        Ok(copy) => copy,
        Err(e) => {
            tally.stop(RunStatus::Aborted, format!("scratch graph: {e}"));
            None
        }
    }
}

/// Remove the throwaway graph and the scratch (an alert when the scratch
/// stays); the graph's load stats are for the finish row.
async fn tear_down(
    scratch: Scratch,
    graph_copy: Option<ScratchGraph>,
    tally: &mut Tally,
) -> Option<graph::LoadStats> {
    let graph_stats = graph_copy.as_ref().map(|copy| copy.stats);
    if let Some(copy) = graph_copy {
        copy.remove().await;
    }
    if let Err(e) = scratch.teardown() {
        tally.alerts.push(format!("scratch not removed: {e}"));
    }
    graph_stats
}

/// Why a turn did not run to its end: the run was asked to stop, ran out of
/// time, or lost its throwaway graph.
#[derive(Debug)]
struct Interrupted {
    reason: String,
    /// The turn's CLI had already started (it may have spent tokens).
    mid_turn: bool,
}

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for Interrupted {}

/// Why the run was asked to stop, as the finish row says it.
const SIGNALLED: &str = "stopped by a signal (SIGTERM or SIGINT)";

/// Resolves once `stop` is set.
async fn stop_requested(stop: &std::sync::atomic::AtomicBool) {
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// The peak memory of this process's cgroup (the whole probe unit under
/// systemd: runner, CLI children, the throwaway graph), from cgroup v2's
/// `memory.peak`. `None` where it cannot be read.
fn cgroup_memory_peak() -> Option<u64> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup.lines().find_map(|l| l.strip_prefix("0::"))?;
    let peak =
        std::fs::read_to_string(format!("/sys/fs/cgroup{}/memory.peak", path.trim())).ok()?;
    peak.trim().parse().ok()
}

/// Write the finish row and summarise the run.
fn finish(
    env: &RunEnv<'_>,
    record: &Record,
    run_id: &str,
    started: std::time::Instant,
    tally: Tally,
    graph: Option<graph::LoadStats>,
) -> Result<RunOutcome, RunError> {
    let (status, reason) = tally.stopped.clone().unwrap_or((RunStatus::Complete, None));
    record.append(
        Stream::Runs,
        json!({
            "v": RECORD_VERSION,
            "event": "finish",
            "run": run_id,
            "at": (env.now + elapsed(started)).to_rfc3339(),
            "elapsed_s": started.elapsed().as_secs(),
            "status": status,
            "reason": reason,
            "turns": tally.turns,
            "input_tokens": tally.input_tokens,
            "output_tokens": tally.output_tokens,
            "items": tally.items,
            "alerts": tally.alerts,
            "graph": graph,
            "memory_peak_bytes": cgroup_memory_peak(),
        }),
    )?;
    Ok(RunOutcome::Finished(RunSummary {
        run: run_id.to_string(),
        status,
        reason,
        turns: tally.turns,
        input_tokens: tally.input_tokens,
        output_tokens: tally.output_tokens,
        items_recorded: tally.items,
        alerts: tally.alerts,
    }))
}

/// What a run will do, decided before anything is written: the sets, the
/// paired asks (read under the lock), the CLI and its read fence, and the
/// held locks.
struct Plan {
    sets: Vec<LoadedSet>,
    /// The run's baseline label, derived from the prompt builder.
    baseline: &'static str,
    /// The runtime's stores, read once; truths are derived per turn.
    sources: TruthSources,
    /// The truths at the start, which decide what is drawn.
    truths: Truths,
    pairs: LoadedPairs,
    asks: Vec<paired::Ask>,
    planned_turns: u32,
    adapter: Box<dyn CliAdapter>,
    read_roots: Vec<std::path::PathBuf>,
    record: Record,
    _host: super::record::RunLock,
    _lock: super::record::RunLock,
}

impl Plan {
    /// Load and check everything, take the locks and pass the gate, or say
    /// why the run is refused (nothing written).
    async fn prepare(
        env: &RunEnv<'_>,
        request: &RunRequest,
    ) -> Result<Result<Self, String>, RunError> {
        match Self::prepare_or_stop(env, request).await {
            Ok(plan) => Ok(Ok(plan)),
            Err(Stop::Refused(refusal)) => Ok(Err(refusal)),
            Err(Stop::Failed(e)) => Err(e),
        }
    }

    async fn prepare_or_stop(env: &RunEnv<'_>, request: &RunRequest) -> Result<Self, Stop> {
        let planner = Planner::new(env, request)?;
        let started = planner.first_look()?;
        let read_roots = planner.run_checks()?;
        let (host, record, lock) = planner.take_locks().await?;
        let now = planner.under_lock(&record, started)?;
        Ok(Self {
            sets: planner.sets,
            baseline: planner.baseline,
            sources: now.sources,
            truths: now.truths,
            pairs: planner.pairs,
            asks: now.asks,
            planned_turns: now.planned_turns,
            adapter: planner.adapter,
            read_roots,
            record,
            _host: host,
            _lock: lock,
        })
    }
}

/// Why a plan was not made: the run is refused (the reason is the outcome),
/// or it failed.
enum Stop {
    Refused(String),
    Failed(RunError),
}

impl From<RunError> for Stop {
    fn from(e: RunError) -> Self {
        Self::Failed(e)
    }
}

impl From<std::io::Error> for Stop {
    fn from(e: std::io::Error) -> Self {
        Self::Failed(e.into())
    }
}

/// A refusal, as the outcome says it.
fn refused(refusal: impl std::fmt::Display) -> Stop {
    Stop::Refused(refusal.to_string())
}

/// What the sealed sets, the config and `--only` decide, before any look at
/// the record.
struct Planner<'a> {
    env: &'a RunEnv<'a>,
    request: &'a RunRequest,
    wants_paired: bool,
    sets: Vec<LoadedSet>,
    pairs: LoadedPairs,
    adapter: Box<dyn CliAdapter>,
    baseline: &'static str,
}

/// The state under the locks: the runtime's stores and the facts as they
/// are now, and the turns that come to.
struct UnderLock {
    sources: TruthSources,
    truths: Truths,
    asks: Vec<paired::Ask>,
    planned_turns: u32,
}

impl<'a> Planner<'a> {
    fn new(env: &'a RunEnv<'a>, request: &'a RunRequest) -> Result<Self, Stop> {
        let installed = installed_sets(env)?;
        let (wants_paired, set_names) = split_only(&request.only);
        let sets = sets_to_ask(&request.only, &set_names, &installed)?;
        let pairs = LoadedPairs::from_installed(&installed).map_err(setup)?;
        let adapter = adapter_for(env.config)?;
        let baseline = super::baseline(env.config).map_err(Stop::Refused)?;
        Ok(Self {
            env,
            request,
            wants_paired,
            sets,
            pairs,
            adapter,
            baseline,
        })
    }

    /// A first look without the lock, so a refused run touches nothing.
    /// Returns when the look was taken: the clock under the lock counts
    /// from there.
    fn first_look(&self) -> Result<std::time::Instant, Stop> {
        let env = self.env;
        let existing = Record::existing(env.record_root);
        let first_asks = self.asks_in(existing.as_ref(), env.now)?;
        let first_truths = Truths::at(
            &TruthSources::read(env.live_root, env.config),
            env.tz,
            env.now,
        );
        let started = std::time::Instant::now();
        self.gate(
            &History::from_record(existing.as_ref())?,
            self.planned(&first_asks, &first_truths),
            env.now,
        )
        .map_err(refused)?;
        Ok(started)
    }

    /// The checks that do not need the record: the process is confined like
    /// the unit, the CLI can log in, and the read fence can be built.
    /// Returns the fence's read roots.
    fn run_checks(&self) -> Result<Vec<std::path::PathBuf>, Stop> {
        let env = self.env;
        (env.confinement)().map_err(Stop::Refused)?;
        self.adapter
            .probe_auth_ready(env.has_env)
            .map_err(Stop::Refused)?;
        let bin = env
            .config
            .llm
            .cli_bin
            .clone()
            .unwrap_or_else(|| self.adapter.default_bin().to_string());
        fence::read_roots(self.adapter.as_ref(), &bin, env.home, env.deny).map_err(refused)
    }

    /// The host lock, then the record's own.
    async fn take_locks(
        &self,
    ) -> Result<(super::record::RunLock, Record, super::record::RunLock), Stop> {
        let env = self.env;
        let Some(host) = host_lock(env.scratch_parent, env.host_lock_wait, env.stop).await? else {
            if env.stop.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Stop::Refused(format!(
                    "{SIGNALLED} while waiting for the host lock"
                )));
            }
            return Err(refused(Refusal::HostBusy));
        };
        let record = Record::open(env.record_root)?;
        let Some(lock) = record.try_lock()? else {
            return Err(refused(Refusal::Locked));
        };
        Ok((host, record, lock))
    }

    /// Under the lock: the facts, the history and the runtime's stores as
    /// they are now (another run, a draw or the owner may have written since
    /// the first look, during a wait of up to 90 minutes for the host lock),
    /// and the clock after that wait.
    fn under_lock(&self, record: &Record, started: std::time::Instant) -> Result<UnderLock, Stop> {
        let env = self.env;
        let now = env.now + elapsed(started);
        let sources = TruthSources::read(env.live_root, env.config);
        let truths = Truths::at(&sources, env.tz, now);
        let mut asks = self.asks_in(Some(record), now)?;
        paired::mark_contaminated(&mut asks, env.live_root);
        let planned_turns = self.planned(&asks, &truths);
        self.gate(&History::from_record(Some(record))?, planned_turns, now)
            .map_err(refused)?;
        Ok(UnderLock {
            sources,
            truths,
            asks,
            planned_turns,
        })
    }

    /// The paired asks due at `now` in `record`'s facts (none when paired
    /// facts are not wanted).
    fn asks_in(
        &self,
        record: Option<&Record>,
        now: DateTime<Utc>,
    ) -> Result<Vec<paired::Ask>, RunError> {
        if !self.wants_paired {
            return Ok(Vec::new());
        }
        let rows = match record {
            Some(record) => record.read(Stream::Facts)?.rows,
            None => Vec::new(),
        };
        Ok(paired::due(
            &self.pairs,
            &paired::facts_from_rows(&rows, &self.pairs),
            now,
        ))
    }

    /// The turns the sets and `asks` come to, with `truths` deciding the
    /// draw.
    fn planned(&self, asks: &[paired::Ask], truths: &Truths) -> u32 {
        self.sets
            .iter()
            .map(|set| set.turns_for(&draw(set, "plan", truths)))
            .sum::<u32>()
            + u32::try_from(asks.len()).unwrap_or(u32::MAX)
    }

    fn gate(
        &self,
        history: &History,
        planned_turns: u32,
        now: DateTime<Utc>,
    ) -> Result<(), Refusal> {
        let env = self.env;
        schedule::check(&GateInput {
            config: &env.config.probe,
            tz: env.tz,
            now,
            if_due: self.request.if_due,
            isolated: crate::server::isolation::is_active(env.live_root),
            landlock: env.landlock,
            history,
            planned_turns,
        })
    }
}

/// The sealed sets from the unit's credentials, or why the run is refused.
fn installed_sets(env: &RunEnv<'_>) -> Result<sealed::InstalledSets, Stop> {
    let loaded = match env.credentials {
        Some(dir) => sealed::load(dir, env.registry),
        None => Err(sealed::SealError::NoCredentials),
    };
    loaded.map_err(refused)
}

/// The item sets `--only` names (all when it names none; none when it names
/// only paired facts).
fn sets_to_ask(
    only: &[String],
    set_names: &[String],
    installed: &sealed::InstalledSets,
) -> Result<Vec<LoadedSet>, RunError> {
    if set_names.is_empty() && !only.is_empty() {
        return Ok(Vec::new());
    }
    sets::select(set_names, installed).map_err(setup)
}

/// A built prompt's components for a receipt: in order, with their tokens,
/// and the ones the budget dropped.
fn prompt_components(built: &crate::server::prompt::SystemPromptResult) -> serde_json::Value {
    json!({
        "included": built
            .components
            .iter()
            .map(|(name, tokens)| json!({"name": name, "tokens": tokens}))
            .collect::<Vec<_>>(),
        "dropped": built
            .dropped_components
            .iter()
            .map(|(name, tokens)| json!({"name": name, "tokens": tokens}))
            .collect::<Vec<_>>(),
        "estimated_tokens": built.estimated_tokens,
    })
}

/// Time since `started`, as a chrono duration.
fn elapsed(started: std::time::Instant) -> chrono::Duration {
    chrono::Duration::from_std(started.elapsed()).unwrap_or_else(|_| chrono::Duration::zero())
}

/// The items of `set` a run asks: only those the run's truths can score.
fn draw<'s>(set: &'s LoadedSet, run_id: &str, truths: &Truths) -> Vec<&'s Item> {
    set.draw_scorable(run_id, |key| truths.can_score(key))
}

/// The host lock: one probe run at a time on this host, whichever pulse
/// (both share one AI account and one disk). Waits up to `wait` for another
/// run to finish, polling.
async fn host_lock(
    scratch_parent: &Path,
    wait: std::time::Duration,
    stop: &std::sync::atomic::AtomicBool,
) -> std::io::Result<Option<super::record::RunLock>> {
    const POLL: std::time::Duration = std::time::Duration::from_secs(10);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(scratch_parent)?;
    let path = scratch_parent.join(super::scratch::HOST_LOCK);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Some(lock) = super::record::try_lock_file(&path)? {
            return Ok(Some(lock));
        }
        // A signal during the wait ends it: the handler took SIGTERM, so
        // nothing else would.
        if tokio::time::Instant::now() >= deadline || stop.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(None);
        }
        let nap = POLL.min(deadline - tokio::time::Instant::now());
        tokio::select! {
            () = tokio::time::sleep(nap) => {}
            () = stop_requested(stop) => {}
        }
    }
}

/// Split `--only` into "paired facts wanted" and the item-set names. Paired
/// facts are wanted when nothing is named, or a name is a prefix of
/// `paired-facts`.
fn split_only(only: &[String]) -> (bool, Vec<String>) {
    let is_paired = |name: &String| "paired-facts".starts_with(name.as_str()) && !name.is_empty();
    let wants_paired = only.is_empty() || only.iter().any(is_paired);
    let names = only.iter().filter(|n| !is_paired(n)).cloned().collect();
    (wants_paired, names)
}

/// The pulse's agent-CLI adapter, or why probes cannot run on it.
fn adapter_for(config: &Config) -> Result<Box<dyn CliAdapter>, RunError> {
    let name = config.llm.cli_adapter().ok_or_else(|| {
        setup("probes need an agent CLI pulse ([llm] provider = \"cli\" with an adapter)")
    })?;
    adapters::by_name(name).ok_or_else(|| setup(format!("unknown cli adapter '{name}'")))
}

/// The agent CLI's `--version`, run the way a probe turn runs it (a bare
/// item, the same sandbox): `unknown` when it prints nothing, and the
/// failure when it cannot start (the first turn then fails too).
async fn cli_version(
    env: &RunEnv<'_>,
    scratch: &Scratch,
    adapter: &dyn CliAdapter,
    read_roots: &[std::path::PathBuf],
) -> String {
    let item = match scratch.item(
        0,
        ItemKind::Bare,
        &ItemPlan {
            adapter,
            recall_bin: env.recall_bin,
            graph: &GraphTarget::Embedded,
            tcp_connect: env.tcp_connect,
            read_roots,
        },
    ) {
        Ok(item) => item,
        Err(e) => return format!("unavailable: {e}"),
    };
    let Some(boxed) = adapters::by_name(adapter.name()) else {
        return "unavailable: adapter vanished".into();
    };
    let provider = CliProvider::new(
        boxed,
        env.config.llm.cli_bin.clone(),
        env.config.llm.model.clone(),
        item.root().to_path_buf(),
    )
    .with_overrides(item.overrides());
    let version = match provider.cli_version().await {
        Ok(version) if version.is_empty() => "unknown".to_string(),
        Ok(version) => version,
        Err(e) => format!(
            "unavailable: {}",
            crate::utils::safe_truncate(&e.to_string(), 120)
        ),
    };
    item.finish();
    version
}

/// Copy the live graph for this run onto a throwaway server. `None` when
/// the pulse's graph is not on a server.
async fn prepare_graph(
    env: &RunEnv<'_>,
    scratch: &Scratch,
) -> Result<Option<ScratchGraph>, graph::GraphError> {
    match &env.live_graph {
        Some(graph) => Ok(Some(graph.copy_for(&env.config.pulse.name, scratch).await?)),
        None => Ok(None),
    }
}

/// What a result was produced by (G6, G7): on every row, and results are
/// never pooled across a change in any of these.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lineage {
    adapter: String,
    configured_model: String,
    reasoning_effort: Option<String>,
    /// The agent CLI's own `--version` line.
    cli_version: String,
    binary: String,
    /// The commit the binary was built from.
    commit: String,
    scorer_version: u32,
    memory: String,
    baseline: &'static str,
    facts_sha256: String,
    /// The fresh arm's one-line system prompt.
    fresh_sha256: String,
    /// The general measurement disclosure in every pulse prompt.
    disclosure_sha256: String,
}

impl Lineage {
    fn of(
        config: &Config,
        adapter: &dyn CliAdapter,
        memory: &str,
        cli_version: String,
        baseline: &'static str,
    ) -> Self {
        Self {
            adapter: adapter.name().to_string(),
            configured_model: config.llm.model.clone(),
            reasoning_effort: config.llm.reasoning_effort.clone(),
            cli_version,
            binary: format!("pulse-null {}", env!("CARGO_PKG_VERSION")),
            commit: env!("PULSE_NULL_GIT_COMMIT").to_string(),
            scorer_version: score::SCORER_VERSION,
            memory: memory.to_string(),
            baseline,
            facts_sha256: sets::facts_sha256(),
            fresh_sha256: sets::fresh_sha256(),
            disclosure_sha256: hex(&Sha256::digest(
                crate::server::prompt::MEASUREMENT_DISCLOSURE.as_bytes(),
            )),
        }
    }

    /// The lineage fields of a row.
    fn fields(&self) -> serde_json::Value {
        json!({
            "adapter": self.adapter,
            "configured_model": self.configured_model,
            "reasoning_effort": self.reasoning_effort,
            "cli_version": self.cli_version,
            "binary": self.binary,
            "commit": self.commit,
            "scorer_version": self.scorer_version,
            "memory": self.memory,
            "baseline": self.baseline,
            "facts_sha256": self.facts_sha256,
            "fresh_sha256": self.fresh_sha256,
            "disclosure_sha256": self.disclosure_sha256,
        })
    }
}

/// `row` with `extra`'s fields added (an existing key is overwritten).
fn merged(mut row: serde_json::Value, extra: serde_json::Value) -> serde_json::Value {
    if let (Some(row), serde_json::Value::Object(extra)) = (row.as_object_mut(), extra) {
        row.extend(extra);
    }
    row
}

/// Running totals, and why the run stopped early, if it did.
#[derive(Debug, Default)]
struct Tally {
    turns: u32,
    succeeded: u32,
    input_tokens: u64,
    output_tokens: u64,
    items: u32,
    alerts: Vec<String>,
    stopped: Option<(RunStatus, Option<String>)>,
}

impl Tally {
    fn stop(&mut self, status: RunStatus, reason: String) {
        self.stopped = Some((status, Some(reason)));
    }

    /// How a run stopped early ends: partial once a turn completed, else
    /// aborted.
    fn stopped_status(&self) -> RunStatus {
        if self.succeeded > 0 {
            RunStatus::Partial
        } else {
            RunStatus::Aborted
        }
    }

    /// Alert on the copied vendor files a turn's CLI changed.
    fn alert_changed(&mut self, teardown: super::scratch::ItemTeardown) {
        for changed in teardown.changed_vendor_files {
            self.alerts.push(format!(
                "{} changed during a probe turn; it was not copied back",
                changed.display()
            ));
        }
    }
}

/// Everything one turn needs that does not change between turns.
struct TurnContext<'a> {
    env: &'a RunEnv<'a>,
    record: &'a Record,
    scratch: &'a Scratch,
    adapter: &'a dyn CliAdapter,
    graph: &'a GraphTarget,
    tcp_connect: &'a [u16],
    read_roots: &'a [std::path::PathBuf],
    sources: &'a TruthSources,
    /// The truths at the start of the run, which decided the draw.
    plan_truths: &'a Truths,
    started: std::time::Instant,
    /// No turn starts after this.
    deadline: std::time::Instant,
    /// The run's throwaway graph, checked before each pulse turn.
    graph_copy: Option<&'a ScratchGraph>,
    lineage: &'a Lineage,
    run_id: &'a str,
}

impl TurnContext<'_> {
    /// Why the next turn must not start, if it must not: a stop request,
    /// the time budget spent, or (for a turn that uses the graph) a
    /// throwaway graph that no longer answers.
    async fn stop_reason(&self, arm: Arm) -> Option<String> {
        if self.env.stop.load(std::sync::atomic::Ordering::SeqCst) {
            return Some(SIGNALLED.to_string());
        }
        if std::time::Instant::now() >= self.deadline {
            return Some(format!(
                "the run's time budget ({} min) ran out",
                self.env.time_budget.as_secs() / 60
            ));
        }
        if arm == Arm::Pulse {
            if let Some(copy) = self.graph_copy {
                if !copy.alive().await {
                    return Some("the throwaway graph stopped answering".to_string());
                }
            }
        }
        None
    }

    /// Now, for this turn: the run's start plus the time since.
    fn now(&self) -> DateTime<Utc> {
        self.env.now + elapsed(self.started)
    }

    /// The truths at this moment (the clock's are this turn's).
    fn truths_now(&self) -> Truths {
        Truths::at(self.sources, self.env.tz, self.now())
    }

    /// Every turn of the plan, in order: item sets (per-item sets item by
    /// item, every arm of an item before the next, so a run stopped by a
    /// quota keeps its arms balanced), then each due fact's planted side,
    /// twin and chance ask. Returns at the first stop; an error is the caller's to record.
    async fn ask_everything(&self, plan: &Plan, tally: &mut Tally) -> Result<(), RunError> {
        self.ask_item_sets(plan, tally).await?;
        if tally.stopped.is_some() {
            return Ok(());
        }
        self.ask_paired_facts(plan, tally).await
    }

    /// [`ask_everything`](Self::ask_everything), with an error stopping the
    /// run (partial, or aborted when no turn had completed).
    async fn ask_or_stop(&self, plan: &Plan, tally: &mut Tally) {
        if let Err(e) = self.ask_everything(plan, tally).await {
            tally.stop(tally.stopped_status(), format!("stopped by an error: {e}"));
        }
    }

    /// Every item set's turns; returns at the first stop.
    async fn ask_item_sets(&self, plan: &Plan, tally: &mut Tally) -> Result<(), RunError> {
        for set in &plan.sets {
            for (items, prompt) in &questions(set, self.run_id, self.plan_truths) {
                for arm in &set.set.arms {
                    let asked: Vec<&Item> = items
                        .iter()
                        .copied()
                        .filter(|item| set.asks(*arm, item))
                        .collect();
                    if asked.is_empty() {
                        continue;
                    }
                    let ceiling;
                    let prompt = if *arm == Arm::InputOnly && set.set.delivery == Delivery::Batched
                    {
                        ceiling = set.prompt_for(&asked);
                        ceiling.as_str()
                    } else {
                        prompt.as_str()
                    };
                    self.run_turn(set, *arm, &asked, prompt, tally).await?;
                    if tally.stopped.is_some() {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// Every due fact's asks; returns at the first stop.
    async fn ask_paired_facts(&self, plan: &Plan, tally: &mut Tally) -> Result<(), RunError> {
        for fact in plan.asks.chunks(paired::ASKS_PER_FACT) {
            let [planted, twin, chance, guess] = fact else {
                continue;
            };
            let asks = [planted, twin, chance, guess];
            if self.ask_fact(&plan.pairs, asks, tally).await?.is_break() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// One due fact: its planted side, then the twin and the fresh arm's
    /// two asks. Breaks when the run stopped.
    async fn ask_fact(
        &self,
        pairs: &LoadedPairs,
        [planted_ask, twin_ask, chance_ask, guess_ask]: [&paired::Ask; paired::ASKS_PER_FACT],
        tally: &mut Tally,
    ) -> Result<std::ops::ControlFlow<()>, RunError> {
        let planted = self.run_paired(pairs, planted_ask, tally).await?;
        if planted.is_none() {
            return Ok(std::ops::ControlFlow::Break(()));
        }
        let ([twin, chance, guess], failure) = self
            .ask_later_sides(pairs, [twin_ask, chance_ask, guess_ask], tally)
            .await;
        // Once its planted side has been asked the offset is used up, even
        // if the run stopped, or failed, before a later ask: asking it again
        // would count the planted side twice.
        self.record_asked(planted_ask, [planted, twin, chance, guess])?;
        if let Some(e) = failure {
            return Err(e);
        }
        if tally.stopped.is_some() {
            return Ok(std::ops::ControlFlow::Break(()));
        }
        Ok(std::ops::ControlFlow::Continue(()))
    }

    /// The twin and the fresh arm's asks of a fact whose planted side was
    /// asked: their outcomes, and the error that ended them, if one did.
    async fn ask_later_sides(
        &self,
        pairs: &LoadedPairs,
        asks: [&paired::Ask; 3],
        tally: &mut Tally,
    ) -> ([Option<Outcome>; 3], Option<RunError>) {
        let mut later = [None, None, None];
        for (slot, ask) in later.iter_mut().zip(asks) {
            if tally.stopped.is_some() {
                break;
            }
            match self.run_paired(pairs, ask, tally).await {
                Ok(outcome) => *slot = outcome,
                Err(e) => return (later, Some(e)),
            }
        }
        (later, None)
    }

    /// One turn of `set` for `arm`, asking `items` with `prompt`: ask, then
    /// record the answers (or the failure).
    async fn run_turn(
        &self,
        set: &LoadedSet,
        arm: Arm,
        items: &[&Item],
        prompt: &str,
        tally: &mut Tally,
    ) -> Result<(), RunError> {
        let asked = self.ask(arm, prompt, tally).await?;
        let turn = Turn {
            set,
            arm,
            items,
            prompt_sha256: &asked.prompt_sha256,
            components: &asked.components,
        };
        match self.settle(&set.label(), arm, asked.result, tally)? {
            Settled::Answered(captured) => {
                self.record_success(&turn, &asked.question, &captured, tally)
            }
            Settled::Failed(error) => self.record_error(&turn, &error, tally),
            Settled::Stopped => Ok(()),
        }
    }

    /// Prepare a scratch item for `arm`, build its system prompt, ask
    /// `prompt` once, and tear the item down.
    async fn ask(&self, arm: Arm, prompt: &str, tally: &mut Tally) -> Result<Asked, RunError> {
        if let Some(reason) = self.stop_reason(arm).await {
            // Counted, then given back by `settle`: no turn was spent.
            tally.turns += 1;
            return Ok(Asked::interrupted(reason));
        }
        let index = usize::try_from(tally.turns).unwrap_or(usize::MAX);
        let item = self
            .scratch
            .item(
                index,
                item_kind(arm),
                &ItemPlan {
                    adapter: self.adapter,
                    recall_bin: self.env.recall_bin,
                    graph: self.graph,
                    tcp_connect: self.tcp_connect,
                    read_roots: self.read_roots,
                },
            )
            .map_err(setup)?;
        let (system_prompt, components) = self.system_prompt(arm, item.root())?;
        let provider = CliProvider::new(
            adapters::by_name(self.adapter.name()).ok_or_else(|| setup("adapter vanished"))?,
            self.env.config.llm.cli_bin.clone(),
            self.env.config.llm.model.clone(),
            item.root().to_path_buf(),
        )
        .with_reasoning_effort(self.env.config.llm.reasoning_effort.clone())
        .with_overrides(item.overrides());
        let question = crate::server::prompt::compose_user_message(false, None, prompt);
        let message = Message {
            role: Role::User,
            content: MessageContent::Text(question.clone()),
            source: None,
        };
        // A stop request ends the turn too: dropping the turn kills the
        // CLI and its process group.
        let messages = [message];
        let result = tokio::select! {
            result = super::probe_turn(&provider, &system_prompt, &messages) => result,
            () = stop_requested(self.env.stop) => Err(Box::new(Interrupted {
                reason: SIGNALLED.to_string(),
                mid_turn: true,
            }) as Box<dyn std::error::Error + Send + Sync>),
        };
        tally.alert_changed(item.finish());
        tally.turns += 1;
        Ok(Asked {
            question,
            prompt_sha256: hex(&Sha256::digest(system_prompt.as_bytes())),
            components,
            result,
        })
    }

    /// `arm`'s system prompt for an item at `item_root`, and what it was made
    /// of (receipts).
    fn system_prompt(
        &self,
        arm: Arm,
        item_root: &Path,
    ) -> Result<(String, serde_json::Value), RunError> {
        match arm {
            Arm::Pulse => {
                let built =
                    crate::server::prompt::build_system_prompt_budgeted(item_root, self.env.config)
                        .map_err(setup)?;
                let components = prompt_components(&built);
                Ok((built.prompt, components))
            }
            Arm::InputOnly => Ok((
                facts::render(
                    &self.truths_now(),
                    &self.env.config.scheduler.timezone,
                    self.now(),
                ),
                json!([{"name": "facts-template", "sha256": sets::facts_sha256()}]),
            )),
            Arm::Fresh => Ok((
                sets::FRESH_SYSTEM_V1.to_string(),
                json!([{"name": "fresh-system", "sha256": sets::fresh_sha256()}]),
            )),
        }
    }

    /// Account for one turn's result: a budget row for anything the model
    /// spent, a stop at the first quota error (partial, or aborted when no
    /// turn had completed), an abort when the first turn fails.
    fn settle(
        &self,
        set_label: &str,
        arm: Arm,
        result: Result<CapturedRun, Box<dyn std::error::Error + Send + Sync>>,
        tally: &mut Tally,
    ) -> Result<Settled, RunError> {
        match result {
            Ok(captured) => self.settle_answered(set_label, arm, captured, tally),
            Err(e) if e.downcast_ref::<Interrupted>().is_some() => {
                self.settle_interrupted(set_label, arm, &*e, tally)
            }
            Err(e) if e.downcast_ref::<QuotaError>().is_some() => {
                // The quota refused the turn: nothing was spent.
                tally.turns -= 1;
                tally.stop(tally.stopped_status(), format!("quota: {e}"));
                Ok(Settled::Stopped)
            }
            Err(e) => self.settle_failed(set_label, arm, &*e, tally),
        }
    }

    fn settle_answered(
        &self,
        set_label: &str,
        arm: Arm,
        captured: CapturedRun,
        tally: &mut Tally,
    ) -> Result<Settled, RunError> {
        tally.succeeded += 1;
        self.record_budget(set_label, arm, Some(&captured))?;
        tally.input_tokens += u64::from(captured.usage.input_tokens.unwrap_or(0));
        tally.output_tokens += u64::from(captured.usage.output_tokens.unwrap_or(0));
        Ok(Settled::Answered(captured))
    }

    /// A stop request, the time budget or a lost graph ended the turn.
    fn settle_interrupted(
        &self,
        set_label: &str,
        arm: Arm,
        error: &(dyn std::error::Error + Send + Sync + 'static),
        tally: &mut Tally,
    ) -> Result<Settled, RunError> {
        let mid_turn = error
            .downcast_ref::<Interrupted>()
            .is_some_and(|i| i.mid_turn);
        if mid_turn {
            // The CLI ran: it may have spent tokens, so the turn counts
            // against the budget (with unknown tokens).
            self.record_budget(set_label, arm, None)?;
        } else {
            tally.turns -= 1;
        }
        tally.stop(tally.stopped_status(), error.to_string());
        Ok(Settled::Stopped)
    }

    /// The turn failed: the first failure aborts the run, a later one is
    /// recorded against its items.
    fn settle_failed(
        &self,
        set_label: &str,
        arm: Arm,
        error: &(dyn std::error::Error + Send + Sync + 'static),
        tally: &mut Tally,
    ) -> Result<Settled, RunError> {
        self.record_budget(set_label, arm, None)?;
        if tally.succeeded == 0 {
            tally.stop(RunStatus::Aborted, format!("first turn failed: {error}"));
            return Ok(Settled::Stopped);
        }
        Ok(Settled::Failed(error.to_string()))
    }

    fn record_budget(
        &self,
        set_label: &str,
        arm: Arm,
        run: Option<&CapturedRun>,
    ) -> std::io::Result<()> {
        self.record.append(
            Stream::Budget,
            json!({
                "v": RECORD_VERSION,
                "at": self.now().to_rfc3339(),
                "run": self.run_id,
                "set": set_label,
                "arm": arm,
                "input_tokens": run.and_then(|r| r.usage.input_tokens),
                "output_tokens": run.and_then(|r| r.usage.output_tokens),
            }),
        )?;
        Ok(())
    }

    /// The void reason for a turn's tool calls, when the CLI shows them.
    fn void_of(&self, captured: &CapturedRun) -> Option<String> {
        if self.adapter.observes_tools() {
            observe::void_reason(&captured.tool_calls)
        } else {
            None
        }
    }

    fn record_success(
        &self,
        turn: &Turn<'_>,
        question: &str,
        captured: &CapturedRun,
        tally: &mut Tally,
    ) -> Result<(), RunError> {
        let receipt_name = match turn.set.set.delivery {
            Delivery::Batched => format!("{}-{}", turn.set.set.set, turn.arm.as_str()),
            Delivery::PerItem => format!(
                "{}-{}-{}",
                turn.set.set.set,
                turn.items.first().map_or("item", |i| i.id.as_str()),
                turn.arm.as_str()
            ),
        };
        let void = self.void_of(captured);
        let truths = self
            .truths_now()
            .with_served_model(captured.served_model.as_deref());
        let answers = match turn.set.set.delivery {
            Delivery::Batched => score::split_batched(&captured.reply, turn.items.len()),
            Delivery::PerItem => vec![Some(captured.reply.trim().to_string())],
        };
        let mut scored = Vec::with_capacity(turn.items.len());
        for (item, answer) in turn.items.iter().zip(answers) {
            let truth = truths.get(item.truth);
            let outcome = if void.is_some() {
                Outcome::Void
            } else {
                score::score(item.rule, truth, answer.as_deref())
            };
            scored.push((
                item,
                answer,
                truth.map(|t| t.value.clone()),
                truth.map(|t| t.source),
                outcome,
            ));
        }
        let scores: Vec<serde_json::Value> = scored
            .iter()
            .map(|(_, _, _, _, outcome)| json!(outcome))
            .collect();
        // The question, the raw CLI output and the truths stay in the
        // probe-private directory. The receipt keeps a digest and the score.
        self.record.write_private(
            self.run_id,
            &receipt_name,
            &json!({
                "run": self.run_id,
                "set": turn.set.label(),
                "set_sha256": turn.set.sha256,
                "arm": turn.arm,
                "system_prompt_sha256": turn.prompt_sha256,
                "system_prompt_components": turn.components,
                "question": question,
                "reply": captured.reply,
                "served_model": captured.served_model,
                "tool_calls": captured.tool_calls,
                "observes_tools": self.adapter.observes_tools(),
                "usage": {
                    "input_tokens": captured.usage.input_tokens,
                    "output_tokens": captured.usage.output_tokens,
                },
                "truths": truths,
                "void": void,
                "raw": captured.raw,
            }),
        )?;
        let receipt = self.record.write_receipt(
            self.run_id,
            &receipt_name,
            &record::slim_receipt(question, &scores, None),
        )?;
        let receipt_rel = receipt
            .strip_prefix(self.record.dir())
            .unwrap_or(receipt.as_path())
            .to_path_buf();
        for (item, answer, truth, source, outcome) in scored {
            self.record.append(
                Stream::Items,
                self.item_row(turn, &item.id, outcome, captured.served_model.as_deref())
                    .merged(json!({
                        "answer": answer.as_deref().map(|a| crate::utils::safe_truncate(a, ANSWER_MAX_BYTES)),
                        "truth": truth,
                        "truth_source": source,
                        "tool_calls": captured.tool_calls.len(),
                        "observes_tools": self.adapter.observes_tools(),
                        "void_reason": void,
                        "family": item.family,
                        "rule": item.rule,
                        "receipt": receipt_rel,
                    })),
            )?;
            tally.items += 1;
        }
        Ok(())
    }

    fn record_error(
        &self,
        turn: &Turn<'_>,
        error: &str,
        tally: &mut Tally,
    ) -> Result<(), RunError> {
        for item in turn.items {
            self.record.append(
                Stream::Items,
                self.item_row(turn, &item.id, Outcome::Error, None)
                    .merged(json!({
                        "error": crate::utils::safe_truncate(error, ANSWER_MAX_BYTES),
                    })),
            )?;
            tally.items += 1;
        }
        Ok(())
    }

    /// The fields every item row carries: what was asked, by which arm,
    /// under which lineage.
    fn item_row(
        &self,
        turn: &Turn<'_>,
        item: &str,
        outcome: Outcome,
        served_model: Option<&str>,
    ) -> Row {
        Row(merged(
            json!({
                "v": RECORD_VERSION,
                "run": self.run_id,
                "at": self.now().to_rfc3339(),
                "set": turn.set.set.set,
                "set_version": turn.set.set.version,
                "set_sha256": turn.set.sha256,
                "file_sha256": turn.set.file_sha256,
                "measure": turn.set.set.measure,
                "item": item,
                "arm": turn.arm,
                "outcome": outcome,
                "served_model": served_model,
                "prompt_sha256": turn.prompt_sha256,
            }),
            self.lineage.fields(),
        ))
    }
}

/// One asked turn, before its result is accounted for.
struct Asked {
    question: String,
    prompt_sha256: String,
    /// What the system prompt was made of (receipts).
    components: serde_json::Value,
    result: Result<CapturedRun, Box<dyn std::error::Error + Send + Sync>>,
}

impl Asked {
    /// A turn that never started: the run stopped before it.
    fn interrupted(reason: String) -> Self {
        Self {
            question: String::new(),
            prompt_sha256: String::new(),
            components: serde_json::Value::Null,
            result: Err(Box::new(Interrupted {
                reason,
                mid_turn: false,
            })),
        }
    }
}

/// The scratch item an arm's turn runs in: the pulse's copy, or a bare one.
fn item_kind(arm: Arm) -> ItemKind {
    if arm == Arm::Pulse {
        ItemKind::Pulse
    } else {
        ItemKind::Bare
    }
}

/// What a turn's result came to.
enum Settled {
    Answered(CapturedRun),
    /// A later turn failed: its items are recorded as errors.
    Failed(String),
    /// The run stops here.
    Stopped,
}

/// The questions a set's turns ask, with the items each one asks: one
/// batched question, or one per item.
fn questions<'s>(
    set: &'s LoadedSet,
    run_id: &str,
    truths: &Truths,
) -> Vec<(Vec<&'s Item>, String)> {
    let drawn = draw(set, run_id, truths);
    match set.set.delivery {
        Delivery::Batched => vec![(drawn, set.batched_prompt())],
        Delivery::PerItem => drawn
            .into_iter()
            .map(|item| (vec![item], set.item_prompt(item)))
            .collect(),
    }
}

/// One turn's fixed facts.
struct Turn<'a> {
    set: &'a LoadedSet,
    arm: Arm,
    /// The items this turn asks.
    items: &'a [&'a Item],
    prompt_sha256: &'a str,
    components: &'a serde_json::Value,
}

/// A row under construction.
struct Row(serde_json::Value);

impl Row {
    /// The row with `extra`'s fields added.
    fn merged(self, extra: serde_json::Value) -> serde_json::Value {
        merged(self.0, extra)
    }
}

mod paired_turns;

#[cfg(test)]
pub(crate) mod tests;
