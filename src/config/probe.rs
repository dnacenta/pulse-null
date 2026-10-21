//! `[probe]` — the growth-record probe runner (PN-144). All values are
//! INITIAL GUESSES (Concept v2 §5 style) until the Phase 0a measurement
//! confirms what a run costs.

use serde::{Deserialize, Serialize};

/// How often scheduled probe runs are due. The days themselves are fixed in
/// code so they cannot be tuned after seeing results (pre-registration).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeCadence {
    /// Max-class plans: Monday, Wednesday and Friday.
    Max,
    /// Pro-class plans: every 14 days.
    Pro,
}

/// `[probe]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProbeConfig {
    /// Off until the owner turns it on.
    pub enabled: bool,
    pub cadence: ProbeCadence,
    /// Start of the quiet window, `HH:MM` in `[scheduler] timezone`.
    pub quiet_start: String,
    /// End of the quiet window (exclusive). May be earlier than the start
    /// (the window wraps midnight); equal means no window at all.
    pub quiet_end: String,
    /// Model turns all probe runs may spend per week (Monday to Sunday).
    pub weekly_turns: u32,
    /// Model turns one run may spend.
    pub max_turns_per_run: u32,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cadence: ProbeCadence::Max,
            quiet_start: "02:00".to_string(),
            quiet_end: "06:00".to_string(),
            // Three max-cadence runs of up to 27 turns each: every item set
            // (15) and three due paired facts of four asks (12).
            weekly_turns: 81,
            max_turns_per_run: 27,
        }
    }
}

impl ProbeConfig {
    /// The quiet window as minutes after local midnight.
    pub fn quiet_window(&self) -> Result<(u32, u32), String> {
        Ok((
            minutes_of_day(&self.quiet_start)?,
            minutes_of_day(&self.quiet_end)?,
        ))
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let (start, end) = self.quiet_window()?;
        if start == end {
            return Err(
                "[probe] quiet_start equals quiet_end: probes run only inside a quiet window"
                    .into(),
            );
        }
        if self.max_turns_per_run == 0 || self.weekly_turns == 0 {
            return Err("[probe] weekly_turns and max_turns_per_run must be > 0".into());
        }
        if self.max_turns_per_run > self.weekly_turns {
            return Err("[probe] max_turns_per_run cannot exceed weekly_turns".into());
        }
        Ok(())
    }
}

/// `HH:MM` → minutes after midnight.
fn minutes_of_day(text: &str) -> Result<u32, String> {
    let bad = || format!("[probe] '{text}' is not a time of day (HH:MM)");
    let (hours, minutes) = text.trim().split_once(':').ok_or_else(bad)?;
    let hours: u32 = hours.parse().map_err(|_| bad())?;
    let minutes: u32 = minutes.parse().map_err(|_| bad())?;
    if hours > 23 || minutes > 59 {
        return Err(bad());
    }
    Ok(hours * 60 + minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_valid() {
        let config = ProbeConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.quiet_window().unwrap(), (120, 360));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn bad_windows_and_budgets_are_refused() {
        let mut config = ProbeConfig {
            quiet_start: "25:00".into(),
            ..ProbeConfig::default()
        };
        assert!(config.validate().is_err());
        config.quiet_start = "03:00".into();
        config.quiet_end = "03:00".into();
        assert!(config.validate().is_err());
        config.quiet_end = "02:00".into();
        assert!(config.validate().is_ok(), "a window may wrap midnight");
        config.max_turns_per_run = 100;
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_section_parses_from_toml() {
        let config: ProbeConfig =
            toml::from_str("enabled = true\ncadence = \"pro\"\nweekly_turns = 30\n").unwrap();
        assert!(config.enabled);
        assert_eq!(config.cadence, ProbeCadence::Pro);
        assert_eq!(config.weekly_turns, 30);
        assert_eq!(config.quiet_start, "02:00");
    }
}
