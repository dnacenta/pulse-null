//! When a probe run may happen (PN-144): the pre-registered schedule, the
//! quiet window, the measurement budget, and the conditions that refuse a
//! run outright. One pure function decides, so every refusal is testable.
//!
//! Pre-registered here, in code, so the days cannot be tuned after seeing
//! results: on `max` cadence a scheduled run is due on Monday, Wednesday and
//! Friday (owner's timezone) unless a run already finished complete or
//! partial that day; on `pro` cadence it is due once 14 days have passed
//! since the last complete or partial run. A run that was aborted or crashed
//! produced nothing usable and does not count.

use chrono::{DateTime, Datelike, Duration, NaiveTime, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::cli_provider::sandbox::LandlockSupport;
use crate::config::{ProbeCadence, ProbeConfig};

use super::record::{ChainRead, Record, Stream};

/// Days a `max`-cadence run is due.
const MAX_CADENCE_DAYS: [Weekday; 3] = [Weekday::Mon, Weekday::Wed, Weekday::Fri];

/// Days between `pro`-cadence runs.
const PRO_CADENCE_DAYS: i64 = 14;

/// How a run ended, as its finish row says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    /// Every planned turn ran.
    Complete,
    /// Stopped early at a quota error; what ran is kept and marked partial.
    Partial,
    /// Stopped before producing results (vendor login failed, setup failed).
    Aborted,
}

impl RunStatus {
    /// Does a run that ended this way count as having happened?
    #[must_use]
    pub fn produced_results(self) -> bool {
        matches!(self, Self::Complete | Self::Partial)
    }
}

/// One run as the record remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastRun {
    pub run: String,
    pub started: DateTime<Utc>,
    /// `None` when the run never wrote its finish row (it crashed).
    pub status: Option<RunStatus>,
}

/// What the record says about past runs and turns spent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    pub runs: Vec<PastRun>,
    /// When each model turn was spent.
    pub turns: Vec<DateTime<Utc>>,
    /// Record streams whose chain is broken, with the first bad line. The
    /// gate refuses while any is: rows lost to a break would let the budget
    /// reset, a pair be reused or an ask repeat (fail closed).
    pub broken: Vec<(&'static str, usize)>,
}

impl History {
    /// Read the verified rows of the record. A record that does not exist
    /// is an empty history.
    pub fn from_record(record: Option<&Record>) -> std::io::Result<Self> {
        let Some(record) = record else {
            return Ok(Self::default());
        };
        let runs = record.read(Stream::Runs)?;
        let broken = broken_streams(record, &runs)?;
        let budget = record.read(Stream::Budget)?;
        Ok(Self {
            runs: past_runs(&runs.rows),
            turns: budget
                .rows
                .iter()
                .filter_map(|r| timestamp(&r["at"]))
                .collect(),
            broken,
        })
    }
}

/// The streams whose chain is broken, with the first bad line: runs (read
/// already), budget and facts, in that order.
fn broken_streams(
    record: &Record,
    runs: &ChainRead,
) -> std::io::Result<Vec<(&'static str, usize)>> {
    let mut broken = Vec::new();
    for (stream, chain) in [
        (Stream::Runs, runs),
        (Stream::Budget, &record.read(Stream::Budget)?),
        (Stream::Facts, &record.read(Stream::Facts)?),
    ] {
        if let Some(line) = chain.broken_at {
            broken.push((stream.label(), line));
        }
    }
    Ok(broken)
}

/// The runs the run rows describe, in start order.
fn past_runs(rows: &[serde_json::Value]) -> Vec<PastRun> {
    let mut runs = Vec::new();
    for row in rows {
        apply_run_row(&mut runs, row);
    }
    runs
}

/// A start row adds its run; a finish row sets the status of the run it
/// finishes.
fn apply_run_row(runs: &mut Vec<PastRun>, row: &serde_json::Value) {
    let Some(run) = row["run"].as_str() else {
        return;
    };
    match row["event"].as_str() {
        Some("start") => {
            if let Some(started) = timestamp(&row["at"]) {
                runs.push(PastRun {
                    run: run.to_string(),
                    started,
                    status: None,
                });
            }
        }
        Some("finish") => {
            let status = serde_json::from_value::<RunStatus>(row["status"].clone()).ok();
            if let Some(past) = runs.iter_mut().find(|r| r.run == run) {
                past.status = status;
            }
        }
        _ => {}
    }
}

/// An RFC 3339 timestamp field of a record row.
pub(super) fn timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// Everything the gate decides on.
#[derive(Debug, Clone)]
pub struct GateInput<'a> {
    pub config: &'a ProbeConfig,
    pub tz: Tz,
    pub now: DateTime<Utc>,
    /// The scheduled trigger: also require the run to be due.
    pub if_due: bool,
    pub isolated: bool,
    pub landlock: LandlockSupport,
    pub history: &'a History,
    /// Model turns this run would spend.
    pub planned_turns: u32,
}

/// Why a run was refused. Nothing is written for a refused run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("probes are off ([probe] enabled = false)")]
    Disabled,
    #[error("{0}")]
    BadConfig(String),
    #[error("the pulse is in isolation (ISOLATION is set)")]
    Isolated,
    #[error("{0}")]
    NoLandlock(String),
    #[error("it is {local}, outside the quiet window {window}")]
    OutsideQuietWindow { local: String, window: String },
    #[error("not due: {0}")]
    NotDue(String),
    #[error("this run needs {planned} turns, over [probe] max_turns_per_run = {max}")]
    RunTooLarge { planned: u32, max: u32 },
    #[error("over budget: {used} turns spent this week + {planned} planned > {weekly}")]
    OverBudget {
        used: u32,
        planned: u32,
        weekly: u32,
    },
    #[error("another probe run holds the lock")]
    Locked,
    /// Another pulse's probe run held the host lock for the whole wait.
    #[error("another pulse's probe run held the host lock for the whole wait")]
    HostBusy,
    /// A record stream's chain is broken.
    #[error(
        "record/probes/{stream}.jsonl is broken at line {line}: no run until it is \
         inspected (docs/methods/probe-runner.md, \"A broken record\")"
    )]
    BrokenRecord { stream: &'static str, line: usize },
}

/// May this run happen now? (The lock is checked by the runner, after.)
pub fn check(input: &GateInput<'_>) -> Result<(), Refusal> {
    check_state(input)?;
    check_window(input)?;
    if input.if_due {
        due(input.config.cadence, input.tz, input.now, input.history).map_err(Refusal::NotDue)?;
    }
    check_turns(input)
}

/// Probes on, a valid config, an intact record, no isolation, Landlock.
fn check_state(input: &GateInput<'_>) -> Result<(), Refusal> {
    let config = input.config;
    if !config.enabled {
        return Err(Refusal::Disabled);
    }
    config.validate().map_err(Refusal::BadConfig)?;
    if let Some(&(stream, line)) = input.history.broken.first() {
        return Err(Refusal::BrokenRecord { stream, line });
    }
    if input.isolated {
        return Err(Refusal::Isolated);
    }
    input
        .landlock
        .require()
        .map_err(|e| Refusal::NoLandlock(e.to_string()))
}

/// Inside the quiet window, in the pulse's timezone.
fn check_window(input: &GateInput<'_>) -> Result<(), Refusal> {
    let config = input.config;
    let local = input.now.with_timezone(&input.tz);
    let (start, end) = config.quiet_window().map_err(Refusal::BadConfig)?;
    let minute = local.hour() * 60 + local.minute();
    // The outreach window check compares positions on a day that wraps at
    // midnight; minutes work exactly as hours do.
    if !crate::outreach::is_quiet_hour(minute, start, end) {
        return Err(Refusal::OutsideQuietWindow {
            local: local.format("%H:%M %Z").to_string(),
            window: format!("{}–{}", config.quiet_start, config.quiet_end),
        });
    }
    Ok(())
}

/// The run's turns fit the per-run cap and this week's budget.
fn check_turns(input: &GateInput<'_>) -> Result<(), Refusal> {
    let config = input.config;
    if input.planned_turns > config.max_turns_per_run {
        return Err(Refusal::RunTooLarge {
            planned: input.planned_turns,
            max: config.max_turns_per_run,
        });
    }
    let used = turns_this_week(input.history, input.tz, input.now);
    if used + input.planned_turns > config.weekly_turns {
        return Err(Refusal::OverBudget {
            used,
            planned: input.planned_turns,
            weekly: config.weekly_turns,
        });
    }
    Ok(())
}

/// Is a scheduled run due? `Err` says why not.
fn due(cadence: ProbeCadence, tz: Tz, now: DateTime<Utc>, history: &History) -> Result<(), String> {
    let local = now.with_timezone(&tz);
    let counted = history
        .runs
        .iter()
        .filter(|r| r.status.is_some_and(RunStatus::produced_results));
    match cadence {
        ProbeCadence::Max => {
            if !MAX_CADENCE_DAYS.contains(&local.weekday()) {
                return Err(format!(
                    "{} is not a probe day (Monday, Wednesday, Friday)",
                    local.format("%A")
                ));
            }
            let today = local.date_naive();
            if counted
                .into_iter()
                .any(|r| r.started.with_timezone(&tz).date_naive() == today)
            {
                return Err("a run already finished today".into());
            }
            Ok(())
        }
        ProbeCadence::Pro => {
            let last = counted.map(|r| r.started).max();
            match last {
                Some(last) if now - last < Duration::days(PRO_CADENCE_DAYS) => Err(format!(
                    "the last run was {} days ago (every {PRO_CADENCE_DAYS} days)",
                    (now - last).num_days()
                )),
                _ => Ok(()),
            }
        }
    }
}

/// Monday 00:00 of `now`'s week in `tz`.
#[must_use]
pub fn week_start(tz: Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    let local = now.with_timezone(&tz);
    let monday =
        local.date_naive() - Duration::days(i64::from(local.weekday().num_days_from_monday()));
    tz.from_local_datetime(&monday.and_time(NaiveTime::MIN))
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or(now)
}

/// Turns spent since this week's Monday.
#[must_use]
pub fn turns_this_week(history: &History, tz: Tz, now: DateTime<Utc>) -> u32 {
    let start = week_start(tz, now);
    let count = history.turns.iter().filter(|t| **t >= start).count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz() -> Tz {
        "Europe/Madrid".parse().unwrap()
    }

    /// Local Madrid time on a given day of October 2026 (CEST, UTC+2).
    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        tz().with_ymd_and_hms(2026, 10, day, hour, minute, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Every refusal's message, as the runner and `probe run` print it.
    #[test]
    fn each_refusal_says_why() {
        let cases = [
            (
                Refusal::Disabled,
                "probes are off ([probe] enabled = false)",
            ),
            (Refusal::BadConfig("bad window".into()), "bad window"),
            (
                Refusal::Isolated,
                "the pulse is in isolation (ISOLATION is set)",
            ),
            (Refusal::NoLandlock("no landlock".into()), "no landlock"),
            (
                Refusal::OutsideQuietWindow {
                    local: "12:30 CEST".into(),
                    window: "01:00–06:00".into(),
                },
                "it is 12:30 CEST, outside the quiet window 01:00–06:00",
            ),
            (
                Refusal::NotDue("a run already finished today".into()),
                "not due: a run already finished today",
            ),
            (
                Refusal::RunTooLarge {
                    planned: 40,
                    max: 30,
                },
                "this run needs 40 turns, over [probe] max_turns_per_run = 30",
            ),
            (
                Refusal::OverBudget {
                    used: 50,
                    planned: 15,
                    weekly: 60,
                },
                "over budget: 50 turns spent this week + 15 planned > 60",
            ),
            (Refusal::Locked, "another probe run holds the lock"),
            (
                Refusal::HostBusy,
                "another pulse's probe run held the host lock for the whole wait",
            ),
            (
                Refusal::BrokenRecord {
                    stream: "budget",
                    line: 7,
                },
                "record/probes/budget.jsonl is broken at line 7: no run until it is inspected \
                 (docs/methods/probe-runner.md, \"A broken record\")",
            ),
        ];
        for (refusal, message) in cases {
            assert_eq!(refusal.to_string(), message);
        }
    }

    fn enabled() -> ProbeConfig {
        ProbeConfig {
            enabled: true,
            ..ProbeConfig::default()
        }
    }

    fn input<'a>(
        config: &'a ProbeConfig,
        history: &'a History,
        now: DateTime<Utc>,
    ) -> GateInput<'a> {
        GateInput {
            config,
            tz: tz(),
            now,
            if_due: true,
            isolated: false,
            landlock: LandlockSupport::Abi(4),
            history,
            planned_turns: 3,
        }
    }

    // 2026-10-16 is a Friday, the 15th a Thursday.

    #[test]
    fn a_due_run_inside_the_window_passes() {
        let config = enabled();
        let history = History::default();
        assert_eq!(check(&input(&config, &history, at(16, 2, 30))), Ok(()));
    }

    #[test]
    fn every_refusal_has_its_reason() {
        let history = History::default();
        let off = ProbeConfig::default();
        assert_eq!(
            check(&input(&off, &history, at(16, 2, 30))),
            Err(Refusal::Disabled)
        );

        let config = enabled();
        let mut i = input(&config, &history, at(16, 2, 30));
        i.isolated = true;
        assert_eq!(check(&i), Err(Refusal::Isolated));

        let mut i = input(&config, &history, at(16, 2, 30));
        i.landlock = LandlockSupport::Abi(3);
        assert!(matches!(check(&i), Err(Refusal::NoLandlock(_))));
        i.landlock = LandlockSupport::NotImplemented;
        assert!(matches!(check(&i), Err(Refusal::NoLandlock(_))));

        let i = input(&config, &history, at(16, 12, 0));
        assert!(matches!(check(&i), Err(Refusal::OutsideQuietWindow { .. })));

        let i = input(&config, &history, at(15, 2, 30));
        assert!(matches!(check(&i), Err(Refusal::NotDue(_))), "Thursday");

        let mut i = input(&config, &history, at(15, 2, 30));
        i.if_due = false;
        assert_eq!(check(&i), Ok(()), "a run by hand needs no probe day");

        let mut i = input(&config, &history, at(16, 2, 30));
        i.planned_turns = 28;
        assert_eq!(
            check(&i),
            Err(Refusal::RunTooLarge {
                planned: 28,
                max: 27
            })
        );
    }

    #[test]
    fn the_window_can_wrap_midnight() {
        let config = ProbeConfig {
            quiet_start: "23:00".into(),
            quiet_end: "05:00".into(),
            ..enabled()
        };
        let history = History::default();
        let mut i = input(&config, &history, at(16, 23, 30));
        i.if_due = false;
        assert_eq!(check(&i), Ok(()));
        i.now = at(16, 4, 59);
        assert_eq!(check(&i), Ok(()));
        i.now = at(16, 5, 0);
        assert!(matches!(check(&i), Err(Refusal::OutsideQuietWindow { .. })));
    }

    #[test]
    fn a_second_scheduled_run_the_same_day_is_refused() {
        let config = enabled();
        let mut history = History {
            runs: vec![PastRun {
                run: "r1".into(),
                started: at(16, 2, 5),
                status: Some(RunStatus::Complete),
            }],
            turns: vec![],
            broken: Vec::new(),
        };
        assert!(matches!(
            check(&input(&config, &history, at(16, 3, 0))),
            Err(Refusal::NotDue(_))
        ));
        // An aborted or crashed run does not count.
        history.runs[0].status = Some(RunStatus::Aborted);
        assert_eq!(check(&input(&config, &history, at(16, 3, 0))), Ok(()));
        history.runs[0].status = None;
        assert_eq!(check(&input(&config, &history, at(16, 3, 0))), Ok(()));
    }

    #[test]
    fn pro_cadence_waits_fourteen_days() {
        let config = ProbeConfig {
            cadence: ProbeCadence::Pro,
            ..enabled()
        };
        let history = History {
            runs: vec![PastRun {
                run: "r1".into(),
                started: at(5, 2, 30),
                status: Some(RunStatus::Partial),
            }],
            turns: vec![],
            broken: Vec::new(),
        };
        assert!(matches!(
            check(&input(&config, &history, at(16, 2, 30))),
            Err(Refusal::NotDue(_))
        ));
        assert_eq!(check(&input(&config, &history, at(19, 2, 30))), Ok(()));
    }

    #[test]
    fn the_weekly_budget_counts_turns_since_monday() {
        let config = ProbeConfig {
            weekly_turns: 10,
            max_turns_per_run: 5,
            ..enabled()
        };
        // Monday the 12th to Friday the 16th: 8 turns this week, 5 last week.
        let mut turns: Vec<DateTime<Utc>> = (0..8).map(|i| at(12 + i % 4, 2, 10)).collect();
        turns.extend((0..5).map(|_| at(9, 2, 10)));
        let history = History {
            runs: vec![],
            turns,
            broken: Vec::new(),
        };
        assert_eq!(turns_this_week(&history, tz(), at(16, 2, 30)), 8);
        assert_eq!(
            check(&input(&config, &history, at(16, 2, 30))),
            Err(Refusal::OverBudget {
                used: 8,
                planned: 3,
                weekly: 10
            })
        );
        assert_eq!(week_start(tz(), at(16, 2, 30)), at(12, 0, 0));
    }

    /// Audit R2-8: a broken budget, runs or facts chain refuses every run.
    #[test]
    fn a_broken_chain_refuses_the_run() {
        let config = enabled();
        let history = History {
            broken: vec![("budget", 4)],
            ..History::default()
        };
        assert_eq!(
            check(&input(&config, &history, at(16, 2, 30))),
            Err(Refusal::BrokenRecord {
                stream: "budget",
                line: 4
            })
        );
    }
}
