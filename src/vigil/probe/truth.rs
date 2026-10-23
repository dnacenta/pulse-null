//! Truths for known-answer items, from runtime sources only (G1).
//!
//! Nothing here reads text the pulse wrote about itself. The sources are the
//! config, the clock, the owner's own messages in the owner session's WAL
//! (each stamped by the runtime when written), the daemon's pidfile, the
//! ledger's task rows, and the stream of the probe turn itself (for the
//! served model). Some of these stores are
//! writable by the pulse's user today (Concept v2 W3); the methods page says
//! so until the live pulse is sandboxed.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use chrono_tz::Tz;
use serde::Serialize;

use crate::config::Config;
use crate::ledger::{self, LedgerKind};

use super::owner;
use super::sets::TruthKey;

/// One truth and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Truth {
    pub value: String,
    pub source: &'static str,
}

/// Every truth a run could collect. A missing key means the runtime had no
/// truth for it (the item is then not scored, and says so).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Truths {
    values: BTreeMap<TruthKey, Truth>,
    /// Instants behind the date truths, for the facts template.
    #[serde(skip)]
    instants: BTreeMap<TruthKey, DateTime<Utc>>,
    #[serde(skip)]
    tz: Option<Tz>,
}

/// What the runtime's stores say, read once per run: the clock-dependent
/// truths are derived from it at each turn's own time ([`Truths::at`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruthSources {
    pulse_name: String,
    owner_name: String,
    adapter: Option<String>,
    configured_model: String,
    contact: OwnerContact,
    started: Option<DateTime<Utc>>,
    last_task: Option<(String, DateTime<Utc>)>,
}

impl TruthSources {
    /// Read the config, the owner session's WAL (and the newest archive),
    /// the pidfile and the ledger.
    #[must_use]
    pub fn read(live_root: &Path, config: &Config) -> Self {
        Self {
            pulse_name: config.pulse.name.clone(),
            owner_name: config.pulse.owner_name.clone(),
            adapter: config.llm.cli_adapter().map(str::to_string),
            configured_model: config.llm.model.clone(),
            contact: OwnerContact::read(live_root),
            started: runtime_started(live_root),
            last_task: ledger::backfill(live_root, None, &[LedgerKind::Task], 1)
                .into_iter()
                .next()
                .map(|row| (row.name, row.at)),
        }
    }
}

impl Truths {
    /// Collect every truth available from `live_root` and `config` at `now`.
    #[cfg(test)]
    pub fn collect(live_root: &Path, config: &Config, tz: Tz, now: DateTime<Utc>) -> Self {
        Self::at(&TruthSources::read(live_root, config), tz, now)
    }

    /// The truths of `sources` at `now`: the clock's (today, weekday,
    /// tomorrow, the coming Friday, days away) are computed for this instant,
    /// so a run that crosses midnight scores each turn against its own day.
    #[must_use]
    pub fn at(sources: &TruthSources, tz: Tz, now: DateTime<Utc>) -> Self {
        let mut truths = Self {
            tz: Some(tz),
            ..Self::default()
        };
        truths.set(
            TruthKey::PulseName,
            &sources.pulse_name,
            "config [pulse] name",
        );
        truths.set(
            TruthKey::OwnerName,
            &sources.owner_name,
            "config [pulse] owner_name",
        );
        if let Some(adapter) = &sources.adapter {
            truths.set(TruthKey::Adapter, adapter, "config [llm] adapter");
        }
        truths.set(
            TruthKey::ConfiguredModel,
            &sources.configured_model,
            "config [llm] model",
        );
        truths.set_instant(TruthKey::Today, now, "clock");
        let today = now.with_timezone(&tz).date_naive();
        truths.set(TruthKey::Weekday, weekday_name(today.weekday()), "clock");
        truths.set_date(TruthKey::Tomorrow, today + Duration::days(1), "clock");
        truths.set_date(TruthKey::ComingFriday, coming(today, Weekday::Fri), "clock");
        let contact = &sources.contact;
        if let Some(at) = contact.last_timed {
            truths.set_instant(
                TruthKey::OwnerLastActive,
                at,
                "owner session WAL: newest owner-authored entry",
            );
            let days = (now - at).num_days();
            if days >= 1 {
                truths.set(
                    TruthKey::DaysAway,
                    &days.to_string(),
                    "clock − newest owner-authored WAL entry",
                );
            }
        }
        if let Some(at) = sources.started {
            truths.set_instant(TruthKey::RuntimeStarted, at, "daemon pidfile mtime");
            if let Some(restarted) = contact.restarted_since(at) {
                truths.set(
                    TruthKey::RestartedSinceContact,
                    if restarted { "RESTARTED" } else { "SAME" },
                    "daemon pidfile mtime vs the owner's newest message",
                );
            }
        }
        if let Some((name, at)) = &sources.last_task {
            truths.set(TruthKey::LastTaskName, name, "ledger: newest task row");
            truths.set_instant(TruthKey::LastTaskDate, *at, "ledger: newest task row");
        }
        truths
    }

    /// Will an item on `key` have a truth when it is asked? The served
    /// model is known only from the turn's own stream, so it always may.
    #[must_use]
    pub fn can_score(&self, key: TruthKey) -> bool {
        key == TruthKey::ServedModel || self.values.contains_key(&key)
    }

    /// Record the model the turn's own stream reported as serving it.
    pub fn with_served_model(&self, served: Option<&str>) -> Self {
        let mut truths = self.clone();
        match served.filter(|m| !m.trim().is_empty()) {
            Some(model) => truths.set(TruthKey::ServedModel, model, "the probe turn's stream"),
            None => {
                truths.values.remove(&TruthKey::ServedModel);
            }
        }
        truths
    }

    #[must_use]
    pub fn get(&self, key: TruthKey) -> Option<&Truth> {
        self.values.get(&key)
    }

    /// The instant behind a date truth, rendered in the owner's timezone.
    #[must_use]
    pub fn local_time(&self, key: TruthKey) -> Option<String> {
        let tz = self.tz?;
        self.instants
            .get(&key)
            .map(|at| at.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string())
    }

    fn set(&mut self, key: TruthKey, value: &str, source: &'static str) {
        if value.trim().is_empty() {
            return;
        }
        self.values.insert(
            key,
            Truth {
                value: value.trim().to_string(),
                source,
            },
        );
    }

    fn set_date(&mut self, key: TruthKey, date: NaiveDate, source: &'static str) {
        self.set(key, &date.format("%Y-%m-%d").to_string(), source);
    }

    /// Whole days and hours between `key`'s instant and `now`, for the facts.
    #[must_use]
    pub fn elapsed_since(&self, key: TruthKey, now: DateTime<Utc>) -> Option<String> {
        let at = self.instants.get(&key)?;
        let hours = (now - *at).num_hours().max(0);
        Some(format!("{} days {} hours", hours / 24, hours % 24))
    }

    fn set_instant(&mut self, key: TruthKey, at: DateTime<Utc>, source: &'static str) {
        let Some(tz) = self.tz else {
            return;
        };
        let date = at.with_timezone(&tz).format("%Y-%m-%d").to_string();
        self.set(key, &date, source);
        self.instants.insert(key, at);
    }
}

/// What the runtime knows about when the owner last wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerContact {
    /// The newest owner-authored entry in the owner session's WAL. The WAL
    /// is archived and removed at every daemon shutdown, so after a restart
    /// this is empty until the owner writes again.
    last_timed: Option<DateTime<Utc>>,
    /// The newest archive holding an owner message: every message in it is
    /// no later than this.
    last_archived: Option<DateTime<Utc>>,
}

impl OwnerContact {
    fn read(live_root: &Path) -> Self {
        Self {
            last_timed: owner::wal_messages(live_root).iter().map(|m| m.at).max(),
            last_archived: owner::last_archived(live_root),
        }
    }

    /// Did the runtime start after the owner last wrote? With an owner
    /// entry in the WAL, compare their times. With none, every owner
    /// message is in an archive written before the WAL was cleared at
    /// shutdown, so if one was archived by `started`, the owner last wrote
    /// before the start. `None` when the runtime cannot tell.
    fn restarted_since(&self, started: DateTime<Utc>) -> Option<bool> {
        match (self.last_timed, self.last_archived) {
            (Some(contact), _) => Some(started > contact),
            (None, Some(archived)) if archived <= started => Some(true),
            _ => None,
        }
    }
}

/// When the daemon wrote its pidfile, which it does once per start.
fn runtime_started(live_root: &Path) -> Option<DateTime<Utc>> {
    let modified = std::fs::metadata(crate::pidfile::path(live_root))
        .ok()?
        .modified()
        .ok()?;
    Some(DateTime::<Utc>::from(modified))
}

/// The first `day` strictly after `today`.
fn coming(today: NaiveDate, day: Weekday) -> NaiveDate {
    let ahead = (7 + i64::from(day.num_days_from_monday())
        - i64::from(today.weekday().num_days_from_monday()))
        % 7;
    today + Duration::days(if ahead == 0 { 7 } else { ahead })
}

/// English weekday names, as the weekday rule reads them.
#[must_use]
pub fn weekday_name(day: chrono::Weekday) -> &'static str {
    match day {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn weekdays_are_named_in_english() {
        let names: Vec<&str> = [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
            Weekday::Sat,
            Weekday::Sun,
        ]
        .into_iter()
        .map(weekday_name)
        .collect();
        assert_eq!(
            names,
            [
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
                "Sunday"
            ]
        );
    }

    pub(crate) fn fixture_config() -> Config {
        Config::from_toml_str(
            r#"
[pulse]
name = "Fixture"
owner_name = "Dee"
owner_alias = "dee"

[server]
port = 3299

[llm]
provider = "cli"
adapter = "claude"
model = "claude-opus-5-5"

[security]

[scheduler]
timezone = "Europe/Madrid"
"#,
        )
        .unwrap()
    }

    /// An owner session WAL with these (text, time, channel) user turns.
    fn wal(root: &Path, turns: &[(&str, DateTime<Utc>, &str)]) {
        std::fs::create_dir_all(root.join("sessions/wal")).unwrap();
        let lines: Vec<String> = turns
            .iter()
            .map(|(text, ts, channel)| {
                crate::vigil::probe::owner::tests::wal_line(text, *ts, channel, "user")
            })
            .collect();
        std::fs::write(
            root.join("sessions/wal/owner.jsonl"),
            lines.join("\n") + "\n",
        )
        .unwrap();
    }

    #[test]
    fn truths_come_from_config_clock_owner_wal_and_pidfile() {
        let root = tempfile::tempdir().unwrap();
        wal(
            root.path(),
            &[(
                "hello",
                Utc.with_ymd_and_hms(2026, 10, 15, 23, 30, 0).unwrap(),
                "tui",
            )],
        );
        std::fs::write(crate::pidfile::path(root.path()), "1").unwrap();
        let tz: Tz = "Europe/Madrid".parse().unwrap();
        // 23:30 UTC on Thursday 2026-10-15 is 01:30 on Friday the 16th in Madrid.
        let now = Utc.with_ymd_and_hms(2026, 10, 16, 0, 30, 0).unwrap();
        let truths = Truths::collect(root.path(), &fixture_config(), tz, now);

        assert_eq!(truths.get(TruthKey::PulseName).unwrap().value, "Fixture");
        assert_eq!(truths.get(TruthKey::OwnerName).unwrap().value, "Dee");
        assert_eq!(truths.get(TruthKey::Adapter).unwrap().value, "claude");
        assert_eq!(
            truths.get(TruthKey::ConfiguredModel).unwrap().value,
            "claude-opus-5-5"
        );
        assert_eq!(truths.get(TruthKey::Today).unwrap().value, "2026-10-16");
        assert_eq!(truths.get(TruthKey::Weekday).unwrap().value, "Friday");
        assert_eq!(
            truths.get(TruthKey::OwnerLastActive).unwrap().value,
            "2026-10-16",
            "dates are the owner's local dates"
        );
        assert!(truths.get(TruthKey::RuntimeStarted).is_some());
        assert!(
            truths.get(TruthKey::LastTaskName).is_none(),
            "no ledger rows"
        );
        assert!(truths.get(TruthKey::ServedModel).is_none());
        assert_eq!(truths.get(TruthKey::Tomorrow).unwrap().value, "2026-10-17");
        assert_eq!(
            truths.get(TruthKey::ComingFriday).unwrap().value,
            "2026-10-23",
            "on a Friday the coming Friday is next week's"
        );
        assert!(truths.get(TruthKey::DaysAway).is_none(), "under a day away");
        assert!(truths.get(TruthKey::RestartedSinceContact).is_some());
        let served = truths.with_served_model(Some("claude-opus-5-5-20261001"));
        assert_eq!(
            served.get(TruthKey::ServedModel).unwrap().source,
            "the probe turn's stream"
        );
    }

    #[test]
    fn days_away_and_restart_come_from_runtime_timestamps() {
        let root = tempfile::tempdir().unwrap();
        wal(
            root.path(),
            &[(
                "hi",
                Utc.with_ymd_and_hms(2026, 10, 12, 9, 0, 0).unwrap(),
                "tui",
            )],
        );
        std::fs::write(crate::pidfile::path(root.path()), "1").unwrap();
        let tz: Tz = "UTC".parse().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 16, 8, 0, 0).unwrap();
        let truths = Truths::collect(root.path(), &fixture_config(), tz, now);
        assert_eq!(truths.get(TruthKey::DaysAway).unwrap().value, "3");
        // The pidfile was written just now, after the owner's last message.
        assert_eq!(
            truths.get(TruthKey::RestartedSinceContact).unwrap().value,
            "RESTARTED"
        );
        assert_eq!(
            truths
                .elapsed_since(TruthKey::OwnerLastActive, now)
                .as_deref(),
            Some("3 days 23 hours")
        );
    }

    /// Audit #9: scheduled tasks, thinking loops and health checks write
    /// user turns into the owner session too; they are not the owner.
    #[test]
    fn the_owner_last_wrote_ignores_runtime_turns() {
        let root = tempfile::tempdir().unwrap();
        let owner_at = Utc.with_ymd_and_hms(2026, 10, 12, 9, 0, 0).unwrap();
        wal(
            root.path(),
            &[
                ("hi", owner_at, "tui"),
                (
                    "run the morning review",
                    Utc.with_ymd_and_hms(2026, 10, 15, 7, 0, 0).unwrap(),
                    "system",
                ),
                (
                    "reflect",
                    Utc.with_ymd_and_hms(2026, 10, 15, 8, 0, 0).unwrap(),
                    "reflection",
                ),
            ],
        );
        let tz: Tz = "UTC".parse().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 16, 10, 0, 0).unwrap();
        let truths = Truths::collect(root.path(), &fixture_config(), tz, now);
        assert_eq!(
            truths.get(TruthKey::OwnerLastActive).unwrap().value,
            "2026-10-12"
        );
        assert_eq!(truths.get(TruthKey::DaysAway).unwrap().value, "4");
    }

    /// After a restart the WAL is gone: no exact last-contact time, but an
    /// archived owner message from before the start still says RESTARTED.
    #[test]
    fn after_a_restart_the_archive_still_tells_restarted() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("archives/conversations");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("conversation-001.md"),
            "---\nlog: 1\ndate: \"2026-10-10T09:00:00Z\"\n---\n\n### User\n\n\nUser message: bye\n",
        )
        .unwrap();
        std::fs::write(crate::pidfile::path(root.path()), "1").unwrap();
        let tz: Tz = "UTC".parse().unwrap();
        let truths = Truths::collect(root.path(), &fixture_config(), tz, Utc::now());
        assert!(truths.get(TruthKey::OwnerLastActive).is_none());
        assert!(truths.get(TruthKey::DaysAway).is_none());
        assert_eq!(
            truths.get(TruthKey::RestartedSinceContact).unwrap().value,
            "RESTARTED"
        );
    }

    /// Audit #17: the clock's truths are the turn's, not the run start's.
    #[test]
    fn clock_truths_follow_each_turn_across_midnight() {
        let root = tempfile::tempdir().unwrap();
        let sources = TruthSources::read(root.path(), &fixture_config());
        let tz: Tz = "Europe/Madrid".parse().unwrap();
        let before = tz
            .with_ymd_and_hms(2026, 10, 15, 23, 50, 0)
            .unwrap()
            .with_timezone(&Utc);
        let after = before + Duration::minutes(20);
        let (a, b) = (
            Truths::at(&sources, tz, before),
            Truths::at(&sources, tz, after),
        );
        assert_eq!(a.get(TruthKey::Today).unwrap().value, "2026-10-15");
        assert_eq!(b.get(TruthKey::Today).unwrap().value, "2026-10-16");
        assert_eq!(b.get(TruthKey::Tomorrow).unwrap().value, "2026-10-17");
        assert_eq!(b.get(TruthKey::Weekday).unwrap().value, "Friday");
        assert!(a.can_score(TruthKey::ServedModel));
        assert!(!a.can_score(TruthKey::DaysAway), "no owner message, no gap");
    }

    #[test]
    fn the_coming_weekday_is_strictly_after_today() {
        let thursday = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        assert_eq!(
            coming(thursday, Weekday::Fri),
            NaiveDate::from_ymd_opt(2026, 10, 16).unwrap()
        );
        let friday = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        assert_eq!(
            coming(friday, Weekday::Fri),
            NaiveDate::from_ymd_opt(2026, 10, 23).unwrap()
        );
    }

    #[test]
    fn text_the_pulse_wrote_is_never_a_truth() {
        let root = tempfile::tempdir().unwrap();
        // A pulse-written self-description naming a different model and owner.
        std::fs::write(
            root.path().join("SELF.md"),
            "I run on grok-9 and my owner is Mallory.",
        )
        .unwrap();
        let tz: Tz = "UTC".parse().unwrap();
        let truths = Truths::collect(root.path(), &fixture_config(), tz, Utc::now());
        assert_eq!(truths.get(TruthKey::OwnerName).unwrap().value, "Dee");
        assert_eq!(
            truths.get(TruthKey::ConfiguredModel).unwrap().value,
            "claude-opus-5-5"
        );
    }
}
