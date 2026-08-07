//! Per-failure task diagnostics: the `[task-error]` line.
//!
//! # Why this is not an alarm
//!
//! Every scheduled-task failure used to post a bold "⚠️ Provider Error" to the
//! same webhook as `[SHARE:]` output and the liveness `[ALERT]` / `[RESOLVED]`
//! alarm, with no dedupe and no backoff. A failing task therefore produced one
//! alarm-shaped message per cycle forever, which taught the operator to stop
//! reading the channel — and that is how a seven-week total outage hid in
//! plain sight. **An alarm only works if nothing else in the channel looks
//! like an alarm.**
//!
//! So this module keeps the information and drops the theatre:
//!
//! * **A separate destination.** `diagnostics_webhook` when set; otherwise the
//!   share webhook, in plain framing (see [`destination`]).
//! * **A separate voice.** Lowercase `[task-error]`, no emoji, no bold — it
//!   reads as a log line, because that is what it is. `[ALERT]` and
//!   `[RESOLVED]` stay reserved for [`super::health`].
//! * **State-awareness.** Only failures that are *news* are posted: the first
//!   of a streak, and any change of error text. See
//!   [`super::health::failure_is_news_for`].
//!
//! # Why it is not deleted
//!
//! These lines carry the only full error text at the moment of failure — the
//! alarm carries one truncated `last_error` on a six-hour ladder, which is
//! enough to know and not enough to diagnose. They are also a stateless
//! fallback for a misconfigured or wrong alarm, which is exactly the failure
//! mode that cost seven weeks.
//!
//! # Degradation
//!
//! The state-awareness reads [`super::health::TaskHealthStore`], which is only
//! written while `[scheduler.liveness] enabled = true`. With the alarm off the
//! store never advances, every failure reads as the first of a streak, and
//! every failure is posted — the pre-existing behaviour, which is the right
//! fallback when this is the only failure signal left.

use std::sync::Arc;

use crate::config::OutputConfig;
use crate::provider_status;
use crate::server::AppState;

use super::liveness::SharedTaskHealth;
use super::output;
use super::ScheduleEntry;

/// Longest error text carried into a webhook message.
///
/// Discord rejects bodies over 2000 characters; a diagnostic that 400s
/// delivers nothing at all. The untruncated text is always written to the
/// journal at `error` level, so nothing is actually lost.
const MAX_WEBHOOK_ERROR_LEN: usize = 1500;

/// Where a diagnostic goes.
///
/// Naming the fallback rather than folding it into an `Option` keeps the
/// distinction visible: a dedicated channel is the configuration this feature
/// exists to encourage, and sharing the alarm channel is a compromise the
/// operator should be able to see in the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination<'a> {
    /// `diagnostics_webhook` — a channel of its own.
    Dedicated(&'a str),
    /// No diagnostics webhook, so the share webhook carries it instead.
    ///
    /// Plain framing, deliberately: the point of the fallback is to keep the
    /// full error text reaching a human, not to add a second alarm shape to
    /// the channel that already holds one.
    SharedChannel(&'a str),
    /// No webhook at all — the journal is the only channel there is.
    LogOnly,
}

/// Resolve where diagnostics go for this configuration.
///
/// Falling back to the share webhook rather than going silent is deliberate:
/// an operator who upgrades and configures nothing would otherwise lose the
/// only full error text the system produces, which is the regression that cost
/// seven weeks. The volume problem the fallback used to cause is solved by
/// state-awareness instead — a 500-cycle streak posts once, not 500 times.
#[must_use]
pub fn destination(output: &OutputConfig) -> Destination<'_> {
    if let Some(url) = non_empty(output.diagnostics_webhook.as_deref()) {
        return Destination::Dedicated(url);
    }
    match non_empty(output.share_webhook.as_deref()) {
        Some(url) => Destination::SharedChannel(url),
        None => Destination::LogOnly,
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|url| !url.is_empty())
}

/// One rendered failure, ready for a channel a human reads.
#[derive(Debug, Clone, Copy)]
pub struct Diagnostic<'a> {
    pub task_id: &'a str,
    pub task_name: &'a str,
    /// Classified error kind (`auth`, `rate_limit`, …).
    pub error_kind: &'a str,
    pub error: &'a str,
    /// Model the task ran on — the effective one, override included. A task
    /// pinned to a model is usually pinned *because* of how a model behaves,
    /// so which one produced the failure is part of the failure.
    pub model: &'a str,
    /// Failures already recorded in this streak, *not* counting this one.
    pub prior_failures: u32,
}

impl Diagnostic<'_> {
    /// The message body: one identifying line, then the error.
    ///
    /// Deliberately unformatted. No emoji, no bold, no `[ALERT]` — anything
    /// that reads as an alarm here devalues the one in the same channel.
    #[must_use]
    pub fn render(&self) -> String {
        let name = if self.task_name.is_empty() {
            self.task_id
        } else {
            self.task_name
        };
        format!(
            "[task-error] {} ({}) — failure #{}, {}; kind={}; model={}\n{}",
            name,
            self.task_id,
            self.prior_failures + 1,
            self.reason(),
            self.error_kind,
            self.model,
            self.truncated_error(),
        )
    }

    /// Why this failure was worth a message. Derived, not stored: a failure is
    /// only reported when it is news, and news with no prior failures in the
    /// streak can only be the streak starting.
    fn reason(&self) -> &'static str {
        if self.prior_failures == 0 {
            "first of a new streak"
        } else {
            "new error text for this streak"
        }
    }

    fn truncated_error(&self) -> String {
        let kept = crate::utils::safe_truncate(self.error, MAX_WEBHOOK_ERROR_LEN);
        if kept.len() == self.error.len() {
            kept.to_string()
        } else {
            format!("{kept}… (truncated; full text in the journal)")
        }
    }
}

/// Report one task failure: always to the journal, to a channel only when the
/// failure is news.
///
/// Called before the outcome is recorded, so the health store still describes
/// the task as of the previous cycle — which is exactly the comparison
/// "is this new?" needs.
///
/// Never fails and never propagates: diagnostics cannot be allowed to take
/// down the task they are reporting on.
pub async fn report_failure(
    health: &SharedTaskHealth,
    state: &Arc<AppState>,
    entry: &ScheduleEntry,
    error: &str,
) {
    let task = &entry.task;
    let error_kind = provider_status::classify_error(error).to_string();
    let model = entry.effective_model(&state.config.llm.model);

    // The journal keeps everything, whatever the channel policy is.
    tracing::error!(
        task_id = %task.id,
        error_kind = %error_kind,
        model = %model,
        "Scheduled task '{}' failed: {}",
        task.name,
        error
    );

    if !state.config.autonomy.events.provider_error {
        return;
    }

    let (is_news, prior_failures) = {
        let store = health.read().await;
        (
            store.failure_is_news(&task.id, error),
            store.consecutive_failures(&task.id),
        )
    };
    if !is_news {
        tracing::debug!(
            task_id = %task.id,
            prior_failures,
            "Repeat of a known failure — journal only; the liveness alarm owns escalation"
        );
        return;
    }

    let diagnostic = Diagnostic {
        task_id: &task.id,
        task_name: &task.name,
        error_kind: &error_kind,
        error,
        model,
        prior_failures,
    };
    let message = diagnostic.render();

    match destination(&state.config.scheduler.output) {
        Destination::Dedicated(url) => output::deliver_diagnostic(&message, url).await,
        Destination::SharedChannel(url) => {
            tracing::debug!(
                task_id = %task.id,
                "No diagnostics_webhook configured — using the share webhook"
            );
            output::deliver_diagnostic(&message, url).await;
        }
        Destination::LogOnly => {
            tracing::warn!(task_id = %task.id, "No webhook configured for task diagnostics");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::health::{failure_is_news_for, TaskHealth, TaskHealthStore};
    use chrono::{DateTime, Duration, Utc};
    use tempfile::TempDir;

    fn output_config(diagnostics: Option<&str>, share: Option<&str>) -> OutputConfig {
        OutputConfig {
            share_webhook: share.map(str::to_string),
            call_endpoint: None,
            diagnostics_webhook: diagnostics.map(str::to_string),
        }
    }

    #[test]
    fn dedicated_webhook_wins() {
        let config = output_config(Some("https://diag"), Some("https://share"));
        assert_eq!(destination(&config), Destination::Dedicated("https://diag"));
    }

    #[test]
    fn falls_back_to_share_webhook_rather_than_going_silent() {
        let config = output_config(None, Some("https://share"));
        assert_eq!(
            destination(&config),
            Destination::SharedChannel("https://share")
        );
    }

    #[test]
    fn blank_webhooks_do_not_count_as_configured() {
        let config = output_config(Some("   "), Some(""));
        assert_eq!(destination(&config), Destination::LogOnly);
    }

    #[test]
    fn no_webhook_at_all_is_log_only() {
        assert_eq!(
            destination(&output_config(None, None)),
            Destination::LogOnly
        );
        assert_eq!(destination(&OutputConfig::default()), Destination::LogOnly);
    }

    /// The whole point: a diagnostic must not be mistakable for the alarm that
    /// shares its channel.
    #[test]
    fn rendered_message_carries_no_alarm_framing() {
        let message = Diagnostic {
            task_id: "thinking-loop",
            task_name: "Thinking Loop",
            error_kind: "auth",
            error: "401 Unauthorized: invalid x-api-key",
            model: "claude-opus-4-8",
            prior_failures: 0,
        }
        .render();

        assert!(message.starts_with("[task-error] "), "got: {message}");
        assert!(!message.contains("[ALERT]"), "got: {message}");
        assert!(!message.contains("[RESOLVED]"), "got: {message}");
        assert!(!message.contains('\u{26a0}'), "got: {message}");
        assert!(!message.contains("**"), "got: {message}");
    }

    #[test]
    fn rendered_message_states_the_failure_number_reason_and_model() {
        let first = Diagnostic {
            task_id: "thinking-loop",
            task_name: "Thinking Loop",
            error_kind: "auth",
            error: "401",
            model: "claude-opus-4-8",
            prior_failures: 0,
        }
        .render();
        assert!(
            first.contains("failure #1, first of a new streak"),
            "{first}"
        );
        assert!(first.contains("kind=auth"), "{first}");
        assert!(first.contains("model=claude-opus-4-8"), "{first}");
        assert!(first.contains("401"), "{first}");

        let changed = Diagnostic {
            task_id: "thinking-loop",
            task_name: "Thinking Loop",
            error_kind: "timeout",
            error: "deadline exceeded",
            model: "fable-5",
            prior_failures: 3,
        }
        .render();
        assert!(
            changed.contains("failure #4, new error text for this streak"),
            "{changed}"
        );
    }

    #[test]
    fn nameless_task_falls_back_to_its_id() {
        let message = Diagnostic {
            task_id: "thinking-loop",
            task_name: "",
            error_kind: "unknown",
            error: "boom",
            model: "fable-5",
            prior_failures: 0,
        }
        .render();
        assert!(message.starts_with("[task-error] thinking-loop (thinking-loop)"));
    }

    /// Discord rejects bodies over 2000 characters, and a rejected diagnostic
    /// delivers nothing at all.
    #[test]
    fn oversized_error_is_truncated_and_says_so() {
        let error = "x".repeat(10_000);
        let message = Diagnostic {
            task_id: "t",
            task_name: "T",
            error_kind: "unknown",
            error: &error,
            model: "fable-5",
            prior_failures: 0,
        }
        .render();
        assert!(message.len() < 2000, "message was {} bytes", message.len());
        assert!(message.contains("(truncated; full text in the journal)"));
    }

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::<Utc>::MIN_UTC + Duration::days(365 * 100) + Duration::minutes(minutes)
    }

    /// The routing rule, end to end over the store the alarm already keeps:
    /// post once per streak, again when the error changes, and again after a
    /// success re-arms it.
    #[test]
    fn news_only_once_per_streak_until_something_changes() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let (id, name) = ("thinking-loop", "Thinking Loop");

        // First failure of a streak: news.
        assert!(store.failure_is_news(id, "rate limited"));
        store.record_failure(id, name, "rate limited", at(0));

        // Same error again: not news — the alarm owns escalation from here.
        assert!(!store.failure_is_news(id, "rate limited"));
        store.record_failure(id, name, "rate limited", at(20));
        assert!(!store.failure_is_news(id, "rate limited"));

        // A different failure mode is news even mid-streak.
        assert!(store.failure_is_news(id, "401 unauthorized"));
        store.record_failure(id, name, "401 unauthorized", at(40));
        assert!(!store.failure_is_news(id, "401 unauthorized"));

        // ...and the old error becomes news again, because it is a change.
        assert!(store.failure_is_news(id, "rate limited"));

        // A success resets the streak, so the next failure re-arms.
        store.record_success(id, name, at(60));
        assert!(store.failure_is_news(id, "401 unauthorized"));
    }

    /// The failure number printed in the message counts the failure being
    /// reported, which has not been recorded yet.
    #[test]
    fn prior_failure_count_excludes_the_unrecorded_failure() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        assert_eq!(store.consecutive_failures("thinking-loop"), 0);

        store.record_failure("thinking-loop", "Thinking Loop", "boom", at(0));
        assert_eq!(store.consecutive_failures("thinking-loop"), 1);

        store.record_success("thinking-loop", "Thinking Loop", at(10));
        assert_eq!(store.consecutive_failures("thinking-loop"), 0);
    }

    /// An unknown task — first cycle after a restart, or liveness disabled so
    /// the store never advances — always reports.
    #[test]
    fn unknown_task_is_always_news() {
        assert!(failure_is_news_for(None, "anything"));
    }

    /// Two errors the store cannot tell apart must not be reported as
    /// different, or a long error with a varying tail would post every cycle.
    #[test]
    fn errors_differing_only_past_the_stored_length_are_not_news() {
        let long = "e".repeat(400);
        let health = TaskHealth {
            consecutive_failures: 2,
            last_error: Some(crate::utils::safe_truncate(&long, 300).to_string()),
            ..TaskHealth::default()
        };
        assert!(!failure_is_news_for(Some(&health), &format!("{long}-tail")));
    }
}
