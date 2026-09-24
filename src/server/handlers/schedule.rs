//! `GET /api/schedule`, `POST /api/schedule/{id}/enable|disable`,
//! `GET /api/schedule/{id}/last`.
//!
//! The read model behind the TUI's schedule pane. Three properties matter
//! more than the JSON shape:
//!
//! * **Disk is the truth.** Every request re-reads `schedule.json` and
//!   `task_health.json` rather than serving the process's in-memory copy —
//!   the CLI, the pulse's own `[SCHEDULE:]` markers and this API all write
//!   the same file, so a cached view is a stale view.
//! * **Writes go through [`Schedule::save_delta`]**, exactly as the CLI does.
//!   The daemon rewrites `schedule.json` wholesale, so a read-modify-write
//!   that does not re-read under the lock loses concurrent edits.
//! * **Enable and disable are not symmetric.** Task loops are spawned once,
//!   at tenure start, and each loop re-checks the on-disk `enabled` flag
//!   before it fires. A disable therefore lands at the task's next fire; an
//!   enable has no loop to land in and waits for the next tenure. The
//!   response says which, because "it did nothing" is the single most
//!   confusing thing this endpoint can do.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, NaiveDateTime, Utc};
use cron::Schedule as CronSchedule;
use serde::Serialize;

use crate::errors::SchedulerError;
use crate::scheduler::health::{TaskHealth, TaskHealthStore};
use crate::scheduler::humanize::humanize;
use crate::scheduler::{normalize_cron, Schedule, ScheduleEntry, TaskCreator};
use crate::server::AppState;

/// Directory `crate::logbook::write_task_output` writes to.
const TASK_OUTPUT_DIR: &str = "task-output";

/// Length of the `YYYYmmdd-HHMMSS-` prefix on a task output filename.
const OUTPUT_STAMP_LEN: usize = "YYYYmmdd-HHMMSS".len();

/// An error body: `{"error": "..."}` with the status the caller should send.
type ApiError = (StatusCode, Json<serde_json::Value>);

/// Handler result: a JSON body on success, a JSON error body on failure.
type ApiResult<T> = Result<Json<T>, ApiError>;

// ---------------------------------------------------------------------------
// View model
// ---------------------------------------------------------------------------

/// One scheduled task as the TUI needs it: definition, cadence in words, and
/// enough liveness to colour a row without a second request.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleTaskView {
    pub id: String,
    pub name: String,
    /// The raw 6-field cron expression, unchanged.
    pub cron: String,
    /// [`humanize`]d cron, or the raw expression when no exact label exists.
    pub cadence: String,
    pub enabled: bool,
    pub created_by: &'static str,
    /// The task's own model *override*, not the effective model: `None`
    /// means "follows `[llm] model`", which is a different fact from "pinned
    /// to whatever `[llm] model` currently says".
    pub model: Option<String>,
    pub channel: String,
    /// The most recent recorded outcome, success or failure.
    pub last_run: Option<LastRun>,
    /// Failures since the last success; `0` for a healthy or unrun task.
    pub consecutive_failures: u32,
    /// Next fire in UTC — `None` when the task is disabled (it has no next
    /// fire) or its cron/timezone does not parse.
    pub next_fire: Option<DateTime<Utc>>,
}

/// The most recent recorded run of a task.
#[derive(Debug, Clone, Serialize)]
pub struct LastRun {
    pub at: DateTime<Utc>,
    pub ok: bool,
    /// The stored failure reason; always `None` when `ok`.
    pub error: Option<String>,
}

/// Result of a toggle, including when it actually takes effect.
#[derive(Debug, Clone, Serialize)]
pub struct ToggleResponse {
    pub task: ScheduleTaskView,
    /// `"now"` (disable: the loop re-reads disk before its next fire) or
    /// `"next_tenure"` (enable: loops only spawn at tenure start).
    pub effective: &'static str,
}

/// The stored output of a task's most recent run.
#[derive(Debug, Clone, Serialize)]
pub struct LastOutputView {
    pub id: String,
    /// When the run that produced this output finished, from the filename.
    pub at: DateTime<Utc>,
    /// The output body, with the front matter block removed; the tail of it
    /// when the run was longer than [`OUTPUT_CAP_BYTES`].
    pub output: String,
    /// True when `output` is the tail of a longer body.
    pub truncated: bool,
}

/// Most bytes of a task's output returned in one response.
pub const OUTPUT_CAP_BYTES: usize = 64 * 1024;

/// Task ids are `[A-Za-z0-9_-]{1,64}`; anything else is refused up front.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---------------------------------------------------------------------------
// Pure core
// ---------------------------------------------------------------------------

/// Every task in `schedule.json`, joined with its liveness record.
///
/// Reads disk on every call. `timezone` is an IANA name (`[scheduler]
/// timezone`); an unparseable one costs `next_fire`, not the listing.
pub fn list_tasks(root: &Path, timezone: &str) -> Result<Vec<ScheduleTaskView>, SchedulerError> {
    let schedule = Schedule::load(root)?;
    let health = TaskHealthStore::load(root);
    Ok(schedule
        .tasks
        .iter()
        .map(|entry| view(entry, &health, timezone))
        .collect())
}

/// Flip one task's `enabled` flag and return its refreshed view.
///
/// `Ok(None)` means no task carries that id — an absent task is a 404, not an
/// error. The mutation runs inside [`Schedule::save_delta`], so it merges with
/// whatever the CLI or the pulse wrote in the meantime.
pub fn set_enabled(
    root: &Path,
    timezone: &str,
    id: &str,
    enabled: bool,
) -> Result<Option<ScheduleTaskView>, SchedulerError> {
    let mut found = false;
    let merged = Schedule::save_delta(root, |schedule| {
        if let Some(entry) = schedule.find_task_mut(id) {
            entry.task.enabled = enabled;
            found = true;
        }
    })?;

    if !found {
        return Ok(None);
    }

    let health = TaskHealthStore::load(root);
    Ok(merged
        .find_task(id)
        .map(|entry| view(entry, &health, timezone)))
}

/// The stored output of a task's latest run, or `None` if nothing is on disk.
///
/// Output lives in `<root>/task-output/<YYYYmmdd-HHMMSS>-<id>.md`, written by
/// `crate::logbook::write_task_output` and pruned to the 50 most recent files
/// across all tasks — so a rarely-run task can legitimately have no output
/// even after it has run.
pub fn last_output(root: &Path, id: &str) -> Option<LastOutputView> {
    let dir = root.join(TASK_OUTPUT_DIR);
    let sanitized = sanitize_id(id);

    let latest = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let at = output_stamp(&name, &sanitized)?;
            Some((at, entry.path()))
        })
        .max_by_key(|(at, _)| *at)?;

    let (at, path) = latest;
    let content = std::fs::read_to_string(&path).ok()?;
    Some(LastOutputView {
        id: id.to_string(),
        at,
        output: {
            let body = strip_front_matter(&content).trim_end();
            if body.len() > OUTPUT_CAP_BYTES {
                let mut start = body.len() - OUTPUT_CAP_BYTES;
                while !body.is_char_boundary(start) {
                    start += 1;
                }
                body[start..].to_string()
            } else {
                body.to_string()
            }
        },
        truncated: strip_front_matter(&content).trim_end().len() > OUTPUT_CAP_BYTES,
    })
}

/// Parse the timestamp out of `<YYYYmmdd-HHMMSS>-<sanitized id>.md`, rejecting
/// any file that does not belong to exactly this task.
///
/// The match is on the whole remainder, not a suffix: `thinking-loop` and
/// `loop` are different tasks, and so are `thinking-loop` and
/// `thinking-loop-farm`.
fn output_stamp(filename: &str, sanitized_id: &str) -> Option<DateTime<Utc>> {
    let rest = filename.get(OUTPUT_STAMP_LEN..)?.strip_prefix('-')?;
    if rest != format!("{sanitized_id}.md") {
        return None;
    }
    let stamp = filename.get(..OUTPUT_STAMP_LEN)?;
    NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Mirrors the filename sanitizing in `crate::logbook::write_task_output`.
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Drop a leading `---\n…\n---\n` block, if present.
fn strip_front_matter(content: &str) -> &str {
    let Some(body) = content.strip_prefix("---\n") else {
        return content;
    };
    match body.split_once("\n---\n") {
        Some((_, rest)) => rest.trim_start_matches('\n'),
        None => content,
    }
}

/// Join one schedule entry with its liveness record.
fn view(entry: &ScheduleEntry, health: &TaskHealthStore, timezone: &str) -> ScheduleTaskView {
    let task = &entry.task;
    let task_health = health.get(&task.id);

    ScheduleTaskView {
        id: task.id.clone(),
        name: task.name.clone(),
        cadence: humanize(&task.cron),
        cron: task.cron.clone(),
        enabled: task.enabled,
        created_by: creator_label(&task.created_by),
        model: entry.model_override().map(str::to_string),
        channel: task.channel.clone(),
        last_run: task_health.and_then(last_run),
        consecutive_failures: task_health.map_or(0, |h| h.consecutive_failures),
        next_fire: task
            .enabled
            .then(|| next_fire(&task.cron, timezone))
            .flatten(),
    }
}

fn creator_label(creator: &TaskCreator) -> &'static str {
    match creator {
        TaskCreator::System => "system",
        TaskCreator::Entity => "entity",
        TaskCreator::User => "user",
    }
}

/// The later of the last success and the last failure.
///
/// `last_error` belongs to the current failure streak, so it is only reported
/// when the most recent run is the failure it describes.
fn last_run(health: &TaskHealth) -> Option<LastRun> {
    match (health.last_success, health.last_failure) {
        (Some(success), Some(failure)) if failure > success => Some(LastRun {
            at: failure,
            ok: false,
            error: health.last_error.clone(),
        }),
        (Some(success), _) => Some(LastRun {
            at: success,
            ok: true,
            error: None,
        }),
        (None, Some(failure)) => Some(LastRun {
            at: failure,
            ok: false,
            error: health.last_error.clone(),
        }),
        (None, None) => None,
    }
}

/// Next fire in UTC, evaluated in the scheduler's own timezone so that a DST
/// shift moves the answer the same way the running loop does.
fn next_fire(cron: &str, timezone: &str) -> Option<DateTime<Utc>> {
    let tz: chrono_tz::Tz = timezone.parse().ok()?;
    let schedule = CronSchedule::from_str(&normalize_cron(cron)).ok()?;
    let now = Utc::now().with_timezone(&tz);
    schedule
        .after(&now)
        .next()
        .map(|fire| fire.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// All tasks with cadence, enabled flag, creator, last run, next fire.
pub async fn list(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<crate::server::auth::AuthIdentity>,
) -> ApiResult<Vec<ScheduleTaskView>> {
    who.require_owner().map_err(forbidden)?;
    let root = state.root_dir.clone();
    let timezone = state.config.scheduler.timezone.clone();

    let tasks = blocking(move || list_tasks(&root, &timezone)).await?;
    Ok(Json(tasks))
}

/// Enable a task through `Schedule::save_delta` (same path as the CLI).
pub async fn enable(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<crate::server::auth::AuthIdentity>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<ToggleResponse> {
    who.require_owner().map_err(forbidden)?;
    toggle(state, id, true).await
}

/// Disable a task through `Schedule::save_delta` (same path as the CLI).
pub async fn disable(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<crate::server::auth::AuthIdentity>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<ToggleResponse> {
    who.require_owner().map_err(forbidden)?;
    toggle(state, id, false).await
}

async fn toggle(state: Arc<AppState>, id: String, enabled: bool) -> ApiResult<ToggleResponse> {
    if !valid_id(&id) {
        return Err(bad_request("task id must be [A-Za-z0-9_-]{1,64}"));
    }
    // "Nothing that writes" while isolated — same rule as /api/sessions/reset.
    if crate::server::isolation::is_active(&state.root_dir) {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "{} isolation mode active — schedule writes are shed until /resume",
                    crate::server::isolation::BANNER
                )
            })),
        ));
    }
    let root = state.root_dir.clone();
    let timezone = state.config.scheduler.timezone.clone();
    let task_id = id.clone();

    let task = blocking(move || set_enabled(&root, &timezone, &task_id, enabled)).await?;

    match task {
        Some(task) => Ok(Json(ToggleResponse {
            task,
            effective: if enabled { "next_tenure" } else { "now" },
        })),
        None => Err(not_found(format!("task not found: {id}"))),
    }
}

/// The most recent run's output for a task, or 404.
pub async fn last(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<crate::server::auth::AuthIdentity>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<LastOutputView> {
    who.require_owner().map_err(forbidden)?;
    if !valid_id(&id) {
        return Err(bad_request("task id must be [A-Za-z0-9_-]{1,64}"));
    }
    let root: PathBuf = state.root_dir.clone();
    let task_id = id.clone();

    let output = tokio::task::spawn_blocking(move || last_output(&root, &task_id))
        .await
        .map_err(|e| internal(&e))?;

    match output {
        Some(output) => Ok(Json(output)),
        None => Err(not_found(format!("no stored output for {id}"))),
    }
}

/// Run a disk-touching closure off the async runtime and flatten both the
/// join failure and the scheduler failure into a 500.
async fn blocking<T, F>(work: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, SchedulerError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| internal(&e))?
        .map_err(|e| internal(&e))
}

fn not_found(message: String) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": message })),
    )
}

fn internal(error: &dyn std::fmt::Display) -> ApiError {
    // The detail (file paths, serde positions) goes to the log, not the client.
    tracing::error!("schedule API: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": "internal error" })),
    )
}

fn bad_request(message: &str) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message })),
    )
}

fn forbidden(status: StatusCode) -> ApiError {
    (status, Json(serde_json::json!({ "error": "owner only" })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{OutputRouting, ScheduledTask};
    use tempfile::TempDir;

    const TZ: &str = "Europe/Madrid";

    fn seeded_root() -> TempDir {
        let dir = TempDir::new().unwrap();
        Schedule::with_defaults().save(dir.path()).unwrap();
        dir
    }

    fn find<'a>(tasks: &'a [ScheduleTaskView], id: &str) -> &'a ScheduleTaskView {
        tasks.iter().find(|t| t.id == id).unwrap()
    }

    #[test]
    fn list_tasks_renders_every_default_task() {
        let dir = seeded_root();
        let tasks = list_tasks(dir.path(), TZ).unwrap();

        let defaults = Schedule::with_defaults();
        assert_eq!(tasks.len(), defaults.tasks.len());
        assert!(!tasks.is_empty());

        for (view, entry) in tasks.iter().zip(defaults.tasks.iter()) {
            assert_eq!(view.id, entry.task.id);
            assert_eq!(view.cron, entry.task.cron);
            assert_eq!(view.cadence, humanize(&entry.task.cron));
            assert_eq!(view.channel, entry.task.channel);
            assert!(matches!(view.created_by, "system" | "entity" | "user"));
            // A fresh root has no task_health.json.
            assert!(view.last_run.is_none());
            assert_eq!(view.consecutive_failures, 0);
        }
    }

    #[test]
    fn list_tasks_errors_when_there_is_no_schedule() {
        let dir = TempDir::new().unwrap();
        assert!(list_tasks(dir.path(), TZ).is_err());
    }

    /// An enabled task has a next fire; a disabled one has none, because it
    /// is not going to fire.
    #[test]
    fn next_fire_tracks_the_enabled_flag() {
        let dir = seeded_root();
        let id = Schedule::load(dir.path()).unwrap().tasks[0].task.id.clone();

        let enabled = set_enabled(dir.path(), TZ, &id, true).unwrap().unwrap();
        assert!(enabled.enabled);
        let fire = enabled
            .next_fire
            .expect("enabled task must have a next fire");
        assert!(fire > Utc::now(), "next fire must be in the future");

        let disabled = set_enabled(dir.path(), TZ, &id, false).unwrap().unwrap();
        assert!(!disabled.enabled);
        assert!(disabled.next_fire.is_none());
    }

    /// An unparseable timezone costs the next-fire estimate and nothing else.
    #[test]
    fn unknown_timezone_drops_next_fire_but_still_lists() {
        let dir = seeded_root();
        let tasks = list_tasks(dir.path(), "Mars/Olympus_Mons").unwrap();
        assert!(!tasks.is_empty());
        assert!(tasks.iter().all(|t| t.next_fire.is_none()));
    }

    /// The write must land on disk, not just in the returned view: the
    /// daemon rewrites schedule.json, so an in-memory-only toggle is a lie.
    #[test]
    fn set_enabled_persists_through_a_reload() {
        let dir = seeded_root();
        let id = Schedule::load(dir.path()).unwrap().tasks[0].task.id.clone();

        let before = std::fs::read_to_string(dir.path().join("schedule.json")).unwrap();
        let view = set_enabled(dir.path(), TZ, &id, false).unwrap().unwrap();
        assert!(!view.enabled);

        let after = std::fs::read_to_string(dir.path().join("schedule.json")).unwrap();
        assert_ne!(before, after, "schedule.json on disk did not change");

        let reloaded = Schedule::load(dir.path()).unwrap();
        assert!(!reloaded.find_task(&id).unwrap().task.enabled);

        let listed = list_tasks(dir.path(), TZ).unwrap();
        assert!(!find(&listed, &id).enabled);

        set_enabled(dir.path(), TZ, &id, true).unwrap().unwrap();
        assert!(
            Schedule::load(dir.path())
                .unwrap()
                .find_task(&id)
                .unwrap()
                .task
                .enabled
        );
    }

    /// A toggle merges with concurrent edits instead of overwriting them,
    /// because it goes through save_delta like the CLI does.
    #[test]
    fn set_enabled_preserves_an_external_edit() {
        let dir = seeded_root();
        let ids: Vec<String> = Schedule::load(dir.path())
            .unwrap()
            .tasks
            .iter()
            .map(|e| e.task.id.clone())
            .collect();
        assert!(ids.len() >= 2);

        Schedule::save_delta(dir.path(), |s| {
            s.add_task(ScheduledTask {
                id: "external".to_string(),
                name: "External".to_string(),
                cron: "0 0 12 * * *".to_string(),
                channel: "system".to_string(),
                prompt: "p".to_string(),
                output_routing: OutputRouting::Silent,
                enabled: true,
                created_by: TaskCreator::Entity,
                evaluator: None,
            });
        })
        .unwrap();

        set_enabled(dir.path(), TZ, &ids[0], false)
            .unwrap()
            .unwrap();

        let listed = list_tasks(dir.path(), TZ).unwrap();
        assert!(find(&listed, "external").enabled);
        assert_eq!(find(&listed, "external").created_by, "entity");
        assert!(!find(&listed, &ids[0]).enabled);
    }

    #[test]
    fn set_enabled_reports_an_unknown_task_as_none() {
        let dir = seeded_root();
        assert!(set_enabled(dir.path(), TZ, "no-such-task", true)
            .unwrap()
            .is_none());
    }

    #[test]
    fn model_override_is_reported_but_never_invented() {
        let dir = seeded_root();
        let id = Schedule::load(dir.path()).unwrap().tasks[0].task.id.clone();

        assert!(find(&list_tasks(dir.path(), TZ).unwrap(), &id)
            .model
            .is_none());

        Schedule::save_delta(dir.path(), |s| {
            s.find_task_mut(&id).unwrap().model = Some("claude-opus-5".to_string());
        })
        .unwrap();

        assert_eq!(
            find(&list_tasks(dir.path(), TZ).unwrap(), &id)
                .model
                .as_deref(),
            Some("claude-opus-5")
        );
    }

    #[test]
    fn last_run_prefers_the_more_recent_outcome() {
        let older = Utc::now() - chrono::Duration::hours(2);
        let newer = Utc::now() - chrono::Duration::minutes(5);

        let failed = TaskHealth {
            last_success: Some(older),
            last_failure: Some(newer),
            last_error: Some("boom".to_string()),
            consecutive_failures: 3,
            ..Default::default()
        };
        let run = last_run(&failed).unwrap();
        assert_eq!(run.at, newer);
        assert!(!run.ok);
        assert_eq!(run.error.as_deref(), Some("boom"));

        let recovered = TaskHealth {
            last_success: Some(newer),
            last_failure: Some(older),
            last_error: Some("stale streak error".to_string()),
            ..Default::default()
        };
        let run = last_run(&recovered).unwrap();
        assert_eq!(run.at, newer);
        assert!(run.ok);
        assert!(
            run.error.is_none(),
            "a successful run must not carry the previous streak's error"
        );

        assert!(last_run(&TaskHealth::default()).is_none());
    }

    #[test]
    fn liveness_reaches_the_view() {
        let dir = seeded_root();
        let id = Schedule::load(dir.path()).unwrap().tasks[0].task.id.clone();
        let failed_at = Utc::now() - chrono::Duration::minutes(11);

        // The real write path: record_failure persists task_health.json.
        let mut store = TaskHealthStore::load(dir.path());
        for _ in 0..4 {
            store.record_failure(&id, &id, "provider offline", failed_at);
        }

        let view = find(&list_tasks(dir.path(), TZ).unwrap(), &id).clone();
        assert_eq!(view.consecutive_failures, 4);
        let run = view.last_run.unwrap();
        assert_eq!(run.at, failed_at);
        assert!(!run.ok);
        assert_eq!(run.error.as_deref(), Some("provider offline"));
    }

    // --- stored output -----------------------------------------------------

    fn write_output(root: &Path, filename: &str, task_id: &str, body: &str) {
        let dir = root.join(TASK_OUTPUT_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            "---\ntask_id: {task_id}\ntask_name: T\ndate: 2026-09-03 07:00 UTC\n\
             tokens_in: 1\ntokens_out: 2\ntool_rounds: 0\n---\n\n{body}\n"
        );
        std::fs::write(dir.join(filename), content).unwrap();
    }

    #[test]
    fn last_output_returns_the_newest_file_without_front_matter() {
        let dir = TempDir::new().unwrap();
        write_output(dir.path(), "20260901-070000-research.md", "research", "old");
        write_output(dir.path(), "20260903-070000-research.md", "research", "new");

        let output = last_output(dir.path(), "research").unwrap();
        assert_eq!(output.id, "research");
        assert_eq!(output.output, "new");
        assert_eq!(
            output.at,
            NaiveDateTime::parse_from_str("20260903-070000", "%Y%m%d-%H%M%S")
                .unwrap()
                .and_utc()
        );
    }

    /// Filenames are matched whole, so neither a task whose id is a suffix of
    /// another nor the `-farm` sidecar output can be served as this task's.
    #[test]
    fn last_output_never_matches_a_neighbouring_task() {
        let dir = TempDir::new().unwrap();
        write_output(
            dir.path(),
            "20260903-070000-thinking-loop.md",
            "thinking-loop",
            "loop output",
        );
        write_output(
            dir.path(),
            "20260903-080000-thinking-loop-farm.md",
            "thinking-loop-farm",
            "farm output",
        );

        assert_eq!(
            last_output(dir.path(), "thinking-loop").unwrap().output,
            "loop output"
        );
        assert_eq!(
            last_output(dir.path(), "thinking-loop-farm")
                .unwrap()
                .output,
            "farm output"
        );
        assert!(last_output(dir.path(), "loop").is_none());
    }

    #[test]
    fn last_output_is_none_when_nothing_is_stored() {
        let dir = TempDir::new().unwrap();
        assert!(last_output(dir.path(), "research").is_none());

        write_output(dir.path(), "20260903-070000-other.md", "other", "x");
        assert!(last_output(dir.path(), "research").is_none());

        // Garbage filenames are skipped, not parsed.
        std::fs::write(dir.path().join(TASK_OUTPUT_DIR).join("research.md"), "x").unwrap();
        std::fs::write(
            dir.path()
                .join(TASK_OUTPUT_DIR)
                .join("nope-nope-research.md"),
            "x",
        )
        .unwrap();
        assert!(last_output(dir.path(), "research").is_none());
    }

    #[test]
    fn front_matter_stripping_leaves_unmarked_content_alone() {
        assert_eq!(strip_front_matter("no front matter"), "no front matter");
        assert_eq!(strip_front_matter("---\nunterminated"), "---\nunterminated");
        assert_eq!(strip_front_matter("---\na: b\n---\n\nbody"), "body");
    }
}
