//! The feedback loop — the part that actually matters (PN-94, spec §2.4).
//!
//! An outreach channel with no feedback converges to noise, and it converges
//! *silently*: the entity's own estimate of message quality is exactly the
//! faculty that would have to detect the drift. D's response is the only
//! independent signal, because it is the only one the entity cannot author.
//!
//! Two properties here are deliberate and neither is an accident of
//! implementation:
//!
//! * **Fail-closed under neglect.** A message with no recorded response
//!   counts as unanswered. If nobody looks at the log, caps tighten rather
//!   than loosen — a control that fails open when unread is not a control.
//! * **The tightening is announced.** Silent self-throttling would let the
//!   channel die without D ever learning it did. [`Tightening`] exists so the
//!   halving has something to say for itself; see
//!   [`crate::outreach::Admission::pending_notice`] for why the notice fires
//!   on rejections too.
//!
//! Non-response is a weak signal — D may be asleep, busy, on a plane. It is
//! accepted anyway, because the alternative is a self-assessed quality score,
//! which is strictly worse: it correlates with the very error it is meant to
//! detect. The weakness is handled by the window size, not by substituting a
//! judgement the entity makes about itself.

use std::path::Path;

use chrono::{DateTime, Utc};

use super::store::{self, OutreachStore, Rating};
use crate::config::OutreachConfig;
use crate::events::SalienceKind;

/// A cap that is currently halved because the response rate is below floor.
#[derive(Debug, Clone, PartialEq)]
pub struct Tightening {
    pub kind: SalienceKind,
    pub base_cap: u32,
    pub tightened_cap: u32,
    pub response_rate: f64,
    pub floor: f64,
    pub window: usize,
}

/// Rolling response rate for `kind` over the last `window` messages.
///
/// `None` until a full window exists. A rate computed from three messages is
/// noise, and halving a cap on a coin flip would make the control less
/// trustworthy than no control — the 20-message window *is* the handling of
/// the signal's weakness.
#[must_use]
pub fn response_rate(store: &OutreachStore, kind: SalienceKind, window: usize) -> Option<f64> {
    if window == 0 {
        return None;
    }
    let recent = store.recent(kind, window);
    if recent.len() < window {
        return None;
    }
    let responded = recent.iter().filter(|m| m.responded()).count();
    Some(responded as f64 / recent.len() as f64)
}

/// The tightening in force for `kind`, if any.
///
/// This is a pure function of the current window, not a ratchet: if D starts
/// responding again the cap comes back on its own. Neglect still holds it
/// down, because neglect keeps the rate below the floor.
#[must_use]
pub fn tightening(
    store: &OutreachStore,
    config: &OutreachConfig,
    kind: SalienceKind,
) -> Option<Tightening> {
    let base_cap = config.cap_for(kind)?;
    let rate = response_rate(store, kind, config.feedback_window)?;
    if rate >= config.response_rate_floor {
        return None;
    }
    Some(Tightening {
        kind,
        base_cap,
        // Integer halving, floor included: a `Development` cap of 1 halves to
        // 0 and the kind goes silent. That is the spec's instruction taken
        // literally, and it is survivable only because it is announced — D
        // sees the notice and can retune. A `.max(1)` here would be a gate
        // that never fully fires, which is the failure §2.4 is guarding.
        tightened_cap: base_cap / 2,
        response_rate: rate,
        floor: config.response_rate_floor,
        window: config.feedback_window,
    })
}

/// The daily cap actually in force for `kind`. `None` is uncapped.
#[must_use]
pub fn effective_cap(
    store: &OutreachStore,
    config: &OutreachConfig,
    kind: SalienceKind,
) -> Option<u32> {
    let base_cap = config.cap_for(kind)?;
    Some(tightening(store, config, kind).map_or(base_cap, |t| t.tightened_cap))
}

/// Render the notice D gets when a cap tightens.
///
/// Plain and specific on purpose: the number that moved, the rate that moved
/// it, and what silence now means. A notice D cannot act on is decoration.
#[must_use]
pub fn tightening_notice(tightening: &Tightening) -> String {
    let consequence = if tightening.tightened_cap == 0 {
        format!(
            "No further `{}` outreach will be sent until this changes.",
            tightening.kind
        )
    } else {
        format!(
            "At most {} `{}` message(s) a day from now on.",
            tightening.tightened_cap, tightening.kind
        )
    };
    format!(
        "**Outreach cap tightened — {kind}**\n\
         Response rate over the last {window} `{kind}` messages: {rate:.0}% \
         (floor {floor:.0}%).\n\
         Daily cap halved: {base} → {tightened}. {consequence}\n\
         If these were useful and I simply did not see a reply, mark them with \
         `pulse-null outreach respond <id> --rating useful` and the cap restores itself.",
        kind = tightening.kind,
        window = tightening.window,
        rate = tightening.response_rate * 100.0,
        floor = tightening.floor * 100.0,
        base = tightening.base_cap,
        tightened = tightening.tightened_cap,
        consequence = consequence,
    )
}

/// Record that D has been told about a tightening.
///
/// Called only after the notice is confirmed delivered. Recording it before
/// delivery would let a failed webhook produce exactly the silent throttling
/// §2.4 forbids: the cap halved, the record saying D knows, and D not knowing.
pub async fn mark_announced(
    root_dir: &Path,
    tightening: &Tightening,
    at: DateTime<Utc>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (kind, cap) = (tightening.kind, tightening.tightened_cap);
    store::save_delta_async(root_dir.to_path_buf(), move |s| {
        s.record_announcement(kind, cap, at);
    })
    .await
}

/// Record D's response to an outreach message.
///
/// Returns `Ok(false)` when the id is unknown — a typo must not look like a
/// recorded response.
pub fn record_response(
    root_dir: &Path,
    id: &str,
    rating: Option<Rating>,
    at: DateTime<Utc>,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    store::save_delta(root_dir, |s| s.mark_responded(id, at, rating))
}

/// Per-kind reporting line for `outreach status`.
#[derive(Debug, Clone, PartialEq)]
pub struct KindStatus {
    pub kind: SalienceKind,
    pub base_cap: Option<u32>,
    pub effective_cap: Option<u32>,
    pub sent_today: u32,
    pub response_rate: Option<f64>,
    pub window_size: usize,
    pub tightened: bool,
    pub announced: bool,
}

/// Summarize one kind's budget and standing.
#[must_use]
pub fn kind_status(
    store: &OutreachStore,
    config: &OutreachConfig,
    kind: SalienceKind,
    timezone: &str,
    now: DateTime<Utc>,
) -> KindStatus {
    let tz = super::resolve_timezone(timezone);
    let tightening = tightening(store, config, kind);
    KindStatus {
        kind,
        base_cap: config.cap_for(kind),
        effective_cap: effective_cap(store, config, kind),
        sent_today: store.sent_today(kind, tz, now),
        response_rate: response_rate(store, kind, config.feedback_window),
        window_size: store.recent(kind, config.feedback_window).len(),
        tightened: tightening.is_some(),
        announced: store.announced(kind).is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outreach::test_support::sent;
    use chrono::Duration;

    /// A store with `count` messages of `kind`, `responded` of them answered.
    fn store_with(kind: SalienceKind, count: usize, responded: usize) -> OutreachStore {
        let base: DateTime<Utc> = "2026-08-02T09:00:00Z".parse().unwrap();
        let mut store = OutreachStore::default();
        for i in 0..count {
            store.record_sent(sent(
                &format!("m{i}"),
                kind,
                base + Duration::minutes(i as i64),
                i < responded,
            ));
        }
        store
    }

    fn config() -> OutreachConfig {
        OutreachConfig {
            feedback_window: 4,
            ..OutreachConfig::default()
        }
    }

    #[test]
    fn rate_is_none_until_the_window_is_full() {
        let config = config();
        let store = store_with(SalienceKind::Finding, 3, 3);
        assert_eq!(
            response_rate(&store, SalienceKind::Finding, config.feedback_window),
            None
        );
        // A partial window must not tighten anything either.
        assert!(tightening(&store, &config, SalienceKind::Finding).is_none());
        assert_eq!(
            effective_cap(&store, &config, SalienceKind::Finding),
            Some(config.cap_finding)
        );
    }

    #[test]
    fn rate_counts_only_the_newest_window() {
        let config = config();
        // 8 messages, the first 4 answered and the last 4 not: the window
        // sees only the recent silence.
        let store = store_with(SalienceKind::Finding, 8, 4);
        assert_eq!(
            response_rate(&store, SalienceKind::Finding, config.feedback_window),
            Some(0.0)
        );
    }

    #[test]
    fn rate_is_per_kind() {
        let config = config();
        let mut store = store_with(SalienceKind::Finding, 4, 0);
        for i in 0..4 {
            store.record_sent(sent(
                &format!("c{i}"),
                SalienceKind::Callback,
                Utc::now(),
                true,
            ));
        }
        assert_eq!(
            response_rate(&store, SalienceKind::Finding, config.feedback_window),
            Some(0.0)
        );
        assert_eq!(
            response_rate(&store, SalienceKind::Callback, config.feedback_window),
            Some(1.0)
        );
    }

    #[test]
    fn a_rate_below_the_floor_halves_the_cap() {
        let config = config();
        // 1 of 4 answered = 0.25, below the 0.3 floor.
        let store = store_with(SalienceKind::Finding, 4, 1);
        let tightening = tightening(&store, &config, SalienceKind::Finding).unwrap();
        assert_eq!(tightening.base_cap, 2);
        assert_eq!(tightening.tightened_cap, 1);
        assert_eq!(
            effective_cap(&store, &config, SalienceKind::Finding),
            Some(1)
        );
    }

    #[test]
    fn a_rate_at_the_floor_does_not_tighten() {
        let config = OutreachConfig {
            feedback_window: 10,
            ..config()
        };
        // 3 of 10 = exactly the floor. "Below this" is strict.
        let store = store_with(SalienceKind::Finding, 10, 3);
        assert!(tightening(&store, &config, SalienceKind::Finding).is_none());
    }

    #[test]
    fn development_halves_to_zero_and_says_so() {
        let config = config();
        let store = store_with(SalienceKind::Development, 4, 0);
        let tightening = tightening(&store, &config, SalienceKind::Development).unwrap();
        assert_eq!(tightening.base_cap, 1);
        assert_eq!(tightening.tightened_cap, 0);

        let notice = tightening_notice(&tightening);
        assert!(notice.contains("1 → 0"), "{notice}");
        assert!(
            notice.contains("No further `development` outreach"),
            "a cap of zero must state that the kind is silenced: {notice}"
        );
    }

    #[test]
    fn blocking_is_never_tightened_because_it_is_never_capped() {
        let config = config();
        let store = store_with(SalienceKind::Blocking, 20, 0);
        assert!(tightening(&store, &config, SalienceKind::Blocking).is_none());
        assert_eq!(effective_cap(&store, &config, SalienceKind::Blocking), None);
    }

    #[test]
    fn neglect_tightens_rather_than_loosens() {
        // Nobody responds to anything: every capped kind ends up halved.
        let config = config();
        for kind in [
            SalienceKind::Finding,
            SalienceKind::Development,
            SalienceKind::Callback,
        ] {
            let store = store_with(kind, 4, 0);
            let base = config.cap_for(kind).unwrap();
            assert_eq!(
                effective_cap(&store, &config, kind),
                Some(base / 2),
                "{kind} must tighten under neglect"
            );
        }
    }

    #[test]
    fn the_cap_restores_when_responses_return() {
        let config = config();
        let mut store = store_with(SalienceKind::Finding, 4, 0);
        assert_eq!(
            effective_cap(&store, &config, SalienceKind::Finding),
            Some(1)
        );

        // Four fresh answered messages push the silent ones out of the window.
        for i in 0..4 {
            store.record_sent(sent(
                &format!("new{i}"),
                SalienceKind::Finding,
                Utc::now(),
                true,
            ));
        }
        assert_eq!(
            effective_cap(&store, &config, SalienceKind::Finding),
            Some(2)
        );
    }

    #[test]
    fn the_notice_names_the_rate_the_floor_and_the_movement() {
        let config = config();
        let store = store_with(SalienceKind::Finding, 4, 1);
        let notice =
            tightening_notice(&tightening(&store, &config, SalienceKind::Finding).unwrap());
        assert!(notice.contains("25%"), "{notice}");
        assert!(notice.contains("30%"), "{notice}");
        assert!(notice.contains("2 → 1"), "{notice}");
    }

    #[test]
    fn recording_a_response_lifts_the_rate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = config();
        let store = store_with(SalienceKind::Finding, 4, 0);
        store::save(tmp.path(), &store).unwrap();

        assert!(record_response(tmp.path(), "m0", Some(Rating::Useful), Utc::now()).unwrap());
        assert!(record_response(tmp.path(), "m1", None, Utc::now()).unwrap());
        assert!(!record_response(tmp.path(), "missing", None, Utc::now()).unwrap());

        let reloaded = store::load(tmp.path());
        assert_eq!(
            response_rate(&reloaded, SalienceKind::Finding, config.feedback_window),
            Some(0.5)
        );
        assert!(tightening(&reloaded, &config, SalienceKind::Finding).is_none());
    }

    #[test]
    fn status_reports_cap_movement_and_window_fill() {
        let config = config();
        let now: DateTime<Utc> = "2026-08-02T12:00:00Z".parse().unwrap();
        let store = store_with(SalienceKind::Finding, 4, 0);
        let status = kind_status(&store, &config, SalienceKind::Finding, "UTC", now);

        assert_eq!(status.base_cap, Some(2));
        assert_eq!(status.effective_cap, Some(1));
        assert!(status.tightened);
        assert!(!status.announced, "nothing has been announced yet");
        assert_eq!(status.sent_today, 4);
        assert_eq!(status.window_size, 4);
    }
}
