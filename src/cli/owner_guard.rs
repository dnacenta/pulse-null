//! Refuse a command that acts on a pulse unless the caller owns its folder.
//!
//! Root is still refused, unless `PULSE_NULL_ALLOW_ROOT` is set, and the
//! hint names the folder's owner. A name or an argument that is not a safe
//! token is quoted, or replaced, so the hint cannot be pasted as an
//! injection.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::root_guard::ALLOW_ROOT_ENV;

/// A refusal to print, and the status to exit with.
#[derive(Debug)]
pub struct Refusal {
    pub message: String,
    pub code: i32,
}

/// Whether `name` (and `action`, when the command has one) acts on a pulse.
///
/// `None` means the command is not classified. `graph provision` is guarded.
#[must_use]
pub fn command_acts_on_a_pulse(name: &str, action: Option<&str>) -> Option<bool> {
    match name {
        "init" | "chat" | "repair" | "up" | "schedule" | "pipeline" | "archive" | "plugin"
        | "intent" | "outreach" | "recall" | "praxis" | "vigil" => Some(true),
        "down" | "status" | "receipts" | "probe" => Some(false),
        "isolate" => match action {
            Some("on" | "off") => Some(true),
            Some("status") | None => Some(false),
            Some(_) => None,
        },
        "graph" => match action {
            Some("provision") => Some(true),
            _ => Some(false),
        },
        _ => None,
    }
}

/// Refuse unless the caller owns `root`. A missing path is an error.
pub fn require_owner(root: &Path, argv: &[OsString]) -> Result<(), Refusal> {
    guard_path(root, argv, false)
}

/// Like [`require_owner`]. When `allow_missing` is set, a path that does not
/// exist yet is allowed for anyone who is not root: they will own what they
/// create. Root still needs the override.
pub(crate) fn guard_path(
    path: &Path,
    argv: &[OsString],
    allow_missing: bool,
) -> Result<(), Refusal> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(Refusal {
            message: "refusing a symlink as the pulse folder".to_string(),
            code: 1,
        }),
        Ok(meta) => {
            let uid = meta.uid();
            let name = user_name(uid);
            decide(uid, name.as_deref(), current_euid(), root_allowed(), argv)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && allow_missing => {
            if current_euid() == 0 && !root_allowed() {
                Err(Refusal {
                    message: refusal_message(None, argv, true),
                    code: 1,
                })
            } else {
                Ok(())
            }
        }
        Err(err) => Err(Refusal {
            message: format!("cannot read the pulse folder: {err}"),
            code: 1,
        }),
    }
}

/// The pure decision. `owner_name` is only shown when it is a safe token.
fn decide(
    owner_uid: u32,
    owner_name: Option<&str>,
    euid: u32,
    allow_root: bool,
    argv: &[OsString],
) -> Result<(), Refusal> {
    if euid == owner_uid || (euid == 0 && allow_root) {
        return Ok(());
    }
    let root = euid == 0;
    Err(Refusal {
        message: refusal_message(owner_name, argv, root),
        code: if root { 1 } else { 2 },
    })
}

fn refusal_message(owner_name: Option<&str>, argv: &[OsString], root: bool) -> String {
    let who = user_label(owner_name);
    let hint = sudo_hint(owner_name, argv);
    if root {
        format!(
            "refuses to run as root. This pulse belongs to {who}.\n\
             {hint}\n\
             Set {ALLOW_ROOT_ENV}=1 to override."
        )
    } else {
        format!("this pulse belongs to {who}\n{hint}")
    }
}

fn sudo_hint(owner_name: Option<&str>, argv: &[OsString]) -> String {
    let mut line = format!("sudo -u {} -H pulse-null", user_shell(owner_name));
    for arg in argv {
        line.push(' ');
        line.push_str(&shell_quote(arg));
    }
    line
}

/// A name a person can read. Anything else becomes the placeholder.
fn user_label(name: Option<&str>) -> String {
    match name {
        Some(name) if safe_user(name) => name.to_string(),
        _ => "<pulse-user>".to_string(),
    }
}

/// The same name, safe to place after `sudo -u`.
fn user_shell(name: Option<&str>) -> String {
    let label = user_label(name);
    if safe_user(&label) {
        label
    } else {
        shell_quote(OsStr::new(&label))
    }
}

fn safe_user(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && name.len() <= 32
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn shell_quote(arg: &OsStr) -> String {
    let text = arg.to_string_lossy();
    if !text.is_empty() && text.chars().all(is_shell_safe) {
        return text.into_owned();
    }
    let mut quoted = String::from("'");
    for ch in text.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn is_shell_safe(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '@' | ',' | '=')
}

fn root_allowed() -> bool {
    std::env::var_os(ALLOW_ROOT_ENV).is_some_and(|value| !value.is_empty())
}

fn current_euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn user_name(uid: u32) -> Option<String> {
    let mut cap = 16 * 1024;
    loop {
        let mut buf = vec![0u8; cap];
        let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: getpwuid_r writes the entry into `pwd` and `buf`, and sets
        // `result` to `pwd` or null. The name is copied before `buf` drops.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            cap = cap.saturating_mul(2);
            if cap > 1024 * 1024 {
                return None;
            }
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        // SAFETY: `result` points at `pwd`, which getpwuid_r initialized.
        let pwd = unsafe { &*result };
        if pwd.pw_name.is_null() {
            return None;
        }
        // SAFETY: `pw_name` is a NUL-terminated string in `buf`.
        let cstr = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
        return cstr.to_str().ok().map(str::to_string);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn entries(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn owner_guard_refuses_another_uid_with_sudo_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep"), b"x").unwrap();
        let before = entries(dir.path());
        let owner = dir.path().metadata().unwrap().uid();
        let other = if owner == 1001 { 1002 } else { 1001 };
        let err = decide(
            owner,
            Some("alice"),
            other,
            false,
            &argv(&["isolate", "on"]),
        )
        .unwrap_err();
        assert_eq!(err.code, 2);
        assert!(
            err.message
                .lines()
                .any(|line| line == "sudo -u alice -H pulse-null isolate on"),
            "{:?}",
            err.message
        );
        assert!(err.message.contains("alice"));
        assert_eq!(entries(dir.path()), before);
        assert_eq!(std::fs::read(dir.path().join("keep")).unwrap(), b"x");
    }

    #[test]
    fn owner_guard_allows_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let before = entries(dir.path());
        let euid = current_euid();
        assert!(decide(euid, Some("alice"), euid, false, &argv(&["isolate", "on"])).is_ok());
        assert!(require_owner(dir.path(), &argv(&["chat"])).is_ok());
        let missing = dir.path().join("not-yet");
        assert!(guard_path(&missing, &argv(&["init"]), true).is_ok());
        assert!(!missing.exists());
        assert!(guard_path(&missing, &argv(&["init"]), false).is_err());
        assert_eq!(entries(dir.path()), before);
    }

    #[test]
    fn owner_guard_root_refused_without_override() {
        let args = argv(&["isolate", "on"]);
        let err = decide(1000, Some("alice"), 0, false, &args).unwrap_err();
        assert_eq!(err.code, 1);
        assert!(
            err.message.contains("refuses to run as root"),
            "{:?}",
            err.message
        );
        assert!(err.message.contains(ALLOW_ROOT_ENV));
        assert!(decide(1000, Some("alice"), 0, true, &args).is_ok());
        // The override lets root through. It does not let a different user through.
        assert!(decide(1000, Some("alice"), 1001, true, &args).is_err());
    }

    #[test]
    fn root_refusal_names_the_folder_owner() {
        let err = decide(1000, Some("pulse-alice"), 0, false, &argv(&["repair"])).unwrap_err();
        assert!(err.message.contains("pulse-alice"), "{:?}", err.message);
        assert!(
            err.message
                .contains("sudo -u pulse-alice -H pulse-null repair"),
            "{:?}",
            err.message
        );
        let hostile = decide(
            1000,
            Some("alice;touch /tmp/pwned"),
            0,
            false,
            &argv(&["repair"]),
        )
        .unwrap_err();
        assert!(!hostile.message.contains("touch"), "{:?}", hostile.message);
        assert!(
            hostile.message.contains("<pulse-user>"),
            "{:?}",
            hostile.message
        );
    }

    #[test]
    fn sudo_hint_quotes_arguments() {
        let err = decide(
            1,
            Some("alice"),
            2,
            false,
            &argv(&["isolate", "on", "--reason", "a b", "it's"]),
        )
        .unwrap_err();
        let line = "sudo -u alice -H pulse-null isolate on --reason 'a b' 'it'\\''s'";
        assert!(err.message.contains(line), "{:?}", err.message);
        let hostile = decide(
            1,
            Some("alice;touch /tmp/pwned"),
            2,
            false,
            &argv(&["isolate", "on;rm -rf /"]),
        )
        .unwrap_err();
        assert!(
            !hostile.message.contains("alice;touch"),
            "{:?}",
            hostile.message
        );
        assert!(
            hostile
                .message
                .contains("sudo -u '<pulse-user>' -H pulse-null isolate 'on;rm -rf /'"),
            "{:?}",
            hostile.message
        );
    }
}
