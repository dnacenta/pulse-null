//! `{entity}/outreach.json` — the sent log, D's responses, and the rejection
//! log (PN-94, spec §4).
//!
//! ## Persistence discipline
//!
//! Identical to `predictions.json` (PN-86): every write is a locked
//! read-modify-write via [`save_delta`], serialized in-process by a static
//! mutex and cross-process by an exclusive lock on `outreach.json.lock`, and
//! applied against a fresh load from disk. The atomic rename in [`save`]
//! prevents torn files; the lock prevents lost updates.
//!
//! ## Fail-closed
//!
//! [`load`] fail-opens so `outreach status` can still render something on a
//! damaged file. [`load_strict`], used inside [`save_delta`], fail-*closes*:
//! an existing file that cannot be read or parsed aborts the delta rather
//! than silently wiping the record of what was already sent. Wiping it would
//! reset every daily cap and the whole response-rate window at once — the
//! store is the only memory the caps have.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use super::{Cost, RejectionReason};
use crate::events::SalienceKind;

/// File name for the outreach log on disk.
const OUTREACH_FILE: &str = "outreach.json";

/// Temporary file used during atomic writes.
const OUTREACH_TMP: &str = "outreach.json.tmp";

/// Sent messages retained. Comfortably exceeds `feedback_window` × kinds so
/// the rolling response rate never runs off the end of the log.
const MAX_SENT: usize = 200;

/// Rejections retained for `outreach status`. The rejection log is the only
/// evidence about whether the gate is calibrated (spec §8), so it is kept —
/// but bounded, because nothing else prunes it.
const MAX_REJECTIONS: usize = 100;

/// How D reacted to a message, when he said so explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rating {
    Useful,
    Noise,
}

impl Rating {
    /// Parse the one-word ratings from spec §2.4 (`/useful`, `/noise`).
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label
            .trim()
            .trim_start_matches('/')
            .to_ascii_lowercase()
            .as_str()
        {
            "useful" => Some(Self::Useful),
            "noise" => Some(Self::Noise),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Useful => "useful",
            Self::Noise => "noise",
        }
    }
}

impl std::fmt::Display for Rating {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One outreach message that cleared the gate.
///
/// Recorded at admission, not at delivery. Counting sends at admission
/// over-counts if a queued intent never runs, which tightens the channel;
/// counting at delivery would let an unbounded number of admitted messages
/// sit in the queue under a cap of one, which loosens it. Over-firing is the
/// unrecoverable direction (spec §6.4), so the count is taken early.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentMessage {
    pub id: String,
    pub kind: SalienceKind,
    #[serde(default)]
    pub thread_id: Option<String>,
    pub headline: String,
    pub evidence: String,
    pub confidence: f64,
    pub cost: Cost,
    pub sent_at: DateTime<Utc>,
    /// When D responded. `None` is the fail-closed reading: no evidence of a
    /// response counts as no response (spec §2.4).
    #[serde(default)]
    pub responded_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub rating: Option<Rating>,
}

impl SentMessage {
    /// Whether D responded to this message.
    #[must_use]
    pub fn responded(&self) -> bool {
        self.responded_at.is_some()
    }

    /// Time from send to response, when there was one.
    #[must_use]
    pub fn response_latency(&self) -> Option<chrono::Duration> {
        self.responded_at.map(|at| at - self.sent_at)
    }
}

/// One candidate the gate turned away, kept so the gate can be audited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedCandidate {
    pub kind: SalienceKind,
    pub headline: String,
    pub reason: RejectionReason,
    pub rejected_at: DateTime<Utc>,
}

/// A cap tightening that has already been announced to D.
///
/// Presence means the notice went out; absence means it has not. Without
/// this the notice would re-fire on every candidate while the response rate
/// stayed low, which is the same channel erosion the caps exist to prevent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnouncedTightening {
    pub kind: SalienceKind,
    pub tightened_cap: u32,
    pub announced_at: DateTime<Utc>,
}

/// The persisted outreach record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutreachStore {
    #[serde(default)]
    pub sent: Vec<SentMessage>,
    #[serde(default)]
    pub rejections: Vec<RejectedCandidate>,
    #[serde(default)]
    pub announced_tightenings: Vec<AnnouncedTightening>,
}

impl OutreachStore {
    /// Append a sent message and prune the log back to its bound.
    pub fn record_sent(&mut self, message: SentMessage) {
        self.sent.push(message);
        prune_front(&mut self.sent, MAX_SENT);
    }

    /// Append a rejection and prune the log back to its bound.
    pub fn record_rejection(&mut self, rejection: RejectedCandidate) {
        self.rejections.push(rejection);
        prune_front(&mut self.rejections, MAX_REJECTIONS);
    }

    /// The most recent `window` messages of `kind`, oldest first.
    #[must_use]
    pub fn recent(&self, kind: SalienceKind, window: usize) -> Vec<&SentMessage> {
        let mut of_kind: Vec<&SentMessage> = self.sent.iter().filter(|m| m.kind == kind).collect();
        if of_kind.len() > window {
            of_kind.drain(..of_kind.len() - window);
        }
        of_kind
    }

    /// How many messages of `kind` were sent on the local calendar day that
    /// `now` falls on. The day boundary is D's local midnight, not UTC's —
    /// a cap of one per day means one per *his* day.
    #[must_use]
    pub fn sent_today(&self, kind: SalienceKind, tz: Tz, now: DateTime<Utc>) -> u32 {
        let today = now.with_timezone(&tz).date_naive();
        self.sent
            .iter()
            .filter(|m| m.kind == kind && m.sent_at.with_timezone(&tz).date_naive() == today)
            .count()
            .try_into()
            .unwrap_or(u32::MAX)
    }

    /// Record D's response to a message. Returns false when the id is unknown.
    ///
    /// Re-recording a response is idempotent on the timestamp (the first
    /// response is the one that measures latency) but always applies the
    /// newest explicit rating, so `/useful` can correct a `/noise`.
    pub fn mark_responded(&mut self, id: &str, at: DateTime<Utc>, rating: Option<Rating>) -> bool {
        let Some(message) = self.sent.iter_mut().find(|m| m.id == id) else {
            return false;
        };
        message.responded_at.get_or_insert(at);
        if rating.is_some() {
            message.rating = rating;
        }
        true
    }

    /// The tightening already announced for `kind`, if any.
    #[must_use]
    pub fn announced(&self, kind: SalienceKind) -> Option<&AnnouncedTightening> {
        self.announced_tightenings.iter().find(|a| a.kind == kind)
    }

    /// Record that D has been told the cap for `kind` is halved.
    pub fn record_announcement(
        &mut self,
        kind: SalienceKind,
        tightened_cap: u32,
        at: DateTime<Utc>,
    ) {
        self.announced_tightenings.retain(|a| a.kind != kind);
        self.announced_tightenings.push(AnnouncedTightening {
            kind,
            tightened_cap,
            announced_at: at,
        });
    }

    /// Forget the announcement for `kind` — the tightening has lifted, so a
    /// future one must be announced again rather than assumed known.
    pub fn clear_announcement(&mut self, kind: SalienceKind) {
        self.announced_tightenings.retain(|a| a.kind != kind);
    }

    /// Every headline already sent — one half of gate 1's corpus.
    #[must_use]
    pub fn sent_headlines(&self) -> Vec<String> {
        self.sent.iter().map(|m| m.headline.clone()).collect()
    }

    /// Ids of messages still awaiting a response, newest first. This is the
    /// list `outreach respond` picks from.
    #[must_use]
    pub fn unanswered(&self) -> Vec<&SentMessage> {
        let mut open: Vec<&SentMessage> = self.sent.iter().filter(|m| !m.responded()).collect();
        open.reverse();
        open
    }
}

/// Drop the oldest entries until `items` fits inside `max`.
fn prune_front<T>(items: &mut Vec<T>, max: usize) {
    if items.len() > max {
        items.drain(..items.len() - max);
    }
}

/// Load the outreach store, fail-open.
///
/// A missing, unreadable or corrupt file yields an empty store so read-only
/// callers (`outreach status`) always render. Writers must use
/// [`save_delta`], which fail-closes instead.
#[must_use]
pub fn load(root_dir: &Path) -> OutreachStore {
    let path = root_dir.join(OUTREACH_FILE);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), error = %e, "Failed to read outreach log");
            }
            return OutreachStore::default();
        }
    };
    serde_json::from_str(&content).unwrap_or_else(|e| {
        tracing::warn!(path = %path.display(), error = %e, "Corrupt outreach log");
        OutreachStore::default()
    })
}

/// Fail-closed load for read-modify-write callers.
///
/// A missing file is a fresh entity and starts empty. An existing file that
/// cannot be read or parsed is an error and the delta is aborted — writing
/// back an empty store would reset every daily cap and the entire response
/// window in one stroke. A corrupt file is quarantined to
/// `outreach.json.corrupt.<ts>` so the failure costs one loud cycle instead
/// of wedging every future write, matching `predictions.json` (SEC-003).
fn load_strict(root_dir: &Path) -> Result<OutreachStore, Box<dyn std::error::Error + Send + Sync>> {
    let path = root_dir.join(OUTREACH_FILE);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OutreachStore::default()),
        Err(e) => return Err(Box::new(e)),
    };
    match serde_json::from_str(&content) {
        Ok(store) => Ok(store),
        Err(e) => {
            let quarantine = root_dir.join(format!(
                "{OUTREACH_FILE}.corrupt.{}",
                Utc::now().format("%Y%m%dT%H%M%S")
            ));
            match fs::rename(&path, &quarantine) {
                Ok(()) => Err(format!(
                    "corrupt outreach.json quarantined to {} ({e})",
                    quarantine.display()
                )
                .into()),
                Err(rename_err) => Err(format!(
                    "corrupt outreach.json, quarantine also failed ({rename_err}): {e}"
                )
                .into()),
            }
        }
    }
}

/// Locked read-modify-write on the outreach store.
///
/// The admission decision and the write that records it happen inside one
/// call, so two candidates racing on a cap of one cannot both observe zero
/// sent today.
pub fn save_delta<T>(
    root_dir: &Path,
    apply: impl FnOnce(&mut OutreachStore) -> T,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
    static IN_PROCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = IN_PROCESS.lock().unwrap_or_else(|p| p.into_inner());

    fs::create_dir_all(root_dir)?;
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root_dir.join(format!("{OUTREACH_FILE}.lock")))?;
    lock_file.lock()?;

    let mut store = load_strict(root_dir)?;
    let out = apply(&mut store);
    save(root_dir, &store)?;
    Ok(out)
    // lock_file drop releases the flock
}

/// Async wrapper for [`save_delta`] — offloads the locked IO (and, for the
/// admission path, the embedding model load) to a blocking thread so neither
/// the flock nor the ONNX runtime ever parks a tokio worker.
pub async fn save_delta_async<T: Send + 'static>(
    root_dir: PathBuf,
    apply: impl FnOnce(&mut OutreachStore) -> T + Send + 'static,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
    match tokio::task::spawn_blocking(move || save_delta(&root_dir, apply)).await {
        Ok(result) => result,
        Err(join_err) => Err(Box::new(join_err)),
    }
}

/// Save the store atomically (tmp file, then rename).
pub fn save(
    root_dir: &Path,
    store: &OutreachStore,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    fs::create_dir_all(root_dir)?;
    let path = root_dir.join(OUTREACH_FILE);
    let tmp_path = root_dir.join(OUTREACH_TMP);

    // Pretty-printed: unlike predictions.json this file is small, bounded,
    // and the first thing a human reads when asking "why did the cap move?".
    let content = serde_json::to_string_pretty(store)?;
    fs::write(&tmp_path, &content)?;
    fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Ids of predictions that have actually resolved.
///
/// Gate 2 accepts a prediction id as an external referent only if it names a
/// prediction that resolved — an unresolved id is a pointer to the entity's
/// own expectation, which is exactly the self-authored evidence the gate
/// exists to reject.
#[must_use]
pub fn resolved_prediction_ids(root_dir: &Path) -> HashSet<String> {
    crate::prediction::store::load(root_dir, crate::config::PredictionConfig::default())
        .predictions
        .iter()
        .filter(|p| p.resolution.is_some())
        .map(|p| p.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outreach::test_support::sent;
    use tempfile::TempDir;

    #[test]
    fn load_returns_empty_when_missing() {
        let tmp = TempDir::new().unwrap();
        let store = load(tmp.path());
        assert!(store.sent.is_empty());
        assert!(store.rejections.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut store = OutreachStore::default();
        store.record_sent(sent("m1", SalienceKind::Finding, Utc::now(), false));
        save(tmp.path(), &store).unwrap();

        let loaded = load(tmp.path());
        assert_eq!(loaded.sent.len(), 1);
        assert_eq!(loaded.sent[0].id, "m1");
    }

    #[test]
    fn sent_today_counts_the_local_day_not_the_utc_day() {
        // 00:30 UTC on the 2nd is 01:30 on the 2nd in Madrid but still the
        // 1st in New York. A cap of one per day must mean one per D's day.
        let madrid: Tz = "Europe/Madrid".parse().unwrap();
        let new_york: Tz = "America/New_York".parse().unwrap();
        let now: DateTime<Utc> = "2026-08-02T00:30:00Z".parse().unwrap();
        let earlier: DateTime<Utc> = "2026-08-01T22:00:00Z".parse().unwrap();

        let mut store = OutreachStore::default();
        store.record_sent(sent("m1", SalienceKind::Finding, earlier, false));

        // Madrid: 2026-08-02 00:30 vs 2026-08-02 00:00 — same local day.
        assert_eq!(store.sent_today(SalienceKind::Finding, madrid, now), 1);
        // New York: 2026-08-01 20:30 vs 2026-08-01 18:00 — same local day.
        assert_eq!(store.sent_today(SalienceKind::Finding, new_york, now), 1);
        // A different kind shares no budget.
        assert_eq!(store.sent_today(SalienceKind::Callback, madrid, now), 0);
    }

    #[test]
    fn sent_today_excludes_yesterday() {
        let utc: Tz = "UTC".parse().unwrap();
        let now: DateTime<Utc> = "2026-08-02T09:00:00Z".parse().unwrap();
        let yesterday: DateTime<Utc> = "2026-08-01T09:00:00Z".parse().unwrap();
        let mut store = OutreachStore::default();
        store.record_sent(sent("m1", SalienceKind::Finding, yesterday, false));
        assert_eq!(store.sent_today(SalienceKind::Finding, utc, now), 0);
    }

    #[test]
    fn mark_responded_is_idempotent_on_time_but_not_on_rating() {
        let first: DateTime<Utc> = "2026-08-02T09:00:00Z".parse().unwrap();
        let later: DateTime<Utc> = "2026-08-02T11:00:00Z".parse().unwrap();
        let mut store = OutreachStore::default();
        store.record_sent(sent("m1", SalienceKind::Finding, first, false));

        assert!(store.mark_responded("m1", first, Some(Rating::Noise)));
        assert!(store.mark_responded("m1", later, Some(Rating::Useful)));
        assert_eq!(store.sent[0].responded_at, Some(first));
        assert_eq!(store.sent[0].rating, Some(Rating::Useful));
        assert!(!store.mark_responded("nope", later, None));
    }

    #[test]
    fn recent_takes_the_newest_window_of_that_kind() {
        let mut store = OutreachStore::default();
        for i in 0..5 {
            let kind = if i % 2 == 0 {
                SalienceKind::Finding
            } else {
                SalienceKind::Callback
            };
            store.record_sent(sent(&format!("m{i}"), kind, Utc::now(), false));
        }
        let recent = store.recent(SalienceKind::Finding, 2);
        assert_eq!(
            recent.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["m2", "m4"]
        );
    }

    #[test]
    fn sent_log_is_bounded() {
        let mut store = OutreachStore::default();
        for i in 0..(MAX_SENT + 10) {
            store.record_sent(sent(
                &format!("m{i}"),
                SalienceKind::Finding,
                Utc::now(),
                false,
            ));
        }
        assert_eq!(store.sent.len(), MAX_SENT);
        // The oldest are the ones dropped.
        assert_eq!(store.sent[0].id, "m10");
    }

    #[test]
    fn rejection_log_is_bounded() {
        let mut store = OutreachStore::default();
        for _ in 0..(MAX_REJECTIONS + 5) {
            store.record_rejection(RejectedCandidate {
                kind: SalienceKind::Finding,
                headline: "h".into(),
                reason: RejectionReason::NoCostStated,
                rejected_at: Utc::now(),
            });
        }
        assert_eq!(store.rejections.len(), MAX_REJECTIONS);
    }

    #[test]
    fn save_delta_applies_against_fresh_disk_state() {
        let tmp = TempDir::new().unwrap();

        // Stale in-memory snapshot taken by caller A, never written back.
        let mut stale = load(tmp.path());
        stale.record_sent(sent("a-memory", SalienceKind::Finding, Utc::now(), false));

        save_delta(tmp.path(), |s| {
            s.record_sent(sent("b-disk", SalienceKind::Finding, Utc::now(), false));
        })
        .unwrap();
        save_delta(tmp.path(), |s| {
            s.record_sent(sent("a-final", SalienceKind::Finding, Utc::now(), false));
        })
        .unwrap();

        let ids: Vec<String> = load(tmp.path()).sent.iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids, vec!["b-disk", "a-final"]);
    }

    #[test]
    fn save_delta_concurrent_writers_lose_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    save_delta(&root, |s| {
                        s.record_sent(sent(
                            &format!("w{i}"),
                            SalienceKind::Finding,
                            Utc::now(),
                            false,
                        ));
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(load(tmp.path()).sent.len(), 8);
    }

    #[test]
    fn save_delta_aborts_and_quarantines_corrupt_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(OUTREACH_FILE);
        fs::write(&path, "{ not valid json").unwrap();

        let result = save_delta(tmp.path(), |s| {
            s.record_sent(sent("nope", SalienceKind::Finding, Utc::now(), false));
        });
        assert!(result.is_err(), "a corrupt log must not be silently wiped");
        assert!(!path.exists());

        let quarantined: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("outreach.json.corrupt.")
            })
            .collect();
        assert_eq!(quarantined.len(), 1);

        // The next delta self-heals from empty.
        save_delta(tmp.path(), |s| {
            s.record_sent(sent("after", SalienceKind::Finding, Utc::now(), false));
        })
        .unwrap();
        assert_eq!(load(tmp.path()).sent.len(), 1);
    }

    #[test]
    fn announcements_are_one_per_kind_and_clearable() {
        let now = Utc::now();
        let mut store = OutreachStore::default();
        store.record_announcement(SalienceKind::Finding, 1, now);
        store.record_announcement(SalienceKind::Finding, 0, now);
        assert_eq!(store.announced_tightenings.len(), 1);
        assert_eq!(
            store
                .announced(SalienceKind::Finding)
                .unwrap()
                .tightened_cap,
            0
        );

        store.clear_announcement(SalienceKind::Finding);
        assert!(store.announced(SalienceKind::Finding).is_none());
    }

    #[test]
    fn rating_parses_slash_commands() {
        assert_eq!(Rating::from_label("/useful"), Some(Rating::Useful));
        assert_eq!(Rating::from_label("NOISE"), Some(Rating::Noise));
        assert_eq!(Rating::from_label("meh"), None);
    }
}
