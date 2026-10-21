//! The input-only arm's facts (PN-144): the runtime facts Phase 1's
//! `<situation>` block will carry, rendered into a fixed, versioned
//! template (`assets/probes/v1/facts.md`). The arm is the same CLI and model
//! with this block as its only system prompt. The v1 items ask for facts the
//! block states, so this arm sits at the ceiling: a check of the scorer and
//! of the facts, not a baseline the pulse could beat.

use super::sets::{TruthKey, FACTS_V1};
use super::truth::Truths;

/// What a missing fact reads as.
const UNKNOWN: &str = "unknown";

/// Render the v1 facts block from `truths`.
#[must_use]
pub fn render(truths: &Truths, timezone: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let value = |key: TruthKey| {
        truths
            .get(key)
            .map(|t| t.value.clone())
            .unwrap_or_else(|| UNKNOWN.to_string())
    };
    let time = |key: TruthKey| {
        truths
            .local_time(key)
            .unwrap_or_else(|| UNKNOWN.to_string())
    };
    let fields = [
        ("{pulse_name}", value(TruthKey::PulseName)),
        ("{owner_name}", value(TruthKey::OwnerName)),
        ("{adapter}", value(TruthKey::Adapter)),
        ("{configured_model}", value(TruthKey::ConfiguredModel)),
        ("{now}", time(TruthKey::Today)),
        ("{weekday}", value(TruthKey::Weekday)),
        ("{timezone}", timezone.to_string()),
        ("{owner_last_active}", time(TruthKey::OwnerLastActive)),
        (
            "{owner_away}",
            truths
                .elapsed_since(TruthKey::OwnerLastActive, now)
                .unwrap_or_else(|| UNKNOWN.to_string()),
        ),
        ("{runtime_started}", time(TruthKey::RuntimeStarted)),
        (
            "{restarted}",
            match truths
                .get(TruthKey::RestartedSinceContact)
                .map(|t| t.value.as_str())
            {
                Some("RESTARTED") => "yes".to_string(),
                Some(_) => "no".to_string(),
                None => UNKNOWN.to_string(),
            },
        ),
        ("{last_task_name}", value(TruthKey::LastTaskName)),
        ("{last_task_at}", time(TruthKey::LastTaskDate)),
    ];
    let mut text = FACTS_V1.to_string();
    for (placeholder, field) in fields {
        text = text.replace(placeholder, &field);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn every_placeholder_is_filled_and_missing_facts_say_unknown() {
        let root = tempfile::tempdir().unwrap();
        let tz: chrono_tz::Tz = "Europe/Madrid".parse().unwrap();
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 10, 16, 0, 30, 0)
            .unwrap();
        let truths = Truths::collect(
            root.path(),
            &crate::vigil::probe::truth::tests::fixture_config(),
            tz,
            now,
        );
        let text = render(&truths, "Europe/Madrid", now);
        assert!(!text.contains('{'), "{text}");
        assert!(text.contains("name: Fixture"));
        assert!(text.contains("now: 2026-10-16 02:30 (Friday, Europe/Madrid)"));
        assert!(text.contains("owner last wrote: unknown (unknown ago)"));
        assert!(text.contains("restarted since the owner last wrote: unknown"));
        assert!(text.starts_with("<situation>"));
    }
}
