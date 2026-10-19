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
/// IO errors other than NotFound count as isolated: the failure direction is
/// INTO the retreat, never silently out of it.
pub fn is_active(root_dir: &Path) -> bool {
    match std::fs::symlink_metadata(marker_path(root_dir)) {
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!("isolation marker unreadable ({e}); treating as ISOLATED");
            true
        }
    }
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
/// only: a guest must not be able to flip the pulse's operating posture.
pub fn intercept_command(
    root_dir: &Path,
    message: &str,
    resolved_key: &crate::session_key::SessionKey,
    sender_label: &str,
) -> Intercept {
    let Some(command) = Command::parse(message) else {
        return Intercept::None;
    };
    // Owner only — not guests, not peers (a sibling pulse must not be able
    // to /resume this one mid-diagnosis). The refusal conceals the posture.
    if !resolved_key.is_owner() {
        return Intercept::Handled {
            response: "Isolation commands are restricted to the owner.".to_string(),
            isolated: false,
        };
    }
    match command {
        Command::Enter { reason } => enter_on_command(root_dir, sender_label, reason),
        Command::Exit => exit_on_command(root_dir),
    }
}

/// An isolation command at the start of a chat message.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// `/isolate`, with the rest of the message as its reason (sanitized;
    /// `None` when empty).
    Enter { reason: Option<String> },
    /// `/resume`.
    Exit,
}

impl Command {
    /// The command the message's first whitespace token names, matched
    /// ignoring ASCII case; `None` for any other message.
    fn parse(message: &str) -> Option<Self> {
        let trimmed = message.trim();
        let first_token = trimmed.split_whitespace().next().unwrap_or("");
        if first_token.eq_ignore_ascii_case(ENTER_COMMAND) {
            let reason =
                Some(sanitize_field(trimmed[first_token.len()..].trim())).filter(|r| !r.is_empty());
            return Some(Self::Enter { reason });
        }
        first_token
            .eq_ignore_ascii_case(EXIT_COMMAND)
            .then_some(Self::Exit)
    }
}

/// The owner's `/isolate`: write the marker, or say why it failed and
/// whether the pulse is isolated anyway.
fn enter_on_command(root_dir: &Path, sender_label: &str, reason: Option<String>) -> Intercept {
    match enter(root_dir, &sanitize_field(sender_label), reason) {
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
}

/// The owner's `/resume`: remove the marker, with the explicit exit signal.
/// A marker that cannot be removed leaves the pulse isolated, and says so.
fn exit_on_command(root_dir: &Path) -> Intercept {
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

/// Apply the sticky banner to a normal (non-command) response while isolated.
pub fn banner_wrap(response: String) -> String {
    format!("{BANNER} {response}")
}

/// Marker fields are echoed to the operator's terminal — cap length and
/// strip control characters (ANSI injection via a crafted sender/reason).
fn sanitize_field(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_control())
        .take(256)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_key::SessionKey;

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
        let result = intercept_command(
            dir.path(),
            "/isolate",
            &SessionKey::guest("stranger"),
            "stranger",
        );
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
        match intercept_command(
            dir.path(),
            "/isolate suspect graph",
            &SessionKey::owner(),
            "D",
        ) {
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

        match intercept_command(dir.path(), "/resume", &SessionKey::owner(), "D") {
            Intercept::Handled { response, isolated } => {
                assert!(!isolated);
                assert_eq!(response, BACK_TO_NORMAL);
            }
            Intercept::None => panic!("should intercept"),
        }
        assert!(!is_active(dir.path()));
    }

    #[test]
    fn readonly_tool_set_has_no_writers() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::test_support::minimal_config();
        let names = crate::server::setup::register_readonly_tools(dir.path(), &config).names();
        assert!(!names.iter().any(|n| n.contains("write")), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("fetch")), "{names:?}");
    }

    #[test]
    fn commands_are_owner_only_and_conceal_posture() {
        let dir = tempfile::tempdir().unwrap();
        enter(dir.path(), "D", None).unwrap();
        for key in [SessionKey::guest("x"), SessionKey::peer("echo2")] {
            match intercept_command(dir.path(), "/resume", &key, "x") {
                Intercept::Handled { response, isolated } => {
                    assert!(response.contains("owner"));
                    assert!(!isolated, "posture leaked to {key}");
                }
                Intercept::None => panic!("should intercept"),
            }
            assert!(is_active(dir.path()), "{key} flipped isolation");
        }
    }

    #[test]
    fn command_matching_is_tokenized_and_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        match intercept_command(
            dir.path(),
            "  /ISOLATE   graph acting weird  ",
            &SessionKey::owner(),
            "D",
        ) {
            Intercept::Handled { isolated, .. } => assert!(isolated),
            Intercept::None => panic!("should intercept"),
        }
        assert_eq!(
            status(dir.path()).unwrap().reason.as_deref(),
            Some("graph acting weird")
        );
        match intercept_command(dir.path(), "/Resume", &SessionKey::owner(), "D") {
            Intercept::Handled { isolated, .. } => assert!(!isolated),
            Intercept::None => panic!("should intercept"),
        }
        assert!(!is_active(dir.path()));
    }

    #[test]
    fn marker_fields_are_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let long_reason = "x".repeat(5000);
        let msg = format!("/isolate \u{1b}[31mred\u{1b}[0m {long_reason}");
        match intercept_command(dir.path(), &msg, &SessionKey::owner(), "evil\u{7}bell") {
            Intercept::Handled { isolated, .. } => assert!(isolated),
            Intercept::None => panic!("should intercept"),
        }
        let marker = status(dir.path()).unwrap();
        assert!(marker.reason.as_ref().unwrap().len() <= 256 * 4);
        assert!(!marker.reason.as_ref().unwrap().contains('\u{1b}'));
        assert!(!marker.by.contains('\u{7}'));
    }

    #[test]
    fn ordinary_messages_pass_through() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            intercept_command(
                dir.path(),
                "tell me about /isolate",
                &SessionKey::owner(),
                "D"
            ),
            Intercept::None
        ));
        assert!(matches!(
            intercept_command(dir.path(), "hello", &SessionKey::owner(), "D"),
            Intercept::None
        ));
    }

    fn handled(outcome: Intercept) -> (String, bool) {
        match outcome {
            Intercept::Handled { response, isolated } => (response, isolated),
            Intercept::None => panic!("should intercept"),
        }
    }

    /// The paths the tests above leave out: near-miss commands, a peer's
    /// /isolate, an /isolate with no reason and a second one, /resume when
    /// not isolated, and a marker that cannot be written or removed.
    #[test]
    fn every_path_of_the_isolation_gate() {
        let owner = SessionKey::owner();
        let dir = tempfile::tempdir().unwrap();
        for message in [
            "",
            "   ",
            "/isolated",
            "/resumes now",
            "please /resume",
            "isolate",
        ] {
            assert!(
                matches!(
                    intercept_command(dir.path(), message, &owner, "D"),
                    Intercept::None
                ),
                "{message:?}"
            );
        }

        let (response, isolated) = handled(intercept_command(
            dir.path(),
            "/isolate",
            &SessionKey::peer("Synth"),
            "Synth",
        ));
        assert_eq!(response, "Isolation commands are restricted to the owner.");
        assert!(!isolated && !is_active(dir.path()));

        let (response, isolated) = handled(intercept_command(dir.path(), "/isolate", &owner, "D"));
        assert!(isolated);
        assert!(
            response.starts_with("[ISOLATION] Isolation mode ACTIVE (entered "),
            "{response}"
        );
        assert!(response.contains(" by D)"), "{response}");
        assert_eq!(status(dir.path()).unwrap().reason, None);

        let (response, isolated) = handled(intercept_command(
            dir.path(),
            "/isolate again",
            &owner,
            "other",
        ));
        assert!(isolated);
        assert!(
            response.contains(" by D)"),
            "the first entry stands: {response}"
        );

        let (response, isolated) = handled(intercept_command(dir.path(), "/resume", &owner, "D"));
        assert_eq!((response.as_str(), isolated), (BACK_TO_NORMAL, false));
        let (response, isolated) = handled(intercept_command(dir.path(), "/resume", &owner, "D"));
        assert_eq!(
            (response.as_str(), isolated),
            ("Not in isolation mode — nothing to resume.", false)
        );

        // A marker that cannot be written: not isolated, and it says why.
        let missing = dir.path().join("no-such-dir");
        let (response, isolated) = handled(intercept_command(&missing, "/isolate", &owner, "D"));
        assert!(
            response.starts_with("Failed to enter isolation mode: "),
            "{response}"
        );
        assert!(!isolated);

        // A marker that is a directory can be neither written nor removed:
        // the pulse counts as isolated, and both commands say so.
        let stuck = tempfile::tempdir().unwrap();
        std::fs::create_dir(stuck.path().join(MARKER_FILE)).unwrap();
        let (response, isolated) =
            handled(intercept_command(stuck.path(), "/isolate", &owner, "D"));
        assert!(
            response.starts_with("Failed to enter isolation mode: "),
            "{response}"
        );
        assert!(isolated);
        let (response, isolated) = handled(intercept_command(stuck.path(), "/resume", &owner, "D"));
        assert!(
            response.starts_with("[ISOLATION] Failed to exit isolation mode: ")
                && response.ends_with(" — still isolated."),
            "{response}"
        );
        assert!(isolated);
    }

    #[test]
    fn a_command_is_its_first_token_and_isolate_takes_the_rest_as_reason() {
        assert_eq!(Command::parse("hello"), None);
        assert_eq!(Command::parse("/RESUME please"), Some(Command::Exit));
        assert_eq!(
            Command::parse(" /isolate "),
            Some(Command::Enter { reason: None })
        );
        assert_eq!(
            Command::parse("/Isolate  graph \u{1b}lies "),
            Some(Command::Enter {
                reason: Some("graph lies".to_string())
            })
        );
    }
}
