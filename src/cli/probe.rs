//! `pulse-null probe` — the growth-record probe runner (PN-144).
//!
//! Run as the pulse's own user from its directory (the systemd unit's
//! WorkingDirectory). Probes never touch the live pulse: every item runs in
//! a scratch copy under a Landlock sandbox, and a kernel without Landlock
//! refuses the run.

use std::error::Error;
use std::path::{Path, PathBuf};

use console::style;
use pulse_system_types::llm::{Message, MessageContent, Role};

use crate::cli_provider::adapter::CliAdapter;
use crate::cli_provider::adapters;
use crate::cli_provider::sandbox::LandlockSupport;
use crate::cli_provider::{CapturedRun, CliProvider};
use crate::config::Config;
use crate::vigil::probe::fence::{self, DenyList};
use crate::vigil::probe::graph::{self, GraphSource, LiveGraph, LiveSource, ScratchGraph};
use crate::vigil::probe::paired::{self, FactError, LoadedPairs};
use crate::vigil::probe::record::Record;
use crate::vigil::probe::run::{self as probe_run, RunEnv, RunOutcome, RunRequest, RunSummary};
use crate::vigil::probe::schedule::RunStatus;
use crate::vigil::probe::scratch::{
    scratch_parent, sweep_under_host_lock, GraphTarget, ItemKind, ItemPlan, ItemScratch,
    ItemTeardown, Scratch, ScratchSpec, SCRATCH_STALE_AFTER,
};
use crate::vigil::probe::sealed::{self, InstalledSets, Registry, SealError};
use crate::vigil::probe::summary::{
    minimum_n, summarize, ArmResult, ChainState, CostPerRun, GroupKey, RunLine, WeekSpend,
    NOT_GROUPED,
};
use crate::vigil::probe::verify::{self, Check, LiveTargets};

type CliResult = Result<(), Box<dyn Error>>;

/// How long a run waits for another pulse's run on this host (both timers
/// fire in the same quiet window).
const HOST_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(90 * 60);

/// How long a run's turns may take once it holds its locks. With the host
/// lock wait before it, it fits the unit's `TimeoutStartSec=4h` with a
/// margin for setup and teardown.
const RUN_TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(2 * 3600);

/// A flag set by SIGTERM (systemd stopping or timing out the unit) or
/// SIGINT (by hand): the run stops at once, tears down and writes its
/// finish row instead of dying mid-turn.
fn stop_on_signal() -> Result<std::sync::Arc<std::sync::atomic::AtomicBool>, Box<dyn Error>> {
    use tokio::signal::unix::SignalKind;
    stop_on([SignalKind::terminate(), SignalKind::interrupt()])
}

/// A flag set once the process gets either of `kinds`.
fn stop_on(
    [first, second]: [tokio::signal::unix::SignalKind; 2],
) -> Result<std::sync::Arc<std::sync::atomic::AtomicBool>, Box<dyn Error>> {
    use tokio::signal::unix::signal;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut term = signal(first)?;
    let mut int = signal(second)?;
    let flag = stop.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    Ok(stop)
}

/// The shared data dir holding the graph server's root password, when
/// `[graph] data_dir` does not name one.
const DEFAULT_DATA_DIR: &str = "/opt/pulse-null";

/// The pulse a probe command runs for, resolved once.
struct ProbeContext {
    config: Config,
    live_root: PathBuf,
    home: PathBuf,
    adapter_name: String,
    /// The pulse's graph, when recall-echo keeps it on the server.
    live_graph: Option<LiveGraph>,
}

impl ProbeContext {
    /// The pulse whose `pulse-null.toml` is found from the current
    /// directory, for the user whose home is `$HOME`.
    fn load() -> Result<Self, Box<dyn Error>> {
        Self::at(&Config::find_config()?, std::env::var_os("HOME"))
    }

    /// The pulse whose `pulse-null.toml` is at `config_path`, for the user
    /// whose home is `home`.
    fn at(config_path: &Path, home: Option<std::ffi::OsString>) -> Result<Self, Box<dyn Error>> {
        let root = config_path
            .parent()
            .ok_or_else(|| crate::errors::ConfigError::Validation("Invalid config path".into()))?;
        let config = Config::load_from(root)?;
        let live_root = root.canonicalize()?;
        let home = home.map(PathBuf::from).ok_or("HOME is not set")?;
        let adapter_name = config
            .llm
            .cli_adapter()
            .ok_or("probes need an agent CLI pulse ([llm] provider = \"cli\" with an adapter)")?
            .to_string();
        let recall_config =
            crate::fs_safe::read_bounded(&live_root.join("memory/.recall-echo.toml"), 1024 * 1024)?
                .unwrap_or_default();
        let live_graph = LiveGraph::from_recall_echo_config(&recall_config, &config.pulse.name)?;
        Ok(Self {
            config,
            live_root,
            home,
            adapter_name,
            live_graph,
        })
    }

    /// TCP ports a probe child may connect to besides the run's throwaway
    /// graph server: HTTPS for the vendor's API.
    fn probe_ports(&self) -> Vec<u16> {
        vec![443]
    }

    /// How a run copies this pulse's graph, when it is on a server: from
    /// the export the export unit leaves.
    fn graph_source(&self) -> Result<Option<GraphSource>, Box<dyn Error>> {
        if self.live_graph.is_none() {
            return Ok(None);
        }
        let surreal_bin =
            crate::surrealdb_manager::find_binary(&self.data_dir()).map_err(|e| e.to_string())?;
        Ok(Some(GraphSource {
            export: self.export_path(),
            surreal_bin,
        }))
    }

    /// Where this pulse's graph export is left for the next run.
    fn export_path(&self) -> PathBuf {
        graph::export_path(&scratch_parent(&self.home), &self.config.pulse.name)
    }

    /// Export the live graph for the next run (the export unit's step, the
    /// only one that reaches the live server). `None` when the pulse's
    /// graph is not on a server.
    async fn export_graph(&self) -> Result<Option<graph::ExportStats>, Box<dyn Error>> {
        let Some(live) = &self.live_graph else {
            return Ok(None);
        };
        let source = LiveSource::from_data_dir(live, &self.data_dir());
        Ok(Some(
            graph::export_live(&source, live, &self.export_path()).await?,
        ))
    }

    fn timezone(&self) -> Result<chrono_tz::Tz, Box<dyn Error>> {
        self.config.scheduler.timezone.parse().map_err(|_| {
            format!(
                "[scheduler] timezone '{}' is not a timezone",
                self.config.scheduler.timezone
            )
            .into()
        })
    }

    fn data_dir(&self) -> PathBuf {
        self.config
            .graph
            .data_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR))
    }

    /// Sweep what crashed runs left, then create this run's scratch.
    fn create_scratch(&self, keep: bool) -> Result<Scratch, Box<dyn Error>> {
        let parent = scratch_parent(&self.home);
        for stale in sweep_under_host_lock(&parent, SCRATCH_STALE_AFTER) {
            tracing::info!("removed stale probe scratch {}", stale.display());
        }
        Ok(Scratch::create(&ScratchSpec {
            live_root: &self.live_root,
            live_home: &self.home,
            pulse: &self.config.pulse.name,
            parent: &parent,
            keep,
        })?)
    }

    /// The run's graph: a copy of the live one on a throwaway server when it
    /// is on a server, else an empty embedded graph in each item.
    async fn prepare_graph(
        &self,
        scratch: &Scratch,
    ) -> Result<(GraphTarget, Option<ScratchGraph>), Box<dyn Error>> {
        let Some(source) = self.graph_source()? else {
            return Ok((GraphTarget::Embedded, None));
        };
        let copy = source.copy_for(&self.config.pulse.name, scratch).await?;
        Ok((copy.target(), Some(copy)))
    }

    /// The live paths a probe child must not read.
    fn deny(&self, adapter: &dyn CliAdapter) -> DenyList {
        DenyList::live(
            &self.live_root,
            &self.home,
            Some(&self.data_dir().join("surrealdb")),
            adapter.vendor_home(&self.home).as_deref(),
        )
    }

    /// What a probe child of this pulse may read besides its own item.
    fn read_roots(&self, adapter: &dyn CliAdapter) -> Result<Vec<PathBuf>, Box<dyn Error>> {
        let bin = self
            .config
            .llm
            .cli_bin
            .clone()
            .unwrap_or_else(|| adapter.default_bin().to_string());
        Ok(fence::read_roots(
            adapter,
            &bin,
            &self.home,
            &self.deny(adapter),
        )?)
    }

    fn adapter(&self) -> Result<Box<dyn CliAdapter>, Box<dyn Error>> {
        adapters::by_name(&self.adapter_name)
            .ok_or_else(|| format!("unknown cli adapter '{}'", self.adapter_name).into())
    }
}

/// What a probe command prints, and the failure it exits with once printed.
/// Built whole before anything is printed, so the logic that decides it can
/// be tested without a terminal.
#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    text: String,
    failure: Option<&'static str>,
}

impl Report {
    fn line(&mut self, line: impl std::fmt::Display) {
        self.text.push_str(&line.to_string());
        self.text.push('\n');
    }

    #[must_use]
    fn failing(self, failure: &'static str) -> Self {
        Self {
            failure: Some(failure),
            ..self
        }
    }
}

/// Print a command's report; its failure, or the error that stopped the
/// command before it had one, is the command's result.
fn emit(report: Result<Report, Box<dyn Error>>) -> CliResult {
    let report = report?;
    print!("{}", report.text);
    report.failure.map_or(Ok(()), |failure| Err(failure.into()))
}

/// Whether `key` is set, and not empty, in this process's environment.
fn env_is_set(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

/// `pulse-null probe run [--if-due] [--only <set>…] [--keep-scratch]`.
///
/// A refusal under `--if-due` (not due, outside the window, over budget) is
/// the timer's normal answer and exits 0; by hand it exits 1. An aborted run
/// exits 1 so the unit shows it failed.
pub async fn run(if_due: bool, only: Vec<String>, keep_scratch: bool) -> CliResult {
    let ctx = ProbeContext::load()?;
    let request = RunRequest {
        if_due,
        only,
        keep_scratch,
    };
    emit(run_for(&ctx, &request, RunHost::detect).await)
}

/// What a run takes from the host rather than from the pulse: probed when
/// the command runs, fixed in tests.
struct RunHost {
    now: chrono::DateTime<chrono::Utc>,
    landlock: LandlockSupport,
    recall_bin: String,
    /// The recall-echo version and graph mode ([`memory_components`]).
    memory: String,
    /// The unit's credentials directory, holding the sealed sets.
    credentials: Option<PathBuf>,
    has_env: &'static dyn Fn(&str) -> bool,
    confinement: &'static dyn Fn() -> Result<(), String>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl RunHost {
    async fn detect(server_graph: bool) -> Result<Self, Box<dyn Error>> {
        let recall_bin = crate::init::agent_bootstrap::find_recall_echo_bin();
        let memory = memory_components(&recall_bin, server_graph).await;
        let credentials = std::env::var_os(sealed::CREDENTIALS_ENV)
            .filter(|d| !d.is_empty())
            .map(PathBuf::from);
        Ok(Self {
            stop: stop_on_signal()?,
            now: chrono::Utc::now(),
            landlock: LandlockSupport::detect(),
            recall_bin,
            memory,
            credentials,
            has_env: &env_is_set,
            confinement: &crate::vigil::probe::confine::require,
        })
    }
}

/// Run the probe sets for `ctx`'s pulse with what `detect` finds on the host.
async fn run_for(
    ctx: &ProbeContext,
    request: &RunRequest,
    detect: impl AsyncFnOnce(bool) -> Result<RunHost, Box<dyn Error>>,
) -> Result<Report, Box<dyn Error>> {
    let tz = ctx.timezone()?;
    let ports = ctx.probe_ports();
    let deny = ctx.deny(ctx.adapter()?.as_ref());
    let registry = Registry::v1()?;
    let host = detect(ctx.live_graph.is_some()).await?;
    let env = RunEnv {
        config: &ctx.config,
        live_root: &ctx.live_root,
        home: &ctx.home,
        scratch_parent: &scratch_parent(&ctx.home),
        tz,
        now: host.now,
        landlock: host.landlock,
        recall_bin: &host.recall_bin,
        tcp_connect: &ports,
        deny: &deny,
        live_graph: ctx.graph_source()?,
        has_env: host.has_env,
        registry: &registry,
        credentials: host.credentials.as_deref(),
        host_lock_wait: HOST_LOCK_WAIT,
        confinement: host.confinement,
        stop: &host.stop,
        time_budget: RUN_TIME_BUDGET,
        memory: &host.memory,
    };
    let outcome = probe_run::run(&env, request).await?;
    Ok(run_report(&outcome, request.if_due))
}

fn run_report(outcome: &RunOutcome, if_due: bool) -> Report {
    match outcome {
        RunOutcome::Refused(reason) => refused_report(reason, if_due),
        RunOutcome::Finished(summary) => finished_report(summary),
    }
}

/// A refusal under `--if-due` is the timer's normal answer; by hand it fails.
fn refused_report(reason: &str, if_due: bool) -> Report {
    let mut report = Report::default();
    report.line(format!(
        "  {} probe run refused: {reason}",
        style("◉").yellow()
    ));
    if if_due {
        report
    } else {
        report.failing("probe run refused")
    }
}

fn finished_report(summary: &RunSummary) -> Report {
    let mut report = Report::default();
    report.line(format!(
        "  {} probe run {} {} — {} turns · {} in / {} out tokens · {} answers recorded",
        style("◉").yellow(),
        summary.run,
        run_status_word(summary.status),
        summary.turns,
        summary.input_tokens,
        summary.output_tokens,
        summary.items_recorded
    ));
    if let Some(reason) = &summary.reason {
        report.line(format!("    stopped: {reason}"));
    }
    for alert in &summary.alerts {
        report.line(format!("    {}  {alert}", style("ALERT").red().bold()));
    }
    if summary.status == RunStatus::Aborted {
        report.failing("probe run aborted")
    } else {
        report
    }
}

fn run_status_word(status: RunStatus) -> console::StyledObject<&'static str> {
    match status {
        RunStatus::Complete => style("complete").green(),
        RunStatus::Partial => style("partial").yellow(),
        RunStatus::Aborted => style("aborted").red().bold(),
    }
}

/// What a paired-fact answer depends on besides the model: the recall-echo
/// version and where the graph lives.
async fn memory_components(recall_bin: &str, server_graph: bool) -> String {
    let version = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new(recall_bin)
            .arg("--version")
            .env_clear()
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .filter(|v| !v.is_empty())
    .unwrap_or_else(|| "recall-echo unknown".to_string());
    let graph = if server_graph { "server" } else { "embedded" };
    format!("{version} · graph {graph}")
}

/// `pulse-null probe status`: spend against the budget, the measured cost
/// per run, chain state, and results by lineage — from the record only.
pub async fn status() -> CliResult {
    let ctx = ProbeContext::load()?;
    emit(status_report(&ctx, chrono::Utc::now()))
}

fn status_report(
    ctx: &ProbeContext,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Report, Box<dyn Error>> {
    let tz = ctx.timezone()?;
    let record = Record::existing(&ctx.live_root);
    let summary = summarize(record.as_ref(), &minimum_n(&Registry::v1()?), tz, now)?;
    let mut report = Report::default();
    report.line(status_header(
        &ctx.config,
        LandlockSupport::detect().describe(),
    ));
    report.line(format!(
        "    sets     sealed: delivered to the probe unit as credentials from {}/{}",
        sealed::SETS_ROOT,
        ctx.config.pulse.name.to_ascii_lowercase()
    ));
    report.line(budget_line(&summary.week, ctx.config.probe.weekly_turns));
    report.line(cost_line(summary.cost_per_run.as_ref()));
    for run in summary.runs.iter().rev().take(5) {
        report.line(run_line(run, tz));
    }
    report.line(format!("    chain    {}", chains_line(&summary.chains)));
    for (key, arms) in &summary.groups {
        let interrupted = summary.interrupted.contains(key);
        for line in group_lines(key, arms, interrupted, now) {
            report.line(line);
        }
    }
    Ok(report)
}

fn status_header(config: &Config, landlock: String) -> String {
    let probe = &config.probe;
    format!(
        "  {} probe record — {}   (probes {} · cadence {:?} · quiet {}–{} {} · {landlock})",
        style("◉").yellow(),
        config.pulse.name,
        if probe.enabled { "on" } else { "off" },
        probe.cadence,
        probe.quiet_start,
        probe.quiet_end,
        config.scheduler.timezone,
    )
}

fn budget_line(week: &WeekSpend, weekly_turns: u32) -> String {
    format!(
        "    budget   {} of {weekly_turns} turns this week · {} in / {} out tokens",
        week.turns, week.input_tokens, week.output_tokens
    )
}

fn cost_line(cost: Option<&CostPerRun>) -> String {
    match cost {
        Some(cost) => format!(
            "    cost     per complete run: {:.1} turns · {:.0} in / {:.0} out tokens (mean of {})",
            cost.turns, cost.input_tokens, cost.output_tokens, cost.runs
        ),
        None => "    cost     not measured yet (no complete run)".to_string(),
    }
}

fn run_line(run: &RunLine, tz: chrono_tz::Tz) -> String {
    let status = run
        .status
        .map_or("no finish row (crashed)".to_string(), |s| {
            format!("{s:?}").to_lowercase()
        });
    format!(
        "    run      {}  {}  {status} · {} turns · {} in / {} out",
        run.started.with_timezone(&tz).format("%Y-%m-%d %H:%M"),
        run.run,
        run.turns,
        run.input_tokens,
        run.output_tokens
    )
}

fn chains_line(chains: &[ChainState]) -> String {
    chains
        .iter()
        .map(chain_state)
        .collect::<Vec<_>>()
        .join(" · ")
}

fn chain_state(chain: &ChainState) -> String {
    let unreadable = if chain.unreadable > 0 {
        format!(", {} unreadable lines skipped", chain.unreadable)
    } else {
        String::new()
    };
    match chain.broken_at {
        None => format!("{} ok ({}{unreadable})", chain.stream, chain.rows),
        Some(line) => format!(
            "{} BROKEN at line {line} ({} verified, {} rejected{unreadable})",
            chain.stream, chain.rows, chain.rejected
        ),
    }
}

/// Whether a group's numbers can be read as a result yet.
#[derive(Debug, Clone, Copy)]
struct BlockState {
    /// A lineage changed inside the block.
    interrupted: bool,
    /// The block has closed.
    readable: bool,
}

/// One results group: its lineage line, then a line per arm.
fn group_lines(
    key: &GroupKey,
    arms: &[ArmResult],
    interrupted: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    let (lineage, readable) = lineage_line(key, now);
    let block = BlockState {
        interrupted,
        readable,
    };
    let mut lines = vec![format!("    {lineage}")];
    lines.extend(arms.iter().map(|arm| arm_line(&key.set, arm, block)));
    lines
}

/// The group's lineage, and whether its block can be read yet.
fn lineage_line(key: &GroupKey, now: chrono::DateTime<chrono::Utc>) -> (String, bool) {
    let mut lineage = vec![key.set.clone(), key.baseline.clone(), key.adapter.clone()];
    if key.configured_model != NOT_GROUPED {
        lineage.push(format!("configured {}", key.configured_model));
    }
    lineage.push(format!("served {}", key.served_model));
    lineage.push(format!("effort {}", key.reasoning_effort));
    if key.cli_version != NOT_GROUPED {
        lineage.push(format!("cli {}", key.cli_version));
    }
    for part in [&key.binary, &key.memory] {
        if part != NOT_GROUPED {
            lineage.push(part.clone());
        }
    }
    for (label, digest) in [
        ("disclosure", &key.disclosure_sha256),
        ("fresh", &key.fresh_sha256),
    ] {
        if digest != NOT_GROUPED {
            lineage.push(format!("{label} {}", short_digest(digest)));
        }
    }
    lineage.push(format!("scorer v{}", key.scorer_version));
    let (block, readable) = key.block.describe(now);
    lineage.push(block);
    (lineage.join(" · "), readable)
}

/// The first eight characters of a digest, enough to tell lineages apart.
fn short_digest(digest: &str) -> &str {
    digest.get(..8).unwrap_or(digest)
}

fn arm_line(set: &str, arm: &ArmResult, block: BlockState) -> String {
    format!(
        "      {:<11} {:<27} {}{}",
        arm.arm,
        arm_role(set, &arm.arm),
        arm_reading(arm, block),
        arm_notes(arm, block.readable)
    )
}

/// The arm's number, or why there is none yet.
fn arm_reading(arm: &ArmResult, block: BlockState) -> String {
    let p = arm.proportion;
    if block.interrupted {
        format!(
            "{} answers: block interrupted by a lineage change, not a result",
            p.n
        )
    } else if !block.readable {
        format!("{} answers so far (read when the block closes)", p.n)
    } else if arm.enough() && arm.arm.starts_with("twin") {
        format!("made up {} of {} it was never told", arm.wrong, p.n)
    } else if arm.enough() && arm.arm.starts_with("guess") {
        chance_rate(arm)
    } else if arm.enough() {
        format!(
            "{} of {} · {:.0}% ({:.0}–{:.0})",
            p.k,
            p.n,
            100.0 * f64::from(p.k) / f64::from(p.n.max(1)),
            100.0 * p.low,
            100.0 * p.high
        )
    } else {
        format!("not enough yet · {} of {}", p.n, arm.min_n.unwrap_or(0))
    }
}

/// A forced guess's number: its unhedged hits (the chance rate), or all its
/// hits when the rows do not say which were hedged.
fn chance_rate(arm: &ArmResult) -> String {
    let u = arm.unhedged.unwrap_or(arm.proportion);
    format!(
        "chance {} of {} unhedged · {:.0}% ({:.0}–{:.0})",
        u.k,
        u.n,
        100.0 * f64::from(u.k) / f64::from(u.n.max(1)),
        100.0 * u.low,
        100.0 * u.high
    )
}

/// What qualifies the arm's number: voids, ask timing, exploratory offsets,
/// unhedged hits and the constant-answer floor.
fn arm_notes(arm: &ArmResult, readable: bool) -> String {
    let mut notes = String::new();
    if arm.void > 0 {
        notes.push_str(&format!(" · {} void", arm.void));
    }
    if let Some(days) = arm.elapsed_days {
        notes.push_str(&asked_after(days));
    }
    if arm.exploratory {
        notes.push_str(" · exploratory (v1 decides on 1 and 7 days)");
    }
    // A forced guess's unhedged hits are its number already.
    let shown = arm.enough() && readable && !arm.arm.starts_with("guess");
    if let Some(u) = arm.unhedged.filter(|_| shown) {
        notes.push_str(&format!(" · unhedged {} of {}", u.k, u.n));
    }
    if let Some(c) = arm.constant_answer {
        notes.push_str(&format!(
            " · RESTARTED/SAME: a constant answer gets {} of {}",
            c.k, c.n
        ));
    }
    notes
}

fn asked_after((low, high): (i64, i64)) -> String {
    if low == high {
        format!(" · asked {low} d after the plant")
    } else {
        format!(" · asked {low}–{high} d after the plant")
    }
}

/// A paired fact's sides, by the label's start, and what each one's number
/// is. The fresh arm is asked twice: as the pulse is (UNKNOWN allowed, what
/// a model with no memory volunteers) and as a forced guess, whose unhedged
/// hits are the chance rate "remembered" is read against (R3-A).
const PAIRED_ARM_ROLES: [(&str, &str); 4] = [
    ("planted", "remembered what it was told"),
    ("twin", "never told (baseline)"),
    ("chance", "volunteered with no memory"),
    ("guess", "chance rate (forced guess)"),
];

/// What an arm's number is, on the page (G4, G10, §1a.2). Known-answer is
/// a regression check. Situation-use v1 is prompted retrieval: the request
/// names the fact it needs, so it shows whether the pulse gave the fact
/// when asked, never that it acted on its situation; its input-only arm is
/// handed the facts and sits at the ceiling, a check of the scorer and the
/// facts, not a baseline that can lose. Paired facts compare what it was
/// told with what it never was.
fn arm_role(set: &str, arm: &str) -> &'static str {
    if let Some((_, role)) = PAIRED_ARM_ROLES
        .iter()
        .find(|(side, _)| arm.starts_with(side))
    {
        return role;
    }
    match (set.starts_with("known-answer"), arm) {
        (true, "pulse") => "regression check",
        (false, "pulse") => "gave the fact when asked",
        (_, "input-only") => "ceiling check (facts given)",
        _ => "labelled reference",
    }
}

/// The pair set and fact key, from this process's systemd credentials
/// (refused without them: the sealed sets never sit where the pulse's user
/// can read them).
fn installed_pairs() -> Result<LoadedPairs, Box<dyn Error>> {
    pairs_from(sealed::load_credentials(&Registry::v1()?))
}

/// The pair set and fact key in the sealed sets, once they are loaded.
fn pairs_from(installed: Result<InstalledSets, SealError>) -> Result<LoadedPairs, Box<dyn Error>> {
    Ok(LoadedPairs::from_installed(&installed?)?)
}

/// Where the paired-fact commands get the pair set: [`installed_pairs`],
/// or a fixture in tests.
type PairsSource = fn() -> Result<LoadedPairs, Box<dyn Error>>;

/// `pulse-null probe facts draw`: the runtime picks the next fact to plant.
pub async fn facts_draw() -> CliResult {
    let ctx = ProbeContext::load()?;
    emit(facts_draw_for(&ctx, installed_pairs, chrono::Utc::now()))
}

fn facts_draw_for(
    ctx: &ProbeContext,
    pairs: PairsSource,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Report, Box<dyn Error>> {
    let pairs = pairs()?;
    let record = Record::open(&ctx.live_root)?;
    let drawn = paired::draw(&record, &pairs, now).map_err(fact_error)?;
    Ok(drawn_report(&drawn, &ctx.config.pulse.name))
}

fn drawn_report(drawn: &paired::Drawn, pulse: &str) -> Report {
    let mut report = Report::default();
    report.line(format!(
        "  {} fact {} drawn. In normal conversation with {pulse}, mention {} (any value you like),",
        style("◉").yellow(),
        style(&drawn.fact).bold(),
        style(&drawn.ask_owner).bold()
    ));
    report.line(format!(
        "    in one message with the words \"{}\" and the value (\"my {} is …\").",
        drawn.say, drawn.say
    ));
    report.line(format!(
        "    Then record it, before that conversation is archived: \
         probe facts plant {} \"<the value you said>\" (through systemd-run with the \
         unit's credentials, as for this draw)",
        drawn.fact
    ));
    report.line(format!(
        "    Until its last ask (90 days), don't mention {} to {pulse}: that side is the baseline.",
        style(&drawn.avoid).bold(),
    ));
    report
}

/// A mention to record: the fact `draw` printed and the value said.
struct Planting {
    fact: String,
    value: String,
    now: chrono::DateTime<chrono::Utc>,
}

/// `pulse-null probe facts plant <fact> <value>`: confirm the mention.
pub async fn facts_plant(fact: String, value: String) -> CliResult {
    let ctx = ProbeContext::load()?;
    let planting = Planting {
        fact,
        value,
        now: chrono::Utc::now(),
    };
    emit(facts_plant_for(&ctx, installed_pairs, &planting))
}

fn facts_plant_for(
    ctx: &ProbeContext,
    pairs: PairsSource,
    planting: &Planting,
) -> Result<Report, Box<dyn Error>> {
    let pairs = pairs()?;
    let record = Record::open(&ctx.live_root)?;
    let (mention, twin_mentioned) = paired::plant(
        &record,
        &ctx.live_root,
        &pairs,
        &planting.fact,
        &planting.value,
        planting.now,
    )
    .map_err(fact_error)?;
    Ok(planted_report(
        &planting.fact,
        &mention,
        &pairs.set.offsets_days,
        twin_mentioned,
    ))
}

fn planted_report(
    fact: &str,
    mention: &paired::Mention,
    offsets_days: &[i64],
    twin_mentioned: bool,
) -> Report {
    let mut report = Report::default();
    report.line(format!(
        "  {} {fact} planted — found in {} at {}. It is asked after {offsets_days:?} days.",
        style("◉").yellow(),
        mention.source,
        mention.at.format("%Y-%m-%d %H:%M UTC"),
    ));
    if twin_mentioned {
        report.line(format!(
            "    {}  the other side of the pair came up in conversation too: its answers will be void",
            style("note").yellow().bold()
        ));
    }
    report
}

/// `pulse-null probe facts list`: the facts in the record and their asks.
pub async fn facts_list() -> CliResult {
    let ctx = ProbeContext::load()?;
    emit(facts_list_for(&ctx, installed_pairs, chrono::Utc::now()))
}

fn facts_list_for(
    ctx: &ProbeContext,
    pairs: PairsSource,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Report, Box<dyn Error>> {
    let Some(record) = Record::existing(&ctx.live_root) else {
        return Ok(facts_report(&[], &[], now));
    };
    let pairs = pairs()?;
    let rows = record
        .read(crate::vigil::probe::record::Stream::Facts)?
        .rows;
    let facts = paired::facts_from_rows(&rows, &pairs);
    Ok(facts_report(&facts, &pairs.set.offsets_days, now))
}

fn facts_report(
    facts: &[paired::Fact],
    offsets_days: &[i64],
    now: chrono::DateTime<chrono::Utc>,
) -> Report {
    let mut report = Report::default();
    if facts.is_empty() {
        report.line("  no facts drawn yet");
    }
    for fact in facts {
        report.line(fact_line(fact, offsets_days, now));
    }
    report
}

fn fact_line(
    fact: &paired::Fact,
    offsets_days: &[i64],
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let state = match (&fact.value, fact.planted_at) {
        (Some(_), Some(at)) => format!("planted {}", at.format("%Y-%m-%d")),
        _ => "drawn, not planted yet".to_string(),
    };
    let done = if fact.active(offsets_days, now) {
        ""
    } else {
        " · finished"
    };
    format!(
        "  {}  {:<16} side {:?} · {state} · asked at {:?} days{done}",
        fact.fact, fact.pair, fact.planted, fact.asked
    )
}

fn fact_error(error: FactError) -> Box<dyn Error> {
    error.to_string().into()
}

/// What `probe sets install` was asked to install, and where.
struct SetsInstall {
    /// The reviewed copy.
    from: PathBuf,
    pulse: String,
    /// The install root; [`sealed::SETS_ROOT`] when not given.
    dest: Option<PathBuf>,
}

/// `pulse-null probe sets install --from <dir> --pulse <pulse> [--dest <root>]`,
/// as root: install the reviewed, sealed item sets into
/// `<root>/<pulse>/` (default `/etc/pulse-null-probe/sets`), root 0700,
/// files 0600, from where the probe unit loads them as credentials. Every
/// file must match the digest this binary was built with, or nothing is
/// installed. The fact key is made on the first install and kept after.
pub async fn sets_install(from: PathBuf, pulse: String, dest: Option<PathBuf>) -> CliResult {
    // SAFETY: geteuid has no preconditions and touches no memory.
    let euid = unsafe { libc::geteuid() };
    let request = SetsInstall { from, pulse, dest };
    emit(install_sets(&request, euid, Registry::v1))
}

/// Install `request`'s sets as the user `euid`, checked against the
/// digests `registry` pins.
fn install_sets(
    request: &SetsInstall,
    euid: u32,
    registry: impl FnOnce() -> Result<Registry, SealError>,
) -> Result<Report, Box<dyn Error>> {
    if euid != 0 {
        return Err(format!(
            "probe sets install runs as root: it writes {}/<pulse>/, which the pulse's user \
             must not be able to read",
            sealed::SETS_ROOT
        )
        .into());
    }
    let registry = registry()?;
    let root = request
        .dest
        .clone()
        .unwrap_or_else(|| PathBuf::from(sealed::SETS_ROOT));
    let dir = sealed::pulse_dir(&root, &request.pulse)?;
    let written = sealed::install(&request.from, &dir, &registry)?;
    Ok(installed_report(&dir, &written, &request.from))
}

fn installed_report(dir: &Path, written: &[String], from: &Path) -> Report {
    let mut report = Report::default();
    report.line(format!(
        "  {} installed into {} (root 0700, files 0600):",
        style("◉").yellow(),
        dir.display()
    ));
    for file in written {
        report.line(format!("    {file}"));
    }
    report.line(format!(
        "    You can delete the copy in {} now.",
        from.display()
    ));
    report
}

/// `pulse-null probe export-graph`: export the live graph for the next
/// probe run. The export unit runs it, just before the probe unit; it is the
/// only probe step that reaches the live graph server, and it only reads.
pub async fn export_graph() -> CliResult {
    let ctx = ProbeContext::load()?;
    emit(export_for(&ctx).await)
}

async fn export_for(ctx: &ProbeContext) -> Result<Report, Box<dyn Error>> {
    let stats = ctx.export_graph().await?;
    if let Some(stats) = &stats {
        tracing::info!(
            bytes = stats.bytes,
            elapsed_ms = u64::try_from(stats.elapsed.as_millis()).unwrap_or(u64::MAX),
            "probe graph export written"
        );
    }
    Ok(export_report(
        &ctx.config.pulse.name,
        &ctx.export_path(),
        stats.as_ref(),
    ))
}

fn export_report(pulse: &str, export: &Path, stats: Option<&graph::ExportStats>) -> Report {
    let mut report = Report::default();
    match stats {
        Some(stats) => report.line(format!(
            "  {} exported {pulse}'s graph to {} ({} bytes in {:.1}s)",
            style("◉").yellow(),
            export.display(),
            stats.bytes,
            stats.elapsed.as_secs_f64()
        )),
        None => report.line(format!(
            "  {pulse}'s graph is not on a server: nothing to export"
        )),
    }
    report
}

/// `pulse-null probe verify-isolation [--turn] [--keep-scratch]`.
pub async fn verify_isolation(turn: bool, keep_scratch: bool) -> CliResult {
    let ctx = ProbeContext::load()?;
    let request = VerifyRequest { turn, keep_scratch };
    emit(verify_for(&ctx, &request, VerifyHost::detect).await)
}

/// What `verify-isolation` was asked for.
struct VerifyRequest {
    /// Also run a canned turn and a fresh-arm date check (two real turns).
    turn: bool,
    keep_scratch: bool,
}

/// What the isolation check takes from the host rather than from the
/// pulse: probed when the command runs, fixed in tests.
struct VerifyHost {
    /// The probe unit's confinement, checked from inside this process.
    confinement: Vec<Check>,
    has_env: &'static dyn Fn(&str) -> bool,
    /// Ports the sandbox allows that the probe unit must still refuse on
    /// loopback (the host's nginx).
    unit_ports: &'static [u16],
}

impl VerifyHost {
    fn detect() -> Self {
        Self {
            confinement: crate::vigil::probe::confine::checks(),
            has_env: &env_is_set,
            unit_ports: &[443],
        }
    }
}

/// Show that a probe item of `ctx`'s pulse writes nowhere live.
async fn verify_for(
    ctx: &ProbeContext,
    request: &VerifyRequest,
    detect: impl FnOnce() -> VerifyHost,
) -> Result<Report, Box<dyn Error>> {
    LandlockSupport::detect().require()?;
    let adapter = ctx.adapter()?;
    let recall_bin = crate::init::agent_bootstrap::find_recall_echo_bin();
    let host = detect();
    export_unless_confined(ctx, &host.confinement).await?;
    let scratch = ctx.create_scratch(request.keep_scratch)?;
    let (graph, graph_copy) = ctx.prepare_graph(&scratch).await?;
    let site = IsolationSite {
        ctx,
        adapter: adapter.as_ref(),
        recall_bin: &recall_bin,
        scratch: &scratch,
        graph: &graph,
        graph_port: graph_copy.as_ref().map(ScratchGraph::port),
    };
    let (mut checks, teardown) = site.checks(host, request).await?;
    checks.extend(stop_graph_copy(graph_copy).await);
    let scratch_dir = scratch.dir().to_path_buf();
    scratch.teardown()?;
    let result = IsolationResult {
        checks,
        changed_vendor_files: teardown.changed_vendor_files,
        kept_scratch: request.keep_scratch.then_some(scratch_dir),
    };
    Ok(result.report(&ctx.config.pulse.name, LandlockSupport::detect().describe()))
}

/// Under the unit the export unit has left a fresh export. By hand (not
/// confined) the loopback is open, so the check exports for itself.
async fn export_unless_confined(
    ctx: &ProbeContext,
    confinement: &[Check],
) -> Result<(), Box<dyn Error>> {
    if confinement.iter().any(|c| !c.passed) && ctx.export_graph().await?.is_some() {
        println!("  (not under the probe unit: exported the live graph for this check)");
    }
    Ok(())
}

/// The scratch copy an isolation check runs its items in.
struct IsolationSite<'a> {
    ctx: &'a ProbeContext,
    adapter: &'a dyn CliAdapter,
    recall_bin: &'a str,
    scratch: &'a Scratch,
    graph: &'a GraphTarget,
    /// The throwaway graph server's port, when the run copied the graph.
    graph_port: Option<u16>,
}

impl IsolationSite<'_> {
    /// Every check in item order (sandbox, confinement, the CLI starting,
    /// then the turns when asked), and what the pulse item's teardown found.
    async fn checks(
        &self,
        host: VerifyHost,
        request: &VerifyRequest,
    ) -> Result<(Vec<Check>, ItemTeardown), Box<dyn Error>> {
        let read_roots = self.ctx.read_roots(self.adapter)?;
        let mut ports = self.ctx.probe_ports();
        ports.extend(self.graph_port);
        let item = self.scratch.item(
            0,
            ItemKind::Pulse,
            &self.plan(self.graph, &ports, &read_roots),
        )?;
        let vendor_home = self.adapter.vendor_home(&self.ctx.home);
        let mut checks = self
            .sandbox_checks(&item, vendor_home.as_deref(), host.unit_ports)
            .await;
        checks.extend(host.confinement);
        checks.push(cli_starts(self.ctx, &item).await?);
        if request.turn {
            let turns = self
                .turn_checks(&item, vendor_home.as_deref(), &read_roots, host.has_env)
                .await?;
            checks.extend(turns);
        }
        Ok((checks, item.finish()))
    }

    fn plan<'p>(
        &'p self,
        graph: &'p GraphTarget,
        tcp_connect: &'p [u16],
        read_roots: &'p [PathBuf],
    ) -> ItemPlan<'p> {
        ItemPlan {
            adapter: self.adapter,
            recall_bin: self.recall_bin,
            graph,
            tcp_connect,
            read_roots,
        }
    }

    async fn sandbox_checks(
        &self,
        item: &ItemScratch,
        vendor_home: Option<&Path>,
        unit_ports: &[u16],
    ) -> Vec<Check> {
        let deny = self.ctx.deny(self.adapter);
        let marker = format!(".pulse-null-probe-check-{}", self.scratch.id());
        let targets = LiveTargets {
            live_root: &self.ctx.live_root,
            live_home: &self.ctx.home,
            vendor_home,
            pulse_port: self.ctx.config.server.port,
            graph_port: self.ctx.live_graph.as_ref().and_then(|g| g.port()),
            deny: &deny,
            unit_ports,
        };
        verify::sandbox_self_test(item, &targets, &marker).await
    }

    /// The canned turn in the pulse item, then the fresh-arm date check in a
    /// bare one: two real turns.
    async fn turn_checks(
        &self,
        item: &ItemScratch,
        vendor_home: Option<&Path>,
        read_roots: &[PathBuf],
        has_env: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<Check>, Box<dyn Error>> {
        let mut checks = self.canned_turn(item, vendor_home, has_env).await?;
        let ports = self.ctx.probe_ports();
        let bare = self.scratch.item(
            1,
            ItemKind::Bare,
            &self.plan(&GraphTarget::Embedded, &ports, read_roots),
        )?;
        checks.push(fresh_arm_date_check(self.ctx, &bare).await?);
        bare.finish();
        Ok(checks)
    }

    /// One real CLI turn in the item copy: it must complete, and its
    /// transcript must land in the scratch vendor dir and nowhere live.
    async fn canned_turn(
        &self,
        item: &ItemScratch,
        vendor_home: Option<&Path>,
        has_env: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<Check>, Box<dyn Error>> {
        let adapter = self.ctx.adapter()?;
        let observes_tools = adapter.observes_tools();
        adapter.probe_auth_ready(has_env)?;
        let system = crate::server::prompt::build_system_prompt(item.root(), &self.ctx.config)?;
        let provider = turn_provider(self.ctx, adapter, item);
        let vendor_files_before = verify::file_count(item.vendor());
        let question = [user_message(ISOLATION_QUESTION.into())];
        let run = crate::vigil::probe::probe_turn(&provider, &system, &question).await;
        let mut checks = vec![
            turn_check(&run, observes_tools),
            vendor_files_check(vendor_files_before, verify::file_count(item.vendor())),
        ];
        checks.extend(vendor_home.and_then(|vendor| leak_check(vendor, self.scratch.id())));
        Ok(checks)
    }
}

/// Stop the run's throwaway graph server, if it had one, and check that it
/// is gone.
async fn stop_graph_copy(copy: Option<ScratchGraph>) -> Option<Check> {
    let copy = copy?;
    let port = copy.port();
    copy.remove().await;
    let gone = tokio::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port))
        .await
        .is_err();
    Some(graph_stopped_check(port, gone))
}

fn graph_stopped_check(port: u16, gone: bool) -> Check {
    Check {
        name: "graph: throwaway server stopped".into(),
        passed: gone,
        detail: if gone {
            format!("the copy on [::1]:{port} is gone; the live server was only read")
        } else {
            format!("[::1]:{port} still answers")
        },
    }
}

/// What an isolation check found.
struct IsolationResult {
    checks: Vec<Check>,
    /// Copied vendor files the CLI changed during the pulse item.
    changed_vendor_files: Vec<PathBuf>,
    /// Where the scratch was left, when it was kept.
    kept_scratch: Option<PathBuf>,
}

impl IsolationResult {
    fn report(&self, pulse: &str, landlock: String) -> Report {
        let mut report = Report::default();
        report.line(format!(
            "  {} probe isolation — {pulse} ({landlock})",
            style("◉").yellow()
        ));
        for check in &self.checks {
            report.line(format!(
                "    {}  {:<40} {}",
                pass_mark(check.passed),
                check.name,
                check.detail
            ));
        }
        for changed in &self.changed_vendor_files {
            report.line(format!(
                "    {}  {} changed during the probe; it was not copied back",
                style("ALERT").red().bold(),
                changed.display()
            ));
        }
        if let Some(dir) = &self.kept_scratch {
            report.line(format!("  scratch kept at {}", dir.display()));
        }
        if self.checks.iter().all(|c| c.passed) {
            report
        } else {
            report.failing("probe isolation check failed")
        }
    }
}

fn pass_mark(passed: bool) -> console::StyledObject<&'static str> {
    if passed {
        style("pass").green()
    } else {
        style("FAIL").red().bold()
    }
}

const CLI_STARTS: &str = "cli: starts under the read fence";
const TURN_COMPLETED: &str = "turn: completed in the copy";
const FRESH_DATE_CHECK: &str = "fresh arm: the CLI hands it no date";

/// The message the canned turn sends.
const ISOLATION_QUESTION: &str =
    "This is an isolation check run by the runtime. Reply with exactly: probe ok";

/// The pulse's CLI starts inside the item's sandbox (its `--version`, no
/// model call): the read fence leaves it everything it needs to start.
async fn cli_starts(ctx: &ProbeContext, item: &ItemScratch) -> Result<Check, Box<dyn Error>> {
    let provider = CliProvider::new(
        ctx.adapter()?,
        ctx.config.llm.cli_bin.clone(),
        ctx.config.llm.model.clone(),
        item.root().to_path_buf(),
    )
    .with_overrides(item.overrides());
    Ok(version_check(provider.cli_version().await))
}

fn version_check(started: Result<String, impl std::fmt::Display>) -> Check {
    match started {
        Ok(version) => Check {
            name: CLI_STARTS.into(),
            passed: true,
            detail: format!("--version: {version}"),
        },
        Err(e) => failed_check(CLI_STARTS, &e),
    }
}

/// A check that failed with `error` (bounded, for the page).
fn failed_check(name: &str, error: &impl std::fmt::Display) -> Check {
    Check {
        name: name.into(),
        passed: false,
        detail: crate::utils::safe_truncate(&error.to_string(), 300).to_string(),
    }
}

/// The pulse's CLI in `item`, set up for a real turn.
fn turn_provider(
    ctx: &ProbeContext,
    adapter: Box<dyn CliAdapter>,
    item: &ItemScratch,
) -> CliProvider {
    CliProvider::new(
        adapter,
        ctx.config.llm.cli_bin.clone(),
        ctx.config.llm.model.clone(),
        item.root().to_path_buf(),
    )
    .with_reasoning_effort(ctx.config.llm.reasoning_effort.clone())
    .with_overrides(item.overrides())
}

fn user_message(text: String) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Text(text),
        source: None,
    }
}

/// The question the fresh-arm date check asks.
const DATE_QUESTION: &str = "What is today's date? Reply with just the date as YYYY-MM-DD, \
     or UNKNOWN if nothing you were given tells you.";

/// One fresh-arm turn (a bare item, the neutral one-line system prompt):
/// does the CLI hand the "no scaffold" arm today's date by itself, through a
/// default prompt or environment the neutral line was meant to replace? A
/// date it got from a tool call it made is not counted against it (audit
/// R2-13). Spends one turn.
async fn fresh_arm_date_check(
    ctx: &ProbeContext,
    bare: &ItemScratch,
) -> Result<Check, Box<dyn Error>> {
    let adapter = ctx.adapter()?;
    let observes_tools = adapter.observes_tools();
    let provider = turn_provider(ctx, adapter, bare);
    let question = user_message(crate::server::prompt::compose_user_message(
        false,
        None,
        DATE_QUESTION,
    ));
    let fresh = crate::vigil::probe::sets::FRESH_SYSTEM_V1;
    match crate::vigil::probe::probe_turn(&provider, fresh, &[question]).await {
        Ok(run) => {
            let today = chrono::Utc::now()
                .with_timezone(&ctx.timezone()?)
                .format("%Y-%m-%d")
                .to_string();
            Ok(date_check(&run, &today, observes_tools))
        }
        Err(e) => Ok(failed_check(FRESH_DATE_CHECK, &e)),
    }
}

/// Whether the fresh arm's reply gave `today`, and where it could have got
/// it from.
fn date_check(run: &CapturedRun, today: &str, observes_tools: bool) -> Check {
    let truth = crate::vigil::probe::truth::Truth {
        value: today.to_string(),
        source: "clock",
    };
    let knew = crate::vigil::probe::score::score(
        crate::vigil::probe::sets::Rule::Date,
        Some(&truth),
        Some(&run.reply),
    ) == crate::vigil::probe::score::Outcome::Correct;
    let looked_up = !run.tool_calls.is_empty();
    Check {
        name: FRESH_DATE_CHECK.into(),
        passed: !knew || looked_up,
        detail: match (knew, looked_up, observes_tools) {
            (false, _, _) => format!(
                "reply {:?}: no date of its own",
                crate::utils::safe_truncate(&run.reply, 60)
            ),
            (true, true, _) => format!("it gave {today}, from a tool call it made"),
            (true, false, true) => {
                format!("it gave {today} with no tool call: the CLI hands the fresh arm the date")
            }
            (true, false, false) => format!(
                "it gave {today}; this CLI does not show its tool calls, so the source is unknown"
            ),
        },
    }
}

fn turn_check(run: &Result<CapturedRun, impl std::fmt::Display>, observes_tools: bool) -> Check {
    match run {
        Ok(run) => Check {
            name: TURN_COMPLETED.into(),
            passed: true,
            detail: format!(
                "reply {:?} · served {} · {} · {} in / {} out tokens",
                crate::utils::safe_truncate(&run.reply, 60),
                run.served_model.as_deref().unwrap_or("unknown"),
                tool_calls_seen(run, observes_tools),
                run.usage.input_tokens.unwrap_or(0),
                run.usage.output_tokens.unwrap_or(0),
            ),
        },
        Err(e) => failed_check(TURN_COMPLETED, e),
    }
}

fn tool_calls_seen(run: &CapturedRun, observes_tools: bool) -> String {
    if observes_tools {
        format!("{} tool calls", run.tool_calls.len())
    } else {
        "tool calls not observable on this CLI".to_string()
    }
}

fn vendor_files_check(before: usize, after: usize) -> Check {
    Check {
        name: "turn: the CLI's own files landed in scratch".into(),
        passed: after > before,
        detail: format!("{before} → {after} files in the scratch vendor dir"),
    }
}

/// When the live vendor dir exists: nothing in it may name the scratch.
fn leak_check(vendor: &Path, scratch_id: &str) -> Option<Check> {
    if !vendor.is_dir() {
        return None;
    }
    let leaked = verify::entries_naming(vendor, scratch_id);
    Some(Check {
        name: "turn: nothing live names the scratch".into(),
        passed: leaked.is_empty(),
        detail: match leaked.first() {
            None => format!("no entry in {} names {scratch_id}", vendor.display()),
            Some(first) => format!("{} entries, e.g. {}", leaked.len(), first.display()),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_provider::adapter::{ToolCall, Usage};
    use crate::cli_provider::sandbox::landlock_or_skip;
    use crate::vigil::probe::run::tests::{Act, FakeCli};
    use crate::vigil::probe::score::Proportion;
    use crate::vigil::probe::scratch::tests::fixture_pulse;
    use crate::vigil::probe::summary::Block;
    use crate::vigil::probe::truth::tests::fixture_config;
    use chrono::{TimeZone as _, Utc};

    /// The text as a terminal without colour shows it.
    fn plain(text: &str) -> String {
        console::strip_ansi_codes(text).into_owned()
    }

    /// A probe context for a fixture pulse with its own home, its CLI at
    /// `cli_bin`. The pulse has no record and no isolation marker, as one
    /// that was never probed.
    struct Fixture {
        _live: tempfile::TempDir,
        _home: tempfile::TempDir,
        ctx: ProbeContext,
    }

    impl Fixture {
        fn new(cli_bin: Option<&Path>) -> Self {
            let live = fixture_pulse();
            std::fs::remove_file(live.path().join("ISOLATION")).unwrap();
            std::fs::remove_dir_all(live.path().join("record")).unwrap();
            let home = tempfile::tempdir().unwrap();
            let mut config = fixture_config();
            config.llm.cli_bin = cli_bin.map(|bin| bin.display().to_string());
            let ctx = ProbeContext {
                config,
                live_root: live.path().canonicalize().unwrap(),
                home: home.path().canonicalize().unwrap(),
                adapter_name: "claude".into(),
                live_graph: None,
            };
            Self {
                _live: live,
                _home: home,
                ctx,
            }
        }
    }

    fn always(_: &str) -> bool {
        true
    }

    fn never(_: &str) -> bool {
        false
    }

    fn confined() -> Result<(), String> {
        Ok(())
    }

    fn check(name: &str, passed: bool) -> Check {
        Check {
            name: name.into(),
            passed,
            detail: format!("{name} detail"),
        }
    }

    fn captured(reply: &str, tool_calls: usize) -> CapturedRun {
        CapturedRun {
            reply: reply.into(),
            raw: String::new(),
            served_model: Some("claude-opus-5-5".into()),
            tool_calls: (0..tool_calls)
                .map(|_| ToolCall {
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "date"}),
                    id: None,
                    result: None,
                })
                .collect(),
            usage: Usage {
                input_tokens: Some(1000),
                output_tokens: Some(40),
            },
        }
    }

    #[test]
    fn a_report_is_its_lines_and_its_failure() {
        let mut report = Report::default();
        report.line("first");
        report.line(format!("second {}", 2));
        assert_eq!(report.text, "first\nsecond 2\n");
        assert_eq!(report.failure, None);
        let failed = report.failing("probe run refused");
        assert_eq!(failed.text, "first\nsecond 2\n");
        assert_eq!(failed.failure, Some("probe run refused"));
    }

    #[test]
    fn emit_passes_on_the_failure_or_the_error() {
        assert!(emit(Ok(Report::default())).is_ok());
        let failed = emit(Ok(Report::default().failing("probe run aborted")));
        assert_eq!(failed.unwrap_err().to_string(), "probe run aborted");
        let stopped = emit(Err("no pulse-null.toml".into()));
        assert_eq!(stopped.unwrap_err().to_string(), "no pulse-null.toml");
    }

    #[test]
    fn an_empty_variable_is_not_set() {
        assert!(env_is_set("PATH"));
        assert!(!env_is_set("PULSE_NULL_PROBE_TEST_NEVER_SET"));
    }

    fn finished(status: RunStatus) -> RunOutcome {
        RunOutcome::Finished(RunSummary {
            run: "r1".into(),
            status,
            reason: None,
            turns: 15,
            input_tokens: 15_000,
            output_tokens: 600,
            items_recorded: 45,
            alerts: Vec::new(),
        })
    }

    #[test]
    fn a_refused_run_fails_only_by_hand() {
        let refused = RunOutcome::Refused("not due".into());
        let timer = run_report(&refused, true);
        assert_eq!(plain(&timer.text), "  ◉ probe run refused: not due\n");
        assert_eq!(timer.failure, None);
        let by_hand = run_report(&refused, false);
        assert_eq!(by_hand.text, timer.text);
        assert_eq!(by_hand.failure, Some("probe run refused"));
    }

    #[test]
    fn a_finished_run_shows_its_spend_why_it_stopped_and_its_alerts() {
        let RunOutcome::Finished(mut summary) = finished(RunStatus::Partial) else {
            unreachable!()
        };
        summary.reason = Some("time budget spent".into());
        summary.alerts = vec!["claude/.credentials.json changed".into()];
        let report = run_report(&RunOutcome::Finished(summary), false);
        assert_eq!(
            plain(&report.text),
            "  ◉ probe run r1 partial — 15 turns · 15000 in / 600 out tokens · 45 answers recorded\n\
             \x20   stopped: time budget spent\n\
             \x20   ALERT  claude/.credentials.json changed\n"
        );
        assert_eq!(report.failure, None);
    }

    #[test]
    fn only_an_aborted_run_fails() {
        let complete = run_report(&finished(RunStatus::Complete), false);
        assert!(plain(&complete.text).contains("probe run r1 complete — "));
        assert_eq!(complete.failure, None);
        let aborted = run_report(&finished(RunStatus::Aborted), true);
        assert!(plain(&aborted.text).contains("probe run r1 aborted — "));
        assert_eq!(aborted.failure, Some("probe run aborted"));
    }

    fn fixture_host() -> RunHost {
        RunHost {
            now: Utc::now(),
            landlock: LandlockSupport::detect(),
            recall_bin: "/nonexistent/recall-echo".into(),
            memory: "recall-echo fixture · graph embedded".into(),
            credentials: None,
            has_env: &always,
            confinement: &confined,
            stop: std::sync::Arc::default(),
        }
    }

    #[tokio::test]
    async fn a_run_without_the_sealed_sets_is_refused_before_any_turn() {
        let fixture = Fixture::new(None);
        let by_hand = RunRequest::default();
        let report = run_for(&fixture.ctx, &by_hand, async |server_graph| {
            assert!(!server_graph, "the fixture's graph is embedded");
            Ok(fixture_host())
        })
        .await
        .unwrap();
        assert!(
            plain(&report.text).starts_with("  ◉ probe run refused: "),
            "{}",
            report.text
        );
        assert_eq!(report.failure, Some("probe run refused"));
    }

    #[tokio::test]
    async fn a_run_stops_when_the_host_cannot_be_probed() {
        let fixture = Fixture::new(None);
        let error = run_for(&fixture.ctx, &RunRequest::default(), async |_| {
            Err("no signal handler".into())
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "no signal handler");
    }

    #[test]
    fn status_of_a_pulse_never_probed() {
        let fixture = Fixture::new(None);
        let report = status_report(&fixture.ctx, Utc::now()).unwrap();
        let text = plain(&report.text);
        assert!(
            text.starts_with("  ◉ probe record — Fixture   (probes off · cadence "),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "\n    sets     sealed: delivered to the probe unit as credentials from {}/fixture\n",
                sealed::SETS_ROOT
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "\n    budget   0 of {} turns this week · 0 in / 0 out tokens\n",
                fixture.ctx.config.probe.weekly_turns
            )),
            "{text}"
        );
        assert!(
            text.contains("\n    cost     not measured yet (no complete run)\n"),
            "{text}"
        );
        assert!(text.contains("\n    chain    "), "{text}");
        assert_eq!(report.failure, None);
    }

    #[test]
    fn the_status_header_says_whether_probes_are_on() {
        let mut config = fixture_config();
        let off = plain(&status_header(&config, "Landlock ABI 6".into()));
        assert!(off.contains("(probes off · cadence "), "{off}");
        assert!(off.ends_with(" Europe/Madrid · Landlock ABI 6)"), "{off}");
        config.probe.enabled = true;
        assert!(plain(&status_header(&config, String::new())).contains("(probes on · "));
    }

    #[test]
    fn budget_and_cost_lines() {
        let week = WeekSpend {
            turns: 12,
            input_tokens: 9_000,
            output_tokens: 300,
        };
        assert_eq!(
            budget_line(&week, 60),
            "    budget   12 of 60 turns this week · 9000 in / 300 out tokens"
        );
        let cost = CostPerRun {
            runs: 2,
            turns: 15.0,
            input_tokens: 15_000.0,
            output_tokens: 600.4,
        };
        assert_eq!(
            cost_line(Some(&cost)),
            "    cost     per complete run: 15.0 turns · 15000 in / 600 out tokens (mean of 2)"
        );
        assert_eq!(
            cost_line(None),
            "    cost     not measured yet (no complete run)"
        );
    }

    #[test]
    fn a_run_without_a_finish_row_shows_as_crashed() {
        let mut run = RunLine {
            run: "r1".into(),
            started: Utc.with_ymd_and_hms(2026, 10, 16, 0, 30, 0).unwrap(),
            status: None,
            turns: 3,
            input_tokens: 10,
            output_tokens: 2,
        };
        assert_eq!(
            run_line(&run, chrono_tz::Tz::Europe__Madrid),
            "    run      2026-10-16 02:30  r1  no finish row (crashed) · 3 turns · 10 in / 2 out"
        );
        run.status = Some(RunStatus::Complete);
        assert!(run_line(&run, chrono_tz::Tz::UTC).contains("00:30  r1  complete · 3 turns"));
    }

    #[test]
    fn chains_show_breaks_and_skipped_lines() {
        let ok = ChainState {
            stream: "items",
            rows: 45,
            broken_at: None,
            rejected: 0,
            unreadable: 0,
        };
        let broken = ChainState {
            stream: "runs",
            rows: 4,
            broken_at: Some(5),
            rejected: 2,
            unreadable: 1,
        };
        assert_eq!(
            chains_line(&[ok, broken]),
            "items ok (45) · runs BROKEN at line 5 (4 verified, 2 rejected, 1 unreadable lines skipped)"
        );
        let skipped = ChainState {
            stream: "facts",
            rows: 3,
            broken_at: None,
            rejected: 0,
            unreadable: 2,
        };
        assert_eq!(
            chain_state(&skipped),
            "facts ok (3, 2 unreadable lines skipped)"
        );
        assert_eq!(chains_line(&[]), "");
    }

    fn key(block: Block) -> GroupKey {
        GroupKey {
            set: "situation-use/v1".into(),
            set_sha256: "ab".into(),
            baseline: "b1".into(),
            block,
            adapter: "claude".into(),
            configured_model: NOT_GROUPED.into(),
            served_model: "claude-opus-5-5".into(),
            binary: NOT_GROUPED.into(),
            memory: NOT_GROUPED.into(),
            scorer_version: "1".into(),
            reasoning_effort: "none".into(),
            cli_version: NOT_GROUPED.into(),
            disclosure_sha256: NOT_GROUPED.into(),
            fresh_sha256: NOT_GROUPED.into(),
        }
    }

    fn proportion(k: u32, n: u32) -> Proportion {
        Proportion {
            k,
            n,
            low: 0.25,
            high: 0.75,
        }
    }

    fn arm(name: &str, k: u32, n: u32) -> ArmResult {
        ArmResult {
            arm: name.into(),
            proportion: proportion(k, n),
            void: 0,
            min_n: Some(10),
            constant_answer: None,
            unhedged: None,
            wrong: 0,
            elapsed_days: None,
            exploratory: false,
        }
    }

    #[test]
    fn a_lineage_leaves_out_what_the_set_is_not_grouped_by() {
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let open = Block::Weeks {
            start: chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
        };
        let (line, readable) = lineage_line(&key(open), now);
        assert_eq!(
            line,
            "situation-use/v1 · b1 · claude · served claude-opus-5-5 · effort none · scorer v1 · \
             block 2026-10-01 → 2026-10-29 (open, not a result until it closes)"
        );
        assert!(!readable);

        let mut grouped = key(Block::Facts { batch: 2 });
        grouped.configured_model = "opus".into();
        grouped.cli_version = "2.1.0".into();
        grouped.binary = "pulse-null 0.32.0".into();
        grouped.memory = "recall-echo 4.8.0 · graph server".into();
        grouped.disclosure_sha256 = "1a2b3c4d5e6f".into();
        grouped.fresh_sha256 = "abc".into();
        let (line, readable) = lineage_line(&grouped, now);
        assert_eq!(
            line,
            "situation-use/v1 · b1 · claude · configured opus · served claude-opus-5-5 · \
             effort none · cli 2.1.0 · pulse-null 0.32.0 · recall-echo 4.8.0 · graph server · \
             disclosure 1a2b3c4d · fresh abc · scorer v1 · batch 2"
        );
        assert!(readable);
    }

    #[test]
    fn a_group_is_its_lineage_then_one_line_per_arm() {
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let lines = group_lines(
            &key(Block::Facts { batch: 1 }),
            &[arm("pulse", 7, 10), arm("input-only", 10, 10)],
            false,
            now,
        );
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("    situation-use/v1 · b1 · "));
        assert_eq!(
            lines[1],
            format!(
                "      {:<11} {:<27} 7 of 10 · 70% (25–75)",
                "pulse", "gave the fact when asked"
            )
        );
        assert!(lines[2].contains("ceiling check (facts given)"));
    }

    #[test]
    fn an_arm_reads_as_a_number_only_when_its_block_allows() {
        let readable = BlockState {
            interrupted: false,
            readable: true,
        };
        let pulse = arm("pulse", 7, 10);
        assert_eq!(arm_reading(&pulse, readable), "7 of 10 · 70% (25–75)");
        assert_eq!(
            arm_reading(
                &pulse,
                BlockState {
                    interrupted: true,
                    readable: true
                }
            ),
            "10 answers: block interrupted by a lineage change, not a result"
        );
        assert_eq!(
            arm_reading(
                &pulse,
                BlockState {
                    interrupted: false,
                    readable: false
                }
            ),
            "10 answers so far (read when the block closes)"
        );
        let mut twin = arm("twin-7d", 0, 10);
        twin.wrong = 3;
        assert_eq!(
            arm_reading(&twin, readable),
            "made up 3 of 10 it was never told"
        );
        let few = arm("planted-7d", 2, 4);
        assert_eq!(arm_reading(&few, readable), "not enough yet · 4 of 10");
        let mut uninstalled = few.clone();
        uninstalled.min_n = None;
        assert_eq!(
            arm_reading(&uninstalled, readable),
            "not enough yet · 4 of 0"
        );
    }

    /// R3-A: the fresh arm's two asks, on the page: the volunteered answer
    /// as what a model with no memory offers, the forced guess's unhedged
    /// hits as the chance rate (shown once, as its number).
    #[test]
    fn the_forced_guess_reads_as_the_chance_rate() {
        assert_eq!(
            arm_role("paired-facts/v1", "chance 7d"),
            "volunteered with no memory"
        );
        assert_eq!(
            arm_role("paired-facts/v1", "guess 7d"),
            "chance rate (forced guess)"
        );
        assert_eq!(
            arm_role("paired-facts/v1", "planted 7d"),
            "remembered what it was told"
        );
        let readable = BlockState {
            interrupted: false,
            readable: true,
        };
        let mut guess = arm("guess 7d", 4, 10);
        guess.unhedged = Some(proportion(2, 10));
        assert_eq!(
            arm_reading(&guess, readable),
            "chance 2 of 10 unhedged · 20% (25–75)"
        );
        assert!(!arm_notes(&guess, true).contains("unhedged"));
        guess.unhedged = None;
        assert_eq!(
            arm_reading(&guess, readable),
            "chance 4 of 10 unhedged · 40% (25–75)",
            "rows that do not say which were hedged: all hits"
        );
        let few = arm("guess 7d", 1, 4);
        assert_eq!(arm_reading(&few, readable), "not enough yet · 4 of 10");
        let mut planted = arm("planted 7d", 8, 10);
        planted.unhedged = Some(proportion(6, 10));
        assert!(arm_notes(&planted, true).contains(" · unhedged 6 of 10"));
    }

    #[test]
    fn an_arms_notes_qualify_its_number() {
        assert_eq!(arm_notes(&arm("pulse", 7, 10), true), "");
        let mut planted = arm("planted-30d", 8, 10);
        planted.void = 2;
        planted.elapsed_days = Some((30, 32));
        planted.exploratory = true;
        planted.unhedged = Some(proportion(6, 10));
        planted.constant_answer = Some(proportion(5, 10));
        assert_eq!(
            arm_notes(&planted, true),
            " · 2 void · asked 30–32 d after the plant · exploratory (v1 decides on 1 and 7 days) \
             · unhedged 6 of 10 · RESTARTED/SAME: a constant answer gets 5 of 10"
        );
        // Unhedged hits are shown only beside a number that can be read.
        assert!(!arm_notes(&planted, false).contains("unhedged"));
        let mut few = planted.clone();
        few.proportion = proportion(1, 2);
        assert!(!arm_notes(&few, true).contains("unhedged"));
        assert_eq!(asked_after((7, 7)), " · asked 7 d after the plant");
    }

    #[test]
    fn the_graph_copy_check_says_whether_the_server_is_gone() {
        let gone = graph_stopped_check(41000, true);
        assert!(gone.passed);
        assert_eq!(
            gone.detail,
            "the copy on [::1]:41000 is gone; the live server was only read"
        );
        let answers = graph_stopped_check(41000, false);
        assert!(!answers.passed);
        assert_eq!(answers.detail, "[::1]:41000 still answers");
    }

    #[tokio::test]
    async fn no_graph_copy_means_no_graph_check() {
        assert_eq!(stop_graph_copy(None).await, None);
    }

    #[test]
    fn an_isolation_report_fails_on_any_failed_check() {
        let passed = IsolationResult {
            checks: vec![check("write: live pulse home", true)],
            changed_vendor_files: Vec::new(),
            kept_scratch: None,
        };
        let report = passed.report("Fixture", "Landlock ABI 6".into());
        assert_eq!(
            plain(&report.text),
            format!(
                "  ◉ probe isolation — Fixture (Landlock ABI 6)\n    pass  {:<40} write: live pulse home detail\n",
                "write: live pulse home"
            )
        );
        assert_eq!(report.failure, None);

        let failed = IsolationResult {
            checks: vec![check("a", true), check("b", false)],
            changed_vendor_files: vec![PathBuf::from("claude/.credentials.json")],
            kept_scratch: Some(PathBuf::from("/state/s/abc")),
        };
        let report = failed.report("Fixture", String::new());
        let text = plain(&report.text);
        assert!(
            text.contains(&format!("\n    FAIL  {:<40} b detail\n", "b")),
            "{text}"
        );
        assert!(text.contains(
            "\n    ALERT  claude/.credentials.json changed during the probe; it was not copied back\n"
        ));
        assert!(
            text.ends_with("\n  scratch kept at /state/s/abc\n"),
            "{text}"
        );
        assert_eq!(report.failure, Some("probe isolation check failed"));
    }

    #[test]
    fn the_cli_check_shows_its_version_or_its_error() {
        let started = version_check(Ok::<_, String>("2.1.0 (Claude Code)".into()));
        assert_eq!(
            started,
            check_with(CLI_STARTS, true, "--version: 2.1.0 (Claude Code)")
        );
        let refused = version_check(Err::<String, _>("x".repeat(400)));
        assert!(!refused.passed);
        assert_eq!(refused.detail, "x".repeat(300));
    }

    fn check_with(name: &str, passed: bool, detail: &str) -> Check {
        Check {
            name: name.into(),
            passed,
            detail: detail.into(),
        }
    }

    #[test]
    fn the_turn_check_reports_what_the_turn_spent() {
        let run: Result<CapturedRun, String> = Ok(captured("probe ok", 2));
        assert_eq!(
            turn_check(&run, true),
            check_with(
                TURN_COMPLETED,
                true,
                "reply \"probe ok\" · served claude-opus-5-5 · 2 tool calls · 1000 in / 40 out tokens"
            )
        );
        assert!(turn_check(&run, false)
            .detail
            .contains(" · tool calls not observable on this CLI · "));
        let failed: Result<CapturedRun, String> = Err("Invalid API key".into());
        assert_eq!(
            turn_check(&failed, true),
            check_with(TURN_COMPLETED, false, "Invalid API key")
        );
    }

    #[test]
    fn the_vendor_files_check_wants_new_files_in_scratch() {
        assert!(vendor_files_check(3, 5).passed);
        let none = vendor_files_check(3, 3);
        assert!(!none.passed);
        assert_eq!(none.detail, "3 → 3 files in the scratch vendor dir");
    }

    #[test]
    fn a_live_vendor_dir_naming_the_scratch_is_a_leak() {
        let vendor = tempfile::tempdir().unwrap();
        assert_eq!(leak_check(&vendor.path().join("missing"), "abc123"), None);
        let clean = leak_check(vendor.path(), "abc123").unwrap();
        assert!(clean.passed);
        assert_eq!(
            clean.detail,
            format!("no entry in {} names abc123", vendor.path().display())
        );
        let leaked_dir = vendor.path().join("projects/-state-s-abc123-item-0");
        std::fs::create_dir_all(&leaked_dir).unwrap();
        let leaked = leak_check(vendor.path(), "abc123").unwrap();
        assert!(!leaked.passed);
        assert_eq!(
            leaked.detail,
            format!("1 entries, e.g. {}", leaked_dir.display())
        );
    }

    #[test]
    fn the_date_check_fails_only_a_date_the_cli_handed_over() {
        let unknown = date_check(&captured("UNKNOWN", 0), "2026-10-18", true);
        assert!(unknown.passed);
        assert_eq!(unknown.name, FRESH_DATE_CHECK);
        assert_eq!(unknown.detail, "reply \"UNKNOWN\": no date of its own");
        let looked_up = date_check(&captured("2026-10-18", 1), "2026-10-18", true);
        assert!(looked_up.passed);
        assert_eq!(
            looked_up.detail,
            "it gave 2026-10-18, from a tool call it made"
        );
        let handed = date_check(&captured("2026-10-18", 0), "2026-10-18", true);
        assert!(!handed.passed);
        assert_eq!(
            handed.detail,
            "it gave 2026-10-18 with no tool call: the CLI hands the fresh arm the date"
        );
        let unseen = date_check(&captured("2026-10-18", 0), "2026-10-18", false);
        assert!(!unseen.passed);
        assert_eq!(
            unseen.detail,
            "it gave 2026-10-18; this CLI does not show its tool calls, so the source is unknown"
        );
    }

    /// The fixture pulse's `pulse-null.toml` (as `fixture_config`), with
    /// `llm` as its `[llm]` table.
    fn pulse_toml(llm: &str) -> String {
        format!(
            "[pulse]\nname = \"Fixture\"\nowner_name = \"Dee\"\nowner_alias = \"dee\"\n\n\
             [server]\nport = 3299\n\n[llm]\n{llm}\n\n[security]\n\n\
             [scheduler]\ntimezone = \"Europe/Madrid\"\n"
        )
    }

    const CLI_LLM: &str = "provider = \"cli\"\nadapter = \"claude\"\nmodel = \"claude-opus-5-5\"";

    /// A pulse directory holding `toml` as its config.
    fn pulse_dir_with(toml: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pulse-null.toml"), toml).unwrap();
        dir
    }

    #[test]
    fn a_probe_context_is_the_pulse_its_config_sits_in() {
        let pulse = pulse_dir_with(&pulse_toml(CLI_LLM));
        let home = tempfile::tempdir().unwrap();
        let ctx = ProbeContext::at(
            &pulse.path().join("pulse-null.toml"),
            Some(home.path().into()),
        )
        .unwrap();
        assert_eq!(ctx.live_root, pulse.path().canonicalize().unwrap());
        assert_eq!(ctx.home, home.path());
        assert_eq!(ctx.adapter_name, "claude");
        assert_eq!(ctx.config.pulse.name, "Fixture");
        assert!(ctx.live_graph.is_none(), "no recall-echo config: embedded");
    }

    #[test]
    fn a_probe_context_needs_a_home_an_agent_cli_and_a_readable_graph_config() {
        let config = |dir: &tempfile::TempDir| dir.path().join("pulse-null.toml");
        let home = || Some(std::ffi::OsString::from("/home/fixture"));
        let error = |result: Result<ProbeContext, Box<dyn Error>>| match result {
            Ok(_) => panic!("the context should be refused"),
            Err(e) => e.to_string(),
        };

        let pulse = pulse_dir_with(&pulse_toml(CLI_LLM));
        assert_eq!(
            error(ProbeContext::at(&config(&pulse), None)),
            "HOME is not set"
        );

        let ollama = pulse_dir_with(&pulse_toml("provider = \"ollama\"\nmodel = \"llama3\""));
        assert!(error(ProbeContext::at(&config(&ollama), home()))
            .starts_with("probes need an agent CLI pulse"));

        std::fs::create_dir_all(pulse.path().join("memory")).unwrap();
        std::fs::write(
            pulse.path().join("memory/.recall-echo.toml"),
            "[graph]\nmode = \"server\"\n",
        )
        .unwrap();
        assert!(error(ProbeContext::at(&config(&pulse), home())).contains("[graph] url is missing"));

        let empty = tempfile::tempdir().unwrap();
        assert!(ProbeContext::at(&config(&empty), home()).is_err());
        assert!(ProbeContext::at(Path::new("/"), home()).is_err());
    }

    fn fixture_pairs() -> Result<LoadedPairs, Box<dyn Error>> {
        Ok(LoadedPairs::fixture())
    }

    fn no_credentials() -> Result<LoadedPairs, Box<dyn Error>> {
        Err("no credentials".into())
    }

    #[test]
    fn the_pairs_come_from_the_sealed_sets_or_not_at_all() {
        let pairs = pairs_from(Ok(crate::vigil::probe::sealed::tests::fixture_sets())).unwrap();
        assert_eq!(pairs.label(), "paired-facts/v1");
        let refused = pairs_from(Err(SealError::NoCredentials)).unwrap_err();
        assert_eq!(refused.to_string(), SealError::NoCredentials.to_string());
    }

    fn at(day: u32, hour: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0).unwrap()
    }

    #[test]
    fn a_fact_is_drawn_planted_and_listed() {
        let fixture = Fixture::new(None);
        let ctx = &fixture.ctx;
        assert_eq!(
            facts_list_for(ctx, no_credentials, at(1, 9)).unwrap().text,
            "  no facts drawn yet\n",
            "without a record no credentials are needed"
        );

        let drawn = plain(&facts_draw_for(ctx, fixture_pairs, at(2, 9)).unwrap().text);
        assert!(drawn.starts_with("  ◉ fact "), "{drawn}");
        assert!(drawn.contains(" drawn. In normal conversation with Fixture, mention "));
        assert!(drawn.contains(": that side is the baseline.\n"));
        let record = Record::existing(&ctx.live_root).unwrap();
        let rows = record
            .read(crate::vigil::probe::record::Stream::Facts)
            .unwrap()
            .rows;
        let fact = paired::facts_from_rows(&rows, &LoadedPairs::fixture()).remove(0);
        assert!(drawn.contains(&format!("probe facts plant {} ", fact.fact)));
        let say = drawn
            .split("with the words \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .to_string();

        let listed = facts_list_for(ctx, fixture_pairs, at(2, 10)).unwrap();
        assert!(
            listed.text.contains("drawn, not planted yet"),
            "{}",
            listed.text
        );
        assert!(listed.text.starts_with(&format!("  {}  ", fact.fact)));

        std::fs::create_dir_all(ctx.live_root.join("sessions/wal")).unwrap();
        std::fs::write(
            ctx.live_root.join("sessions/wal/owner.jsonl"),
            crate::vigil::probe::owner::tests::wal_line(
                &format!("by the way, my {say} is Lychee"),
                at(2, 10),
                "tui",
                "user",
            ) + "\n",
        )
        .unwrap();
        let planting = Planting {
            fact: fact.fact.clone(),
            value: "Lychee".into(),
            now: at(2, 11),
        };
        let planted = facts_plant_for(ctx, fixture_pairs, &planting).unwrap();
        assert_eq!(
            plain(&planted.text),
            format!(
                "  ◉ {} planted — found in sessions/wal/owner.jsonl at 2026-10-02 10:00 UTC. \
                 It is asked after [1, 7, 30, 90] days.\n",
                fact.fact
            )
        );
        let listed = facts_list_for(ctx, fixture_pairs, at(3, 9)).unwrap();
        assert!(
            listed.text.contains(" · planted 2026-10-02 · "),
            "{}",
            listed.text
        );

        let again = facts_plant_for(ctx, fixture_pairs, &planting);
        assert!(again.is_err(), "a fact is planted once");
    }

    #[test]
    fn the_fact_commands_stop_without_the_sealed_sets() {
        let fixture = Fixture::new(None);
        let ctx = &fixture.ctx;
        let refused = |report: Result<Report, Box<dyn Error>>| report.unwrap_err().to_string();
        assert_eq!(
            refused(facts_draw_for(ctx, no_credentials, at(2, 9))),
            "no credentials"
        );
        let planting = Planting {
            fact: "f-1".into(),
            value: "Lychee".into(),
            now: at(2, 9),
        };
        assert_eq!(
            refused(facts_plant_for(ctx, no_credentials, &planting)),
            "no credentials"
        );
        Record::open(&ctx.live_root).unwrap();
        assert_eq!(
            refused(facts_list_for(ctx, no_credentials, at(2, 9))),
            "no credentials"
        );
        assert_eq!(
            facts_list_for(ctx, fixture_pairs, at(2, 9)).unwrap().text,
            "  no facts drawn yet\n"
        );
    }

    #[test]
    fn a_fact_line_says_when_its_asks_are_over() {
        let fact = paired::Fact {
            fact: "f-1".into(),
            pair: "lighthouse".into(),
            planted: paired::Which::B,
            drawn_at: at(1, 9),
            value: Some("helix".into()),
            planted_at: Some(at(1, 10)),
            asked: vec![1, 7, 30, 90],
        };
        assert_eq!(
            fact_line(&fact, &[1, 7, 30, 90], at(20, 9)),
            format!(
                "  f-1  {:<16} side B · planted 2026-10-01 · asked at [1, 7, 30, 90] days · finished",
                "lighthouse"
            )
        );
        let open = paired::Fact {
            asked: vec![1],
            ..fact
        };
        assert!(fact_line(&open, &[1, 7, 30, 90], at(3, 9)).ends_with("asked at [1] days"));
    }

    #[test]
    fn a_plant_notes_a_twin_that_came_up_too() {
        let mention = paired::Mention {
            at: at(2, 10),
            source: "archives/conversations/c-1.md".into(),
        };
        let report = plain(&planted_report("f-1", &mention, &[1, 7], true).text);
        assert!(report.starts_with(
            "  ◉ f-1 planted — found in archives/conversations/c-1.md at 2026-10-02 10:00 UTC. \
             It is asked after [1, 7] days.\n"
        ));
        assert!(report.ends_with(
            "\n    note  the other side of the pair came up in conversation too: its answers will be void\n"
        ));
    }

    #[test]
    fn sets_install_only_as_root_and_only_a_plain_pulse_name() {
        let from = tempfile::tempdir().unwrap();
        let registry = crate::vigil::probe::sealed::tests::fixture_credentials(from.path());
        let dest = tempfile::tempdir().unwrap();
        let request = |pulse: &str| SetsInstall {
            from: from.path().to_path_buf(),
            pulse: pulse.into(),
            dest: Some(dest.path().to_path_buf()),
        };
        let as_user = install_sets(&request("echo"), 1000, || Ok(registry.clone()));
        assert!(as_user
            .unwrap_err()
            .to_string()
            .starts_with("probe sets install runs as root"));
        let dotted = install_sets(&request("../echo"), 0, || Ok(registry.clone()));
        assert!(dotted
            .unwrap_err()
            .to_string()
            .contains("not a plain pulse name"));
        let unpinned = install_sets(&request("echo"), 0, || Err(SealError::NoCredentials));
        assert!(unpinned.is_err());
        assert!(!dest.path().join("echo").exists(), "nothing was installed");

        let report = install_sets(&request("Echo"), 0, || Ok(registry.clone())).unwrap();
        let text = plain(&report.text);
        let dir = dest.path().join("echo");
        assert!(
            text.starts_with(&format!(
                "  ◉ installed into {} (root 0700, files 0600):\n",
                dir.display()
            )),
            "{text}"
        );
        assert!(text.ends_with(&format!(
            "    You can delete the copy in {} now.\n",
            from.path().display()
        )));
        for file in registry.files() {
            assert!(text.contains(&format!("\n    {file}\n")), "{file}: {text}");
            assert!(dir.join(file).is_file(), "{file}");
        }
        assert_eq!(report.failure, None);
    }

    #[tokio::test]
    async fn a_pulse_without_a_graph_server_has_nothing_to_export() {
        let fixture = Fixture::new(None);
        let report = export_for(&fixture.ctx).await.unwrap();
        assert_eq!(
            report.text,
            "  Fixture's graph is not on a server: nothing to export\n"
        );
    }

    #[test]
    fn an_export_reports_its_size_and_time() {
        let stats = graph::ExportStats {
            bytes: 2048,
            elapsed: std::time::Duration::from_millis(1500),
        };
        let report = export_report("Echo", Path::new("/state/echo.surql"), Some(&stats));
        assert_eq!(
            plain(&report.text),
            "  ◉ exported Echo's graph to /state/echo.surql (2048 bytes in 1.5s)\n"
        );
    }

    #[tokio::test]
    async fn the_stop_flag_is_set_by_either_signal() {
        use tokio::signal::unix::SignalKind;
        // The user signals stand in for SIGTERM and SIGINT, which the test
        // process must keep for itself.
        let stop = stop_on([SignalKind::user_defined1(), SignalKind::user_defined2()]).unwrap();
        assert!(!stop.load(std::sync::atomic::Ordering::SeqCst));
        // SAFETY: raise has no preconditions; SIGUSR2 is handled by the
        // listener just registered, so it cannot end the process.
        assert_eq!(unsafe { libc::raise(libc::SIGUSR2) }, 0);
        let set = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(set.is_ok(), "the flag was not set");
    }

    /// Outside the probe unit: one confinement check fails, as by hand.
    fn unconfined() -> VerifyHost {
        VerifyHost {
            confinement: vec![check("unit: loopback refused", false)],
            has_env: &always,
            unit_ports: &[],
        }
    }

    /// Scratch dirs left under the fixture's scratch parent.
    fn scratch_left(ctx: &ProbeContext) -> Vec<PathBuf> {
        std::fs::read_dir(scratch_parent(&ctx.home))
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn verify_isolation_runs_its_checks_and_turns_in_a_scratch_copy() {
        if !landlock_or_skip("verify_isolation_runs_its_checks_and_turns_in_a_scratch_copy") {
            return;
        }
        let cli = FakeCli::new(
            Act::Answer("probe ok"),
            Act::Answer("probe ok"),
            Act::Answer("UNKNOWN"),
        );
        let fixture = Fixture::new(Some(&cli.bin()));
        let request = VerifyRequest {
            turn: true,
            keep_scratch: false,
        };
        let report = verify_for(&fixture.ctx, &request, unconfined)
            .await
            .unwrap();
        let text = plain(&report.text);
        assert!(
            text.starts_with("  ◉ probe isolation — Fixture ("),
            "{text}"
        );
        let line = |name: &str| {
            text.lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("no {name} line in {text}"))
                .to_string()
        };
        assert!(line("live pulse home").starts_with("    pass  "), "{text}");
        assert!(line("unit: loopback refused").starts_with("    FAIL  "));
        assert!(line(CLI_STARTS).starts_with("    pass  "), "{text}");
        assert!(
            line(TURN_COMPLETED).contains("reply \"probe ok\""),
            "{text}"
        );
        assert!(line(FRESH_DATE_CHECK).starts_with("    pass  "), "{text}");
        assert!(line(FRESH_DATE_CHECK).contains("reply \"UNKNOWN\": no date of its own"));
        // The fixture's failed confinement check fails the whole check.
        assert_eq!(report.failure, Some("probe isolation check failed"));
        assert!(!text.contains("scratch kept at"));
        assert_eq!(scratch_left(&fixture.ctx), Vec::<PathBuf>::new());
    }

    #[tokio::test]
    async fn verify_isolation_without_turns_can_keep_its_scratch() {
        if !landlock_or_skip("verify_isolation_without_turns_can_keep_its_scratch") {
            return;
        }
        let cli = FakeCli::new(Act::Quota, Act::Quota, Act::Quota);
        let fixture = Fixture::new(Some(&cli.bin()));
        let request = VerifyRequest {
            turn: false,
            keep_scratch: true,
        };
        let report = verify_for(&fixture.ctx, &request, unconfined)
            .await
            .unwrap();
        let text = plain(&report.text);
        assert!(text.contains(CLI_STARTS), "{text}");
        assert!(
            !text.contains(TURN_COMPLETED),
            "no turn was asked for: {text}"
        );
        let kept = scratch_left(&fixture.ctx);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert!(
            text.ends_with(&format!("\n  scratch kept at {}\n", kept[0].display())),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_fresh_arm_that_cannot_answer_fails_its_date_check() {
        if !landlock_or_skip("a_fresh_arm_that_cannot_answer_fails_its_date_check") {
            return;
        }
        let cli = FakeCli::new(
            Act::Answer("probe ok"),
            Act::Answer("probe ok"),
            Act::AuthFailure,
        );
        let fixture = Fixture::new(Some(&cli.bin()));
        let request = VerifyRequest {
            turn: true,
            keep_scratch: false,
        };
        let report = verify_for(&fixture.ctx, &request, unconfined)
            .await
            .unwrap();
        let text = plain(&report.text);
        let fresh = text
            .lines()
            .find(|l| l.contains(FRESH_DATE_CHECK))
            .unwrap_or_else(|| panic!("no fresh-arm line in {text}"));
        assert!(fresh.starts_with("    FAIL  "), "{fresh}");
        assert_eq!(report.failure, Some("probe isolation check failed"));
    }

    #[tokio::test]
    async fn the_canned_turn_needs_the_units_login() {
        if !landlock_or_skip("the_canned_turn_needs_the_units_login") {
            return;
        }
        let cli = FakeCli::new(Act::Quota, Act::Quota, Act::Quota);
        let fixture = Fixture::new(Some(&cli.bin()));
        let request = VerifyRequest {
            turn: true,
            keep_scratch: false,
        };
        let logged_out = || VerifyHost {
            has_env: &never,
            ..unconfined()
        };
        let error = verify_for(&fixture.ctx, &request, logged_out)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("CLAUDE_CODE_OAUTH_TOKEN"),
            "{error}"
        );
        assert_eq!(scratch_left(&fixture.ctx), Vec::<PathBuf>::new());
    }

    /// G10 and audit #4: no v1 label credits the pulse with acting on its
    /// situation, and input-only is never shown as a baseline that can lose.
    #[test]
    fn labels_never_claim_acting_or_a_losing_input_only_baseline() {
        for set in ["known-answer/v1", "situation-use/v1"] {
            for arm in ["pulse", "input-only", "fresh"] {
                let role = arm_role(set, arm).to_lowercase();
                assert!(
                    !role.contains("acted") && !role.contains("used its"),
                    "{role}"
                );
                assert!(!role.contains("input-only model"), "{role}");
            }
        }
        assert_eq!(
            arm_role("situation-use/v1", "input-only"),
            "ceiling check (facts given)"
        );
        assert_eq!(
            arm_role("situation-use/v1", "pulse"),
            "gave the fact when asked"
        );
    }
}
