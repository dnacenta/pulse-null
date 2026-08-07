//! Isolation Mode — the data plane's deliberate retreat (coordinator spec,
//! Stage 2).
//!
//! A bisection tool, not a crash state: Echo sheds every subsystem that
//! might be lying to it and drops to a known-good minimal core — the
//! interactive channel, read-only introspection over journal/ and memory/,
//! and basic reasoning. Nothing that writes.
//!
//! Ownership (spec decision 5): the trigger and the banner belong to the
//! interactive-channel holder — this module lives in the server, NOT under
//! `src/coordinator/`, precisely so entering, seeing, and exiting isolation
//! never depend on the thing that might be wedged. State is a marker file
//! (`{root}/ISOLATION`), so it is sticky across restarts, writable with the
//! whole control plane dead, and visible to the CLI and a bare `ls` alike.
//!
//! The one limit, written down (spec): a clean bill in Isolation Mode means
//! "healthy in isolation", NOT "healthy" — some failures only manifest
//! under real load.

use std::io;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MARKER_FILE: &str = "ISOLATION";
pub const ENTER_COMMAND: &str = "/isolate";
pub const EXIT_COMMAND: &str = "/resume";
/// Prefixed to every response while isolated — a persistent state indicator,
/// not a one-shot notification (spec: sticky banner).
pub const BANNER: &str = "[ISOLATION]";
/// The explicit exit signal (spec: "a silent return is how you end up
/// debugging a system you *think* is isolated but isn't").
pub const BACK_TO_NORMAL: &str =
    "Back to normal — isolation mode exited; coordinator and subsystems will resume.";

/// Contents of the marker file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsolationMarker {
    pub entered_at: DateTime<Utc>,
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn marker_path(root_dir: &Path) -> std::path::PathBuf {
    root_dir.join(MARKER_FILE)
}

/// Whether isolation mode is active. One stat — cheap enough per request.
pub fn is_active(root_dir: &Path) -> bool {
    marker_path(root_dir).exists()
}

/// Current marker, if isolated. A marker that exists but doesn't parse still
/// counts as isolated (fail toward the retreat, never silently out of it).
pub fn status(root_dir: &Path) -> Option<IsolationMarker> {
    let content = std::fs::read_to_string(marker_path(root_dir)).ok()?;
    Some(serde_json::from_str(&content).unwrap_or(IsolationMarker {
        entered_at: DateTime::<Utc>::MIN_UTC,
        by: "unknown (unparseable marker)".to_string(),
        reason: None,
    }))
}

/// Enter isolation. Idempotent-ish: an existing marker is left untouched
/// (the original entry time is the honest one).
pub fn enter(root_dir: &Path, by: &str, reason: Option<String>) -> io::Result<IsolationMarker> {
    if let Some(existing) = status(root_dir) {
        return Ok(existing);
    }
    let marker = IsolationMarker {
        entered_at: Utc::now(),
        by: by.to_string(),
        reason,
    };
    let content = serde_json::to_string_pretty(&marker)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = root_dir.join(format!(".{MARKER_FILE}.tmp.{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, marker_path(root_dir))?;
    Ok(marker)
}

/// Exit isolation. Returns whether we were isolated.
pub fn exit(root_dir: &Path) -> io::Result<bool> {
    match std::fs::remove_file(marker_path(root_dir)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Outcome of checking a chat message for isolation commands.
pub enum Intercept {
    /// Not an isolation command — proceed with the normal turn.
    None,
    /// An isolation command was handled; respond with this text (already
    /// banner-prefixed where appropriate) and do not run the LLM turn.
    Handled { response: String, isolated: bool },
}

/// Handle `/isolate` / `/resume` before anything else touches the turn —
/// including the provider, which may itself be the suspect. Trusted senders
/// only: a guest must not be able to flip the entity's operating posture.
pub fn intercept_command(
    root_dir: &Path,
    message: &str,
    resolved_key: &str,
    sender_label: &str,
) -> Intercept {
    let trimmed = message.trim();
    let (is_enter, is_exit) = (
        trimmed == ENTER_COMMAND || trimmed.starts_with(&format!("{ENTER_COMMAND} ")),
        trimmed == EXIT_COMMAND,
    );
    if !is_enter && !is_exit {
        return Intercept::None;
    }

    if resolved_key.starts_with("guest:") {
        return Intercept::Handled {
            response: "Isolation commands are restricted to trusted senders.".to_string(),
            isolated: is_active(root_dir),
        };
    }

    if is_enter {
        let reason = trimmed
            .strip_prefix(ENTER_COMMAND)
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(String::from);
        match enter(root_dir, sender_label, reason) {
            Ok(marker) => Intercept::Handled {
                response: format!(
                    "{BANNER} Isolation mode ACTIVE (entered {} by {}). \
                     Minimal core only: this channel, read-only introspection over \
                     journal/ and memory/, reasoning. Coordinator, scheduler, and all \
                     state writes are shed. This banner stays on every reply until \
                     {EXIT_COMMAND}. Remember: clean here means healthy IN ISOLATION, \
                     not healthy.",
                    marker.entered_at.format("%Y-%m-%d %H:%M:%SZ"),
                    marker.by
                ),
                isolated: true,
            },
            Err(e) => Intercept::Handled {
                response: format!("Failed to enter isolation mode: {e}"),
                isolated: is_active(root_dir),
            },
        }
    } else {
        match exit(root_dir) {
            Ok(true) => Intercept::Handled {
                response: BACK_TO_NORMAL.to_string(),
                isolated: false,
            },
            Ok(false) => Intercept::Handled {
                response: "Not in isolation mode — nothing to resume.".to_string(),
                isolated: false,
            },
            Err(e) => Intercept::Handled {
                response: format!("{BANNER} Failed to exit isolation mode: {e} — still isolated."),
                isolated: true,
            },
        }
    }
}

/// Apply the sticky banner to a normal (non-command) response while isolated.
pub fn banner_wrap(response: String) -> String {
    format!("{BANNER} {response}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_lifecycle_enter_status_exit() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_active(dir.path()));
        assert!(status(dir.path()).is_none());

        let marker = enter(dir.path(), "D", Some("suspect graph".into())).unwrap();
        assert!(is_active(dir.path()));
        assert_eq!(marker.by, "D");

        // Re-entry keeps the original marker (honest entry time).
        let again = enter(dir.path(), "someone-else", None).unwrap();
        assert_eq!(again.by, "D");

        assert!(exit(dir.path()).unwrap());
        assert!(!is_active(dir.path()));
        assert!(!exit(dir.path()).unwrap());
    }

    #[test]
    fn unparseable_marker_still_counts_as_isolated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MARKER_FILE), "not json at all").unwrap();
        assert!(is_active(dir.path()));
        let marker = status(dir.path()).unwrap();
        assert!(marker.by.contains("unparseable"));
    }

    #[test]
    fn guest_cannot_flip_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let result = intercept_command(dir.path(), "/isolate", "guest:stranger", "stranger");
        match result {
            Intercept::Handled { response, isolated } => {
                assert!(response.contains("restricted"));
                assert!(!isolated);
            }
            Intercept::None => panic!("command should be intercepted"),
        }
        assert!(!is_active(dir.path()));
    }

    #[test]
    fn trusted_sender_enters_and_exits_with_explicit_signals() {
        let dir = tempfile::tempdir().unwrap();
        match intercept_command(dir.path(), "/isolate suspect graph", "owner:D", "D") {
            Intercept::Handled { response, isolated } => {
                assert!(isolated);
                assert!(response.starts_with(BANNER));
            }
            Intercept::None => panic!("should intercept"),
        }
        assert!(is_active(dir.path()));
        assert_eq!(
            status(dir.path()).unwrap().reason.as_deref(),
            Some("suspect graph")
        );

        match intercept_command(dir.path(), "/resume", "owner:D", "D") {
            Intercept::Handled { response, isolated } => {
                assert!(!isolated);
                assert_eq!(response, BACK_TO_NORMAL);
            }
            Intercept::None => panic!("should intercept"),
        }
        assert!(!is_active(dir.path()));
    }

    #[test]
    fn ordinary_messages_pass_through() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            intercept_command(dir.path(), "tell me about /isolate", "owner:D", "D"),
            Intercept::None
        ));
        assert!(matches!(
            intercept_command(dir.path(), "hello", "owner:D", "D"),
            Intercept::None
        ));
    }
}
