//! Runner tests against a fixture pulse and a fake CLI. Every turn runs
//! under the real Landlock sandbox, so these skip on a kernel without it.

use std::path::{Path, PathBuf};

use chrono::TimeZone;
use serde_json::Value;

use super::*;
use crate::cli_provider::sandbox::landlock_or_skip;
use crate::vigil::probe::record::Stream;
use crate::vigil::probe::scratch::tests::fixture_pulse;
use crate::vigil::probe::scratch::tree_digest;
use crate::vigil::probe::truth::tests::fixture_config;

/// What the fake CLI does for one arm.
pub(crate) enum Act {
    Answer(&'static str),
    /// Answer, after a tool call that reads the held-out record.
    PeekAtRecord(&'static str),
    Quota,
    AuthFailure,
}

/// A fake claude CLI: answers the capability probe silently, then picks its
/// arm from the system prompt it was handed and replays a canned stream.
pub(crate) struct FakeCli {
    dir: tempfile::TempDir,
}

impl FakeCli {
    pub(crate) fn new(pulse: Act, input_only: Act, fresh: Act) -> Self {
        Self::with_needles(pulse, input_only, fresh, &[])
    }

    /// As [`new`](Self::new), but a prompt containing a needle gets that
    /// needle's reply, whatever the arm.
    fn with_needles(pulse: Act, input_only: Act, fresh: Act, needles: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for (arm, act) in [("pulse", pulse), ("input", input_only), ("fresh", fresh)] {
            let (stdout, code) = match act {
                Act::Answer(reply) => (
                    [
                        r#"{"type":"system","subtype":"init","model":"claude-opus-5-5"}"#.to_string(),
                        serde_json::json!({"type":"assistant","message":{"model":"claude-opus-5-5","content":[{"type":"text","text":reply}]}}).to_string(),
                        serde_json::json!({"type":"result","result":reply,"is_error":false,"usage":{"input_tokens":1000,"output_tokens":40}}).to_string(),
                    ]
                    .join("\n"),
                    0,
                ),
                Act::PeekAtRecord(reply) => (
                    [
                        r#"{"type":"system","subtype":"init","model":"claude-opus-5-5"}"#.to_string(),
                        r#"{"type":"assistant","message":{"model":"claude-opus-5-5","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/home/pulse/pulse-null/echo/record/probes/items.jsonl"}}]}}"#.to_string(),
                        serde_json::json!({"type":"result","result":reply,"is_error":false,"usage":{"input_tokens":1000,"output_tokens":40}}).to_string(),
                    ]
                    .join("\n"),
                    0,
                ),
                Act::Quota => (
                    r#"{"type":"result","is_error":true,"result":"You've hit your limit · resets 6am"}"#.to_string(),
                    1,
                ),
                Act::AuthFailure => (String::new(), 1),
            };
            std::fs::write(dir.path().join(format!("{arm}.out")), stdout).unwrap();
            std::fs::write(dir.path().join(format!("{arm}.code")), code.to_string()).unwrap();
        }
        let d = dir.path().display();
        let mut by_needle = String::new();
        for (index, (needle, reply)) in needles.iter().enumerate() {
            // A reply of QUOTA answers with a spent quota instead; FAIL
            // fails the turn as a lost login does (nothing out, exit 1).
            if *reply == FAIL {
                by_needle.push_str(&format!(
                    "if printf '%s' \"$prompt\" | grep -qF '{needle}'; then \
                     echo 'Invalid API key · Please run /login' >&2; exit 1; fi\n"
                ));
                continue;
            }
            let (stream, code) = if *reply == QUOTA {
                (
                    r#"{"type":"result","is_error":true,"result":"You've hit your limit · resets 6am"}"#
                        .to_string(),
                    1,
                )
            } else {
                (
                    [
                        r#"{"type":"system","subtype":"init","model":"claude-opus-5-5"}"#.to_string(),
                        serde_json::json!({"type":"result","result":reply,"is_error":false,"usage":{"input_tokens":500,"output_tokens":5}}).to_string(),
                    ]
                    .join("\n"),
                    0,
                )
            };
            std::fs::write(dir.path().join(format!("needle{index}.out")), stream).unwrap();
            by_needle.push_str(&format!(
                "if printf '%s' \"$prompt\" | grep -qF '{needle}'; then cat '{d}/needle{index}.out'; exit {code}; fi\n"
            ));
        }
        let script = format!(
            r#"#!/bin/bash
case " $* " in *" --output-format "*) ;; *) exit 0 ;; esac
sp=""; prev=""
for a in "$@"; do [ "$prev" = "--system-prompt-file" ] && sp="$a"; prev="$a"; done
prompt=$(cat)
{by_needle}arm=fresh
if [ -n "$sp" ]; then
  if grep -q '<situation>' "$sp"; then arm=input
  elif grep -qF 'You are a helpful assistant.' "$sp"; then arm=fresh
  else arm=pulse; fi
fi
code=$(cat '{d}/'$arm.code)
[ "$code" != 0 ] && [ ! -s '{d}/'$arm.out ] && echo "Invalid API key · Please run /login" >&2
cat '{d}/'$arm.out
exit $code
"#
        );
        let bin = dir.path().join("fake-claude");
        // A root-owned install in production; a temp dir here.
        crate::vigil::probe::fence::tests::trust_fixture_install(dir.path());
        std::fs::write(&bin, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    pub(crate) fn bin(&self) -> PathBuf {
        self.dir.path().join("fake-claude")
    }
}

/// A needle reply that makes the fake CLI report a spent quota.
const QUOTA: &str = "QUOTA";

/// A needle reply that makes the fake CLI fail the turn (not a quota).
const FAIL: &str = "FAIL";

const PULSE_REPLY: &str = "1: Fixture\n2: Dee\n3: claude\n4: claude-opus-5-5\n5: Claude Opus 5.5\n\
                           6: 2026-10-16\n7: Friday\n8: UNKNOWN\n9: UNKNOWN\n10: UNKNOWN\n11: UNKNOWN";
const FRESH_REPLY: &str = "1: Claude\n2: UNKNOWN\n3: claude\n4: UNKNOWN\n5: claude\n6: 2025-01-01";

struct Harness {
    live: tempfile::TempDir,
    home: tempfile::TempDir,
    state: tempfile::TempDir,
    config: Config,
    cli: FakeCli,
    registry: crate::vigil::probe::sealed::Registry,
    /// The sealed sets as the unit's credentials directory holds them.
    credentials: tempfile::TempDir,
    /// Run as if outside the unit: no credentials directory.
    without_credentials: bool,
    /// The record as the harness left it: the sealed sets and nothing else.
    pristine: String,
    confinement: &'static (dyn Fn() -> Result<(), String> + Sync),
    /// The run's stop flag (a test may set it, as a signal would).
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    time_budget: std::time::Duration,
}

/// Stands in for the probe unit's confinement.
fn confined() -> Result<(), String> {
    Ok(())
}

impl Harness {
    fn new(cli: FakeCli) -> Self {
        let mut config = fixture_config();
        config.probe.enabled = true;
        config.llm.cli_bin = Some(cli.bin().display().to_string());
        // The fixture carries an isolation marker and a record to prove the
        // copy leaves them behind; a pulse being probed has neither.
        let live = fixture_pulse();
        std::fs::remove_file(live.path().join("ISOLATION")).unwrap();
        std::fs::remove_dir_all(live.path().join("record")).unwrap();
        // The record exists, as it does once any probe command ran.
        Record::open(live.path()).unwrap();
        let credentials = tempfile::tempdir().unwrap();
        let registry = crate::vigil::probe::sealed::tests::fixture_credentials(credentials.path());
        let mut harness = Self {
            live,
            home: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
            config,
            cli,
            registry,
            credentials,
            without_credentials: false,
            pristine: String::new(),
            confinement: &confined,
            stop: std::sync::Arc::default(),
            time_budget: std::time::Duration::from_secs(3600),
        };
        harness.pristine = harness.record_digest();
        harness
    }

    fn root(&self) -> PathBuf {
        self.live.path().canonicalize().unwrap()
    }

    /// Friday 2026-10-16, 02:30 in Madrid: a probe day, inside the window.
    fn friday_night() -> DateTime<Utc> {
        Tz::Europe__Madrid
            .with_ymd_and_hms(2026, 10, 16, 2, 30, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    async fn run_at(&self, now: DateTime<Utc>, request: &RunRequest) -> RunOutcome {
        self.run_with_host_wait(now, request, std::time::Duration::ZERO)
            .await
    }

    async fn run_with_host_wait(
        &self,
        now: DateTime<Utc>,
        request: &RunRequest,
        host_lock_wait: std::time::Duration,
    ) -> RunOutcome {
        let root = self.root();
        let has_env = |_: &str| true;
        let deny = DenyList::live(&root, self.home.path(), None, None);
        let env = RunEnv {
            config: &self.config,
            live_root: &root,
            home: self.home.path(),
            scratch_parent: &self.state.path().join("s"),
            tz: Tz::Europe__Madrid,
            now,
            landlock: LandlockSupport::detect(),
            recall_bin: "/usr/local/bin/recall-echo",
            tcp_connect: &[443],
            deny: &deny,
            live_graph: None,
            has_env: &has_env,
            registry: &self.registry,
            credentials: (!self.without_credentials).then(|| self.credentials.path()),
            host_lock_wait,
            // The suite is not under the probe unit; the check itself is
            // tested in `confine`, and its refusal below.
            confinement: self.confinement,
            stop: &self.stop,
            time_budget: self.time_budget,
            memory: "recall-echo fixture · graph embedded",
        };
        let _ = &self.cli;
        run(&env, request).await.unwrap()
    }

    fn rows(&self, stream: Stream) -> Vec<Value> {
        Record::existing(&self.root())
            .map(|r| r.read(stream).unwrap().rows)
            .unwrap_or_default()
    }

    /// The record's rows and receipts (the installed sets and the lock left
    /// out).
    fn record_digest(&self) -> String {
        tree_digest(&self.root().join("record/probes"), &["run.lock", "sets"])
    }
}

fn scheduled() -> RunRequest {
    RunRequest {
        if_due: true,
        ..RunRequest::default()
    }
}

#[tokio::test]
async fn a_full_run_records_every_arm_and_writes_nothing_live() {
    if !landlock_or_skip("a_full_run_records_every_arm_and_writes_nothing_live") {
        return;
    }
    let h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    let root = h.root();
    // The owner last wrote three days ago, so every family has a truth.
    std::fs::create_dir_all(root.join("sessions/wal")).unwrap();
    std::fs::write(
        root.join("sessions/wal/owner.jsonl"),
        crate::vigil::probe::owner::tests::wal_line(
            "see you",
            Harness::friday_night() - chrono::Duration::days(3),
            "tui",
            "user",
        ) + "\n",
    )
    .unwrap();
    // Live vendor dirs in the user's home, so the digest covers them too.
    std::fs::create_dir_all(h.home.path().join(".claude/projects")).unwrap();
    std::fs::write(h.home.path().join(".claude/settings.json"), "{}").unwrap();
    std::fs::create_dir_all(h.home.path().join(".grok/sessions")).unwrap();
    let before = (
        tree_digest(&root, &["record"]),
        tree_digest(h.home.path(), &[]),
    );

    let outcome = h.run_at(Harness::friday_night(), &scheduled()).await;
    let RunOutcome::Finished(summary) = outcome else {
        panic!("the run should go ahead: {outcome:?}");
    };
    assert_eq!(summary.status, RunStatus::Complete, "{summary:?}");
    // known-answer: 3 batched turns (11 items each); situation-use: 4 drawn
    // items × 3 arms, one turn each.
    assert_eq!(summary.turns, 15);
    assert_eq!(summary.items_recorded, 33 + 12);
    assert_eq!(summary.input_tokens, 15_000);
    assert!(summary.alerts.is_empty(), "{:?}", summary.alerts);

    let runs = h.rows(Stream::Runs);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["event"], "start");
    assert_eq!(
        runs[0]["baseline"],
        crate::vigil::probe::baseline(&h.config).unwrap()
    );
    assert_eq!(runs[1]["status"], "complete");
    assert_eq!(h.rows(Stream::Budget).len(), 15);

    let items = h.rows(Stream::Items);
    assert_eq!(items.len(), 45);
    let situation: Vec<&Value> = items
        .iter()
        .filter(|r| r["set"] == "situation-use")
        .collect();
    assert_eq!(situation.len(), 12);
    let families: std::collections::BTreeSet<&str> = situation
        .iter()
        .filter_map(|r| r["family"].as_str())
        .collect();
    assert_eq!(families.len(), 4, "one item per family");
    for arm in ["pulse", "input-only", "fresh"] {
        assert_eq!(
            situation.iter().filter(|r| r["arm"] == arm).count(),
            4,
            "{arm}"
        );
    }
    let outcome = |arm: &str, item: &str| -> String {
        items
            .iter()
            .find(|r| r["arm"] == arm && r["item"] == item)
            .map(|r| r["outcome"].as_str().unwrap().to_string())
            .unwrap()
    };
    assert_eq!(outcome("pulse", "name"), "correct");
    assert_eq!(outcome("pulse", "served-model"), "correct");
    assert_eq!(outcome("pulse", "weekday"), "correct");
    assert_eq!(
        outcome("pulse", "owner-last-wrote"),
        "unknown",
        "the WAL is a truth; the pulse said UNKNOWN"
    );
    assert_eq!(
        outcome("pulse", "runtime-started"),
        "unknown",
        "the pidfile is a truth; the pulse said UNKNOWN"
    );
    assert_eq!(outcome("fresh", "name"), "wrong");
    assert_eq!(outcome("fresh", "today"), "wrong");
    assert_eq!(outcome("fresh", "weekday"), "unanswered");
    for row in &items {
        for field in [
            "adapter",
            "configured_model",
            "served_model",
            "binary",
            "commit",
            "cli_version",
            "memory",
            "baseline",
            "prompt_sha256",
            "set_sha256",
            "file_sha256",
            "facts_sha256",
            "disclosure_sha256",
        ] {
            assert!(row[field].is_string(), "{field} on {row}");
        }
        assert_eq!(row["scorer_version"], score::SCORER_VERSION, "{row}");
        assert!(row.get("reasoning_effort").is_some(), "{row}");
    }
    let fresh_prompt = items.iter().find(|r| r["arm"] == "fresh").unwrap()["prompt_sha256"].clone();
    assert_eq!(
        fresh_prompt,
        crate::vigil::probe::sets::fresh_sha256(),
        "the fresh arm gets the neutral line, through the same flag"
    );

    let receipts = root.join("record/probes/receipts").join(&summary.run);
    assert_eq!(std::fs::read_dir(&receipts).unwrap().count(), 15);
    let receipt: Value = serde_json::from_str(
        &std::fs::read_to_string(receipts.join("known-answer-pulse.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["served_model"], "claude-opus-5-5");
    // R2-9: the receipt says what the pulse's prompt was made of, and the
    // general disclosure is one of its parts.
    let included: Vec<&str> = receipt["system_prompt_components"]["included"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(included.contains(&"measurement-disclosure"), "{included:?}");
    assert!(included.contains(&"SELF.md"), "{included:?}");
    assert!(receipt["raw"]
        .as_str()
        .unwrap()
        .contains("\"type\":\"result\""));

    assert_eq!(
        (
            tree_digest(&root, &["record"]),
            tree_digest(h.home.path(), &[]),
        ),
        before,
        "the probe wrote nothing to the live pulse home or the user's home"
    );
    let leftovers = std::fs::read_dir(h.state.path().join("s"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .count();
    assert_eq!(
        leftovers, 0,
        "the scratch is removed (the host lock file stays)"
    );
}

#[tokio::test]
async fn refused_runs_write_nothing() {
    // Without Landlock every run is refused for that reason first.
    if !landlock_or_skip("refused_runs_write_nothing") {
        return;
    }
    let h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    let friday = Harness::friday_night();

    let mut off = Harness::new(FakeCli::new(
        Act::AuthFailure,
        Act::AuthFailure,
        Act::AuthFailure,
    ));
    off.config.probe.enabled = false;
    assert!(
        matches!(off.run_at(friday, &scheduled()).await, RunOutcome::Refused(r) if r.contains("off"))
    );
    assert_eq!(off.record_digest(), off.pristine);

    let noon = friday + chrono::Duration::hours(10);
    assert!(
        matches!(h.run_at(noon, &scheduled()).await, RunOutcome::Refused(r) if r.contains("quiet window"))
    );

    let thursday = friday - chrono::Duration::days(1);
    assert!(
        matches!(h.run_at(thursday, &scheduled()).await, RunOutcome::Refused(r) if r.contains("not a probe day"))
    );

    std::fs::write(h.root().join("ISOLATION"), "{}").unwrap();
    assert!(
        matches!(h.run_at(friday, &scheduled()).await, RunOutcome::Refused(r) if r.contains("isolation"))
    );
    std::fs::remove_file(h.root().join("ISOLATION")).unwrap();
    assert_eq!(h.record_digest(), h.pristine, "no refusal wrote anything");

    let record = Record::open(&h.root()).unwrap();
    let held = record.try_lock().unwrap().unwrap();
    let before = h.record_digest();
    assert!(
        matches!(h.run_at(friday, &scheduled()).await, RunOutcome::Refused(r) if r.contains("lock"))
    );
    assert_eq!(h.record_digest(), before);
    drop(held);

    // Another pulse's run on this host holds the host lock.
    let host = crate::vigil::probe::record::try_lock_file(&h.state.path().join("s/host.lock"))
        .unwrap()
        .unwrap();
    assert!(
        matches!(h.run_at(friday, &scheduled()).await, RunOutcome::Refused(r) if r.contains("host lock"))
    );
    assert_eq!(h.record_digest(), before);
    drop(host);
}

#[tokio::test]
async fn a_second_scheduled_run_the_same_day_and_an_over_budget_run_are_refused() {
    if !landlock_or_skip("a_second_scheduled_run_the_same_day_and_an_over_budget_run_are_refused") {
        return;
    }
    let mut h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    h.config.probe.weekly_turns = 5;
    h.config.probe.max_turns_per_run = 3;
    let friday = Harness::friday_night();
    let known_only = RunRequest {
        if_due: true,
        only: vec!["known".into()],
        ..RunRequest::default()
    };
    let everything = h.run_at(friday, &scheduled()).await;
    assert!(
        matches!(&everything, RunOutcome::Refused(r) if r.contains("max_turns_per_run")),
        "{everything:?}"
    );
    assert!(matches!(
        h.run_at(friday, &known_only).await,
        RunOutcome::Finished(_)
    ));
    let before = h.record_digest();

    let later = friday + chrono::Duration::minutes(40);
    let again = h.run_at(later, &known_only).await;
    assert!(
        matches!(&again, RunOutcome::Refused(r) if r.contains("already finished today")),
        "{again:?}"
    );
    let by_hand = RunRequest {
        only: vec!["known".into()],
        ..RunRequest::default()
    };
    let over = h.run_at(later, &by_hand).await;
    assert!(
        matches!(&over, RunOutcome::Refused(r) if r.contains("over budget")),
        "{over:?}"
    );
    assert_eq!(h.record_digest(), before);
}

#[tokio::test]
async fn a_quota_error_stops_the_run_and_marks_it_partial() {
    if !landlock_or_skip("a_quota_error_stops_the_run_and_marks_it_partial") {
        return;
    }
    let h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Quota,
        Act::Answer(FRESH_REPLY),
    ));
    let RunOutcome::Finished(summary) = h.run_at(Harness::friday_night(), &scheduled()).await
    else {
        panic!("runs");
    };
    assert_eq!(summary.status, RunStatus::Partial);
    assert!(summary.reason.as_deref().unwrap().starts_with("quota"));
    assert_eq!(summary.turns, 1, "the refused turn spent nothing");
    let items = h.rows(Stream::Items);
    assert_eq!(items.len(), 11);
    assert!(
        items.iter().all(|r| r["arm"] == "pulse"),
        "it stopped at the quota"
    );
    assert_eq!(h.rows(Stream::Runs)[1]["status"], "partial");
}

#[tokio::test]
async fn a_failing_first_turn_aborts_the_run() {
    if !landlock_or_skip("a_failing_first_turn_aborts_the_run") {
        return;
    }
    let h = Harness::new(FakeCli::new(
        Act::AuthFailure,
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    let RunOutcome::Finished(summary) = h.run_at(Harness::friday_night(), &scheduled()).await
    else {
        panic!("runs");
    };
    assert_eq!(summary.status, RunStatus::Aborted);
    assert!(summary.reason.unwrap().contains("first turn failed"));
    assert!(h.rows(Stream::Items).is_empty());
    // An aborted run does not count: the scheduled run may try again.
    assert!(!RunStatus::Aborted.produced_results());
}

#[tokio::test]
async fn a_later_failing_turn_records_its_items_as_errors_and_the_run_goes_on() {
    if !landlock_or_skip("a_later_failing_turn_records_its_items_as_errors_and_the_run_goes_on") {
        return;
    }
    let h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::AuthFailure,
        Act::Answer(FRESH_REPLY),
    ));
    let known_only = RunRequest {
        only: vec!["known".into()],
        ..scheduled()
    };
    let RunOutcome::Finished(summary) = h.run_at(Harness::friday_night(), &known_only).await else {
        panic!("runs");
    };
    assert_eq!(summary.status, RunStatus::Complete, "{summary:?}");
    assert_eq!(summary.turns, 3, "every arm still asked");
    let items = h.rows(Stream::Items);
    let failed: Vec<&Value> = items.iter().filter(|r| r["arm"] == "input-only").collect();
    assert_eq!(failed.len(), 11);
    for row in &failed {
        assert_eq!(row["outcome"], "error", "{row}");
        assert!(
            row["error"].as_str().unwrap().contains("Invalid API key"),
            "{row}"
        );
        assert_eq!(row["run"], summary.run.as_str());
    }
    assert!(items
        .iter()
        .filter(|r| r["arm"] != "input-only")
        .all(|r| r["outcome"] != "error"));
    assert_eq!(summary.items_recorded, 33);
    assert_eq!(
        h.rows(Stream::Budget).len(),
        3,
        "the failed turn is budgeted"
    );
}

#[tokio::test]
async fn a_turn_that_reads_the_record_is_void() {
    if !landlock_or_skip("a_turn_that_reads_the_record_is_void") {
        return;
    }
    let h = Harness::new(FakeCli::new(
        Act::PeekAtRecord(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    let known_only = RunRequest {
        only: vec!["known".into()],
        ..scheduled()
    };
    let RunOutcome::Finished(summary) = h.run_at(Harness::friday_night(), &known_only).await else {
        panic!("runs");
    };
    assert_eq!(summary.status, RunStatus::Complete);
    let items = h.rows(Stream::Items);
    let pulse: Vec<&Value> = items.iter().filter(|r| r["arm"] == "pulse").collect();
    assert_eq!(pulse.len(), 11);
    for row in &pulse {
        assert_eq!(row["outcome"], "void", "{row}");
        assert_eq!(row["void_reason"], "Read call reaches record/");
    }
    assert!(items
        .iter()
        .filter(|r| r["arm"] == "input-only")
        .all(|r| r["outcome"] != "void"));
    let receipt = h
        .root()
        .join("record/probes/receipts")
        .join(&summary.run)
        .join("known-answer-pulse.json");
    let receipt: Value = serde_json::from_str(&std::fs::read_to_string(receipt).unwrap()).unwrap();
    assert_eq!(receipt["void"], "Read call reaches record/");
    assert_eq!(
        receipt["tool_calls"][0]["name"], "Read",
        "the call is kept as a receipt"
    );
}

/// Seed a drawn and planted `keepsakes` fact (sundial planted), planted at
/// `planted_at`, as `probe facts draw` / `plant` would have written it.
fn seed_planted_sundial(h: &Harness, planted_at: DateTime<Utc>) {
    let record = Record::open(&h.root()).unwrap();
    record
        .append(
            Stream::Facts,
            crate::vigil::probe::paired::tests::draw_row(
                "fseed01",
                "keepsakes",
                crate::vigil::probe::paired::Which::A,
                planted_at - chrono::Duration::hours(1),
            ),
        )
        .unwrap();
    record
        .append(
            Stream::Facts,
            serde_json::json!({"event": "plant", "fact": "fseed01", "value": "lychee",
                               "planted_at": planted_at.to_rfc3339(), "at": planted_at.to_rfc3339()}),
        )
        .unwrap();
}

fn paired_cli() -> FakeCli {
    FakeCli::with_needles(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
        &[
            ("favourite sundial", "Lychee"),
            ("favourite umbrella", "Probably carrots."),
        ],
    )
}

#[tokio::test]
async fn a_due_paired_fact_asks_both_sides_and_logs_the_ask() {
    if !landlock_or_skip("a_due_paired_fact_asks_both_sides_and_logs_the_ask") {
        return;
    }
    let h = Harness::new(paired_cli());
    let friday = Harness::friday_night();
    seed_planted_sundial(&h, friday - chrono::Duration::hours(26));
    let paired_only = RunRequest {
        only: vec!["paired".into()],
        ..RunRequest::default()
    };
    let RunOutcome::Finished(summary) = h.run_at(friday, &paired_only).await else {
        panic!("runs");
    };
    assert_eq!(summary.status, RunStatus::Complete, "{summary:?}");
    assert_eq!(
        summary.turns, 4,
        "the planted side and its twin, then the planted side for the fresh arm, \
         volunteered and as a forced guess"
    );
    let items = h.rows(Stream::Items);
    let side = |s: &str| items.iter().find(|r| r["side"] == s).unwrap();
    assert_eq!(side("planted")["outcome"], "correct");
    assert_eq!(side("planted")["truth"], "lychee");
    assert_eq!(side("planted")["answered_from"], "context");
    assert_eq!(
        side("twin")["outcome"],
        "wrong",
        "an answer for the twin is made up"
    );
    assert!(side("twin")["truth"].is_null());
    assert_eq!(side("twin")["offset_days"], 1);
    assert_eq!(
        side("twin")["elapsed_days"],
        1,
        "planted 26 h before the run: asked 1 day after"
    );
    let asked: Vec<Value> = h
        .rows(Stream::Facts)
        .into_iter()
        .filter(|r| r["event"] == "ask")
        .collect();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["planted"], "correct");
    // R2-6: the fresh arm was asked the planted question; this fake CLI
    // answers by prompt, so it "guessed" the value.
    let chance = side("chance");
    assert_eq!(chance["arm"], "fresh");
    assert_eq!(chance["outcome"], "correct");
    assert_eq!(chance["truth"], "lychee");
    assert_eq!(asked[0]["chance"], "correct");
    // R3-A: and as a forced guess, with no UNKNOWN to fall back on.
    let guess = side("guess");
    assert_eq!(guess["arm"], "fresh");
    assert_eq!(guess["outcome"], "correct");
    assert_eq!(guess["truth"], "lychee");
    assert_eq!(guess["answer_kind"], "value");
    assert_eq!(asked[0]["guess"], "correct");
    // R3-C: what the answers depend on besides the model is on every row.
    for row in &items {
        assert_eq!(
            row["fresh_sha256"],
            crate::vigil::probe::sets::fresh_sha256()
        );
        assert_eq!(
            row["disclosure_sha256"],
            hex(&Sha256::digest(
                crate::server::prompt::MEASUREMENT_DISCLOSURE.as_bytes()
            ))
        );
        assert_ne!(
            row["set_sha256"], row["file_sha256"],
            "the set's definition covers the fresh line, not only its file"
        );
    }

    // The same offset is never asked twice.
    let later = friday + chrono::Duration::minutes(30);
    let RunOutcome::Finished(again) = h.run_at(later, &paired_only).await else {
        panic!("runs");
    };
    assert_eq!(again.turns, 0);
}

#[tokio::test]
async fn a_later_paired_turn_that_fails_is_an_error_and_the_fact_still_asked() {
    if !landlock_or_skip("a_later_paired_turn_that_fails_is_an_error_and_the_fact_still_asked") {
        return;
    }
    let h = Harness::new(FakeCli::with_needles(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
        &[
            ("favourite sundial", "Lychee"),
            ("favourite umbrella", FAIL),
        ],
    ));
    let friday = Harness::friday_night();
    seed_planted_sundial(&h, friday - chrono::Duration::hours(26));
    let paired_only = RunRequest {
        only: vec!["paired".into()],
        ..RunRequest::default()
    };
    let RunOutcome::Finished(summary) = h.run_at(friday, &paired_only).await else {
        panic!("runs");
    };
    assert_eq!(summary.status, RunStatus::Complete, "{summary:?}");
    assert_eq!(
        summary.turns, 4,
        "the twin failed; the fresh arm's two asks still ran"
    );
    let items = h.rows(Stream::Items);
    let twin = items.iter().find(|r| r["side"] == "twin").unwrap();
    assert_eq!(twin["outcome"], "error", "{twin}");
    assert!(
        twin["error"].as_str().unwrap().contains("Invalid API key"),
        "{twin}"
    );
    assert_eq!(twin["measure"], "paired-facts");
    assert_eq!(twin["fact"], "fseed01");
    let planted = items.iter().find(|r| r["side"] == "planted").unwrap();
    assert_eq!(planted["outcome"], "correct");
    let ask = h
        .rows(Stream::Facts)
        .into_iter()
        .find(|r| r["event"] == "ask")
        .unwrap();
    assert_eq!(ask["planted"], "correct");
    assert_eq!(ask["twin"], "error");
}

#[tokio::test]
async fn a_twin_the_owner_talked_about_is_void() {
    if !landlock_or_skip("a_twin_the_owner_talked_about_is_void") {
        return;
    }
    let h = Harness::new(paired_cli());
    let friday = Harness::friday_night();
    let planted_at = friday - chrono::Duration::hours(26);
    seed_planted_sundial(&h, planted_at);
    let wal = h.root().join("sessions/wal");
    std::fs::create_dir_all(&wal).unwrap();
    std::fs::write(
        wal.join("owner.jsonl"),
        serde_json::json!({"ts": planted_at.to_rfc3339(), "seq": 1, "role": "user",
                           "content": "\nUser message: my favourite umbrella? not telling",
                           "meta": {"channel": "tui"}})
        .to_string()
            + "\n",
    )
    .unwrap();
    let paired_only = RunRequest {
        only: vec!["paired".into()],
        ..RunRequest::default()
    };
    assert!(matches!(
        h.run_at(friday, &paired_only).await,
        RunOutcome::Finished(_)
    ));
    let items = h.rows(Stream::Items);
    let twin = items.iter().find(|r| r["side"] == "twin").unwrap();
    assert_eq!(twin["outcome"], "void");
    assert_eq!(
        twin["void_reason"],
        "an owner message named the twin's slot"
    );
    let planted = items.iter().find(|r| r["side"] == "planted").unwrap();
    assert_eq!(planted["outcome"], "correct");
}

/// Audit #16: a run stopped after a fact's planted side used that offset
/// up, so the next run does not ask (and count) the planted side again.
#[tokio::test]
async fn a_stop_between_planted_and_twin_uses_the_offset_up() {
    if !landlock_or_skip("a_stop_between_planted_and_twin_uses_the_offset_up") {
        return;
    }
    let h = Harness::new(FakeCli::with_needles(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
        &[
            ("favourite sundial", "Lychee"),
            ("favourite umbrella", QUOTA),
        ],
    ));
    let friday = Harness::friday_night();
    seed_planted_sundial(&h, friday - chrono::Duration::hours(26));
    let paired_only = RunRequest {
        only: vec!["paired".into()],
        ..RunRequest::default()
    };
    let RunOutcome::Finished(summary) = h.run_at(friday, &paired_only).await else {
        panic!("runs");
    };
    assert_eq!(summary.status, RunStatus::Partial, "{summary:?}");
    let asked: Vec<Value> = h
        .rows(Stream::Facts)
        .into_iter()
        .filter(|r| r["event"] == "ask")
        .collect();
    assert_eq!(asked.len(), 1, "the offset is used up");
    assert_eq!(asked[0]["planted"], "correct");
    assert!(asked[0]["twin"].is_null());
    let RunOutcome::Finished(again) = h
        .run_at(friday + chrono::Duration::minutes(30), &paired_only)
        .await
    else {
        panic!("runs");
    };
    assert_eq!(again.turns, 0, "the planted side is not asked twice");
}

/// Audit #16: an error after the start row still ends with teardown and a
/// finish row.
#[tokio::test]
async fn an_error_mid_run_still_writes_the_finish_row() {
    if !landlock_or_skip("an_error_mid_run_still_writes_the_finish_row") {
        return;
    }
    let h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    // The item rows cannot be appended: the first scored answer fails.
    std::fs::create_dir_all(h.root().join("record/probes/items.jsonl")).unwrap();
    let known_only = RunRequest {
        only: vec!["known".into()],
        ..scheduled()
    };
    let RunOutcome::Finished(summary) = h.run_at(Harness::friday_night(), &known_only).await else {
        panic!("runs");
    };
    assert_eq!(summary.status, RunStatus::Partial, "{summary:?}");
    assert!(summary.reason.unwrap().contains("stopped by an error"));
    let runs = h.rows(Stream::Runs);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1]["event"], "finish");
    let scratch_dirs = std::fs::read_dir(h.state.path().join("s"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .count();
    assert_eq!(scratch_dirs, 0, "the scratch was torn down");
}

/// Audit #17: with no owner message the gap and restart families have no
/// truth, and no turn is spent on them.
#[tokio::test]
async fn families_without_a_truth_spend_no_turns() {
    if !landlock_or_skip("families_without_a_truth_spend_no_turns") {
        return;
    }
    let h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    let situation_only = RunRequest {
        only: vec!["situation".into()],
        ..scheduled()
    };
    let RunOutcome::Finished(summary) = h.run_at(Harness::friday_night(), &situation_only).await
    else {
        panic!("runs");
    };
    assert_eq!(summary.turns, 6, "tomorrow and model only, three arms each");
    let items = h.rows(Stream::Items);
    assert!(items
        .iter()
        .all(|r| matches!(r["family"].as_str(), Some("tomorrow" | "model"))));
    assert!(items.iter().all(|r| r["outcome"] != "no-truth"));
}

/// Loopback split: a run that is not confined like the probe unit is
/// refused, and writes nothing.
#[tokio::test]
async fn an_unconfined_run_is_refused() {
    if !landlock_or_skip("an_unconfined_run_is_refused") {
        return;
    }
    let mut h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    fn unconfined() -> Result<(), String> {
        Err("not confined like the probe unit: the IPv4 loopback is reachable".into())
    }
    h.confinement = &unconfined;
    let outcome = h.run_at(Harness::friday_night(), &scheduled()).await;
    assert!(
        matches!(&outcome, RunOutcome::Refused(r) if r.contains("not confined")),
        "{outcome:?}"
    );
    assert_eq!(h.record_digest(), h.pristine);
}

/// Audit R2-8: a broken budget chain fails closed, it does not reset the
/// week's spend.
#[tokio::test]
async fn a_broken_budget_chain_refuses_the_run() {
    if !landlock_or_skip("a_broken_budget_chain_refuses_the_run") {
        return;
    }
    let h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    let record = Record::open(&h.root()).unwrap();
    record
        .append(
            Stream::Budget,
            serde_json::json!({"run": "r0", "at": "2026-10-15T00:00:00Z"}),
        )
        .unwrap();
    let path = h.root().join("record/probes/budget.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("r0", "r9")).unwrap();
    let before = h.record_digest();
    let outcome = h.run_at(Harness::friday_night(), &scheduled()).await;
    assert!(
        matches!(&outcome, RunOutcome::Refused(r) if r.contains("budget.jsonl is broken")),
        "{outcome:?}"
    );
    assert_eq!(h.record_digest(), before);
}

/// Audit R2-10: a signal during a turn ends it at once; the run still tears
/// down and writes its finish row.
#[tokio::test]
async fn a_signal_mid_turn_stops_the_run_with_a_finish_row() {
    if !landlock_or_skip("a_signal_mid_turn_stops_the_run_with_a_finish_row") {
        return;
    }
    let cli = FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    );
    // Make the pulse arm's turn hang, as a slow model would.
    let script = cli.bin();
    let text = std::fs::read_to_string(&script).unwrap();
    std::fs::write(
        &script,
        text.replace("code=$(cat", "[ \"$arm\" = pulse ] && sleep 60\ncode=$(cat"),
    )
    .unwrap();
    let h = Harness::new(cli);
    let stop = h.stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let started = std::time::Instant::now();
    let RunOutcome::Finished(summary) = h.run_at(Harness::friday_night(), &scheduled()).await
    else {
        panic!("runs");
    };
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the hung turn was cut"
    );
    assert_eq!(summary.status, RunStatus::Aborted, "{summary:?}");
    assert!(summary.reason.unwrap().contains("signal"));
    let runs = h.rows(Stream::Runs);
    assert_eq!(runs.last().unwrap()["event"], "finish");
    assert_eq!(
        h.rows(Stream::Budget).len(),
        1,
        "the cut turn counts against the budget"
    );
    let scratch_dirs = std::fs::read_dir(h.state.path().join("s"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .count();
    assert_eq!(scratch_dirs, 0, "torn down");
}

/// Audit R2-10: a signal while waiting for another pulse's run ends the
/// wait at once, and nothing is written.
#[tokio::test]
async fn a_signal_during_the_host_lock_wait_refuses_at_once() {
    if !landlock_or_skip("a_signal_during_the_host_lock_wait_refuses_at_once") {
        return;
    }
    let h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    std::fs::create_dir_all(h.state.path().join("s")).unwrap();
    let host = crate::vigil::probe::record::try_lock_file(&h.state.path().join("s/host.lock"))
        .unwrap()
        .unwrap();
    let stop = h.stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let started = std::time::Instant::now();
    let outcome = h
        .run_with_host_wait(
            Harness::friday_night(),
            &scheduled(),
            std::time::Duration::from_secs(600),
        )
        .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert!(
        matches!(&outcome, RunOutcome::Refused(r) if r.contains("signal")),
        "{outcome:?}"
    );
    assert_eq!(h.record_digest(), h.pristine);
    drop(host);
}

/// Audit R2-10: no turn starts once the run's time budget is spent.
#[tokio::test]
async fn a_spent_time_budget_stops_the_run() {
    if !landlock_or_skip("a_spent_time_budget_stops_the_run") {
        return;
    }
    let mut h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    h.time_budget = std::time::Duration::ZERO;
    let RunOutcome::Finished(summary) = h.run_at(Harness::friday_night(), &scheduled()).await
    else {
        panic!("runs");
    };
    assert_eq!(summary.status, RunStatus::Aborted);
    assert!(summary.reason.unwrap().contains("time budget"));
    assert_eq!(summary.turns, 0);
    assert!(h.rows(Stream::Budget).is_empty(), "nothing was spent");
    assert!(h.rows(Stream::Runs)[1].get("memory_peak_bytes").is_some());
}

/// Audit R2-11: outside the unit there are no credentials, so no sets:
/// the run refuses and writes nothing.
#[tokio::test]
async fn a_run_without_the_credentials_refuses() {
    let mut h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    h.without_credentials = true;
    let outcome = h.run_at(Harness::friday_night(), &scheduled()).await;
    assert!(
        matches!(&outcome, RunOutcome::Refused(r) if r.contains("credentials")),
        "{outcome:?}"
    );
    assert_eq!(h.record_digest(), h.pristine);
}

#[test]
fn only_splits_paired_facts_from_item_sets() {
    assert_eq!(split_only(&[]), (true, vec![]));
    assert_eq!(split_only(&["paired".into()]), (true, vec![]));
    assert_eq!(
        split_only(&["known".into()]),
        (false, vec!["known".to_string()])
    );
    assert_eq!(
        split_only(&["paired-facts".into(), "situation".into()]),
        (true, vec!["situation".to_string()])
    );
}

#[tokio::test]
async fn a_set_that_differs_from_its_sealed_digest_refuses_the_run() {
    let h = Harness::new(FakeCli::new(
        Act::Answer(PULSE_REPLY),
        Act::Answer(PULSE_REPLY),
        Act::Answer(FRESH_REPLY),
    ));
    let set = h.credentials.path().join("known-answer.toml");
    let text = std::fs::read_to_string(&set).unwrap();
    std::fs::write(
        &set,
        text.replace("Fixture question about the name.", "Easier?"),
    )
    .unwrap();
    let outcome = h.run_at(Harness::friday_night(), &scheduled()).await;
    assert!(
        matches!(&outcome, RunOutcome::Refused(r) if r.contains("does not match the digest")),
        "{outcome:?}"
    );
    assert_eq!(h.record_digest(), h.pristine);
}

#[test]
fn the_plan_counts_turns_per_set() {
    let sets = sets::all(&crate::vigil::probe::sealed::tests::fixture_sets()).unwrap();
    let turns: u32 = sets.iter().map(|s| s.turns_for(s.draw("r").len())).sum();
    assert_eq!(turns, 15, "3 known-answer + 12 situation-use");
    let _ = Path::new("/");
}

/// R3-A: four asks per due fact. The worst case, every item set (15 turns)
/// and `max_active` due facts (3 × 4 = 12), fits the default cap of 27 (D
/// 2026-10-21; paired-facts.md, "Why `max_active` is 3"), and three such
/// runs fit the weekly budget. A change to the cap, `max_active` or the
/// asks per fact must revisit that reasoning; this fails first.
#[test]
fn three_due_facts_and_every_set_fit_the_default_cap() {
    let item_sets: u32 = 15;
    let config = crate::config::ProbeConfig::default();
    let per_fact = u32::try_from(paired::ASKS_PER_FACT).unwrap();
    let max_active = u32::try_from(LoadedPairs::fixture().set.max_active).unwrap();
    assert_eq!(
        (
            config.max_turns_per_run,
            config.weekly_turns,
            per_fact,
            max_active
        ),
        (27, 81, 4, 3)
    );
    let worst = item_sets + max_active * per_fact;
    assert!(
        worst <= config.max_turns_per_run,
        "three due facts and every set fit the cap"
    );
    assert!(
        3 * worst <= config.weekly_turns,
        "three such runs a week fit"
    );
}
