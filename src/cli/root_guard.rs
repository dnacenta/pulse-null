//! Refuse to create or run an entity as root.
//!
//! Two things go wrong under root and neither is recoverable later without
//! hand-fixing: every file the entity writes is root-owned, so the unix user
//! meant to run it cannot touch its own memory; and agent CLIs refuse
//! `--dangerously-skip-permissions` under root, so a claude-code entity can
//! never talk to its provider. Seen live 2026-09-03 (PN-104). Checked before
//! any filesystem work so a mistake leaves nothing behind.

/// Environment variable that lifts the refusal — for CI and smoke runs that
/// know what they are doing.
pub const ALLOW_ROOT_ENV: &str = "PULSE_NULL_ALLOW_ROOT";

/// Refuse `command` when running as root, unless [`ALLOW_ROOT_ENV`] is set.
pub fn refuse_root(command: &str) -> Result<(), String> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let allow = std::env::var_os(ALLOW_ROOT_ENV).is_some_and(|v| !v.is_empty());
    root_decision(command, euid, allow)
}

/// The pure decision, separated so it can be tested without changing uid.
fn root_decision(command: &str, euid: u32, allow: bool) -> Result<(), String> {
    if euid != 0 || allow {
        return Ok(());
    }
    Err(format!(
        "`pulse-null {command}` refuses to run as root.\n\
         \n\
         An entity created or started by root ends up root-owned, so the user meant to run it\n\
         cannot write its own memory; and agent CLIs refuse to skip permission prompts under\n\
         root, so a cli entity can never reach its provider.\n\
         \n\
         Run it as the entity's user instead, e.g. `sudo -u pulse -H pulse-null {command}`.\n\
         Set {ALLOW_ROOT_ENV}=1 to override (CI, smoke tests)."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_decision_refuses_euid_zero() {
        let err = root_decision("init", 0, false).unwrap_err();
        assert!(err.contains("refuses to run as root"));
        assert!(err.contains("refuse to skip permission prompts"));
        assert!(err.contains(ALLOW_ROOT_ENV));
    }

    #[test]
    fn root_decision_allows_override() {
        assert!(root_decision("up", 0, true).is_ok());
    }

    #[test]
    fn root_decision_allows_non_root() {
        assert!(root_decision("up", 1001, false).is_ok());
    }
}
