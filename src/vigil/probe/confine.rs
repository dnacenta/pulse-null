//! Is this process confined the way the probe unit confines it? (PN-144
//! audit, loopback split.)
//!
//! Landlock holds a probe child to TCP ports 443 and its throwaway graph's,
//! but says nothing about addresses, and nothing about named Unix sockets.
//! The probe unit adds what Landlock cannot: `IPAddressDeny=` for the IPv4
//! loopback (where the live graph server and the host's nginx listen),
//! `InaccessiblePaths=` for the host sockets any user may connect to, and a
//! resolver that is not on 127.0.0.53. A `probe run` checks those from the
//! inside and refuses when they are missing, so a run by hand outside the
//! unit (or an equivalent `systemd-run`) never runs half-confined;
//! `verify-isolation` shows the same checks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::verify::Check;

/// Host sockets the probe unit makes inaccessible; one that accepts a
/// connection means the run is not under the unit.
const HOST_SOCKETS: &[&str] = &[
    "/run/dbus/system_bus_socket",
    "/run/tailscale/tailscaled.sock",
    "/run/snapd.socket",
    "/run/snapd-snap.socket",
    "/run/avahi-daemon/socket",
    "/run/uuidd/request",
];

/// A port on 127.0.0.1 to probe: the discard port, which nothing listens
/// on. Under the unit the connect fails with `EPERM` before any packet.
const LOOPBACK_PROBE: &str = "127.0.0.1:9";

/// The confinement checks, as `verify-isolation` shows them.
#[must_use]
pub fn checks() -> Vec<Check> {
    let mut checks = vec![loopback_check()];
    checks.extend(HOST_SOCKETS.iter().filter_map(|path| socket_check(path)));
    let resolv = crate::fs_safe::read_bounded(Path::new("/etc/resolv.conf"), 64 * 1024)
        .ok()
        .flatten()
        .unwrap_or_default();
    checks.push(resolver_check(&resolv));
    checks
}

/// `Ok` when the process is the probe user (not the live pulse), the probe
/// record is not readable by the pulse's uid, and every confinement check
/// passes. `record_dir` is `record/probes`.
pub fn require(expected_user: &str, pulse_uid: u32, record_dir: &Path) -> Result<(), String> {
    let euid = euid();
    let name = account(euid).map(|a| a.name).unwrap_or_default();
    let readable = record_readable_by(record_dir, pulse_uid)?;
    if let Some(reason) = identity_refusal(euid, &name, expected_user, pulse_uid, readable) {
        return Err(reason);
    }
    require_unit()
}

/// `Ok` when every unit confinement check passes; else why the run is refused.
pub fn require_unit() -> Result<(), String> {
    let failed: Vec<String> = checks()
        .into_iter()
        .filter(|c| !c.passed)
        .map(|c| format!("{} ({})", c.name, c.detail))
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "not confined like the probe unit: {}. Run it through \
             pulse-null-probe@<pulse>.service (or systemd-run with that unit's properties)",
            failed.join("; ")
        ))
    }
}

/// Why this process must not run a probe, or `None` when the uid split holds.
/// `record_readable` is whether `record/probes` is readable by `pulse_uid`.
#[must_use]
pub fn identity_refusal(
    euid: u32,
    euid_name: &str,
    expected_user: &str,
    pulse_uid: u32,
    record_readable: bool,
) -> Option<String> {
    if euid == pulse_uid {
        return Some(format!(
            "the probe runs as its own user ({expected_user}), not as the live pulse \
             (uid {pulse_uid})"
        ));
    }
    if euid_name != expected_user {
        return Some(format!(
            "running as {euid_name}, not the probe user '{expected_user}' ([probe] user)"
        ));
    }
    if record_readable {
        return Some(
            "the probe record is readable by the live pulse; it must be mode 0700 \
             and owned by the probe user"
                .into(),
        );
    }
    None
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and returns the effective uid.
    unsafe { libc::geteuid() }
}

/// A passwd entry. The home is the account's, copied out of the libc buffer.
struct Account {
    name: String,
    gid: u32,
    home: PathBuf,
}

/// The passwd entry for `uid`.
fn account(uid: u32) -> Result<Account, String> {
    let mut cap = 16 * 1024;
    loop {
        let mut buf = vec![0u8; cap];
        let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: getpwuid_r writes the entry into `pwd` and `buf`, and sets
        // `result` to `pwd` or null. The strings are copied before `buf` drops.
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
                return Err(format!("passwd entry for uid {uid} does not fit"));
            }
            continue;
        }
        if rc != 0 || result.is_null() {
            return Err(format!("no passwd entry for uid {uid}"));
        }
        // SAFETY: `result` points at `pwd`, which getpwuid_r has initialized.
        let pwd = unsafe { &*result };
        let name =
            cstr(pwd.pw_name).map_err(|_| format!("user name for uid {uid} is not utf-8"))?;
        let home = cstr(pwd.pw_dir).map_err(|_| format!("home for uid {uid} is not utf-8"))?;
        return Ok(Account {
            name,
            gid: pwd.pw_gid,
            home: PathBuf::from(home),
        });
    }
}

fn cstr(ptr: *const libc::c_char) -> Result<String, ()> {
    if ptr.is_null() {
        return Err(());
    }
    // SAFETY: `ptr` is a NUL-terminated string from a passwd field just read.
    let text = unsafe { std::ffi::CStr::from_ptr(ptr) };
    text.to_str().map(str::to_string).map_err(|_| ())
}

/// The home directory of `uid`.
pub(crate) fn home_of(uid: u32) -> Result<PathBuf, String> {
    Ok(account(uid)?.home)
}

/// Whether `dir` can be read by `uid`. A missing directory is not readable.
/// A symlink is treated as readable so a replaced record is refused. When
/// the group-read bit is set and that user's groups cannot be listed, the
/// directory is treated as readable (fail closed).
fn record_readable_by(dir: &Path, uid: u32) -> Result<bool, String> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("cannot stat {}: {e}", dir.display())),
    };
    if meta.file_type().is_symlink() {
        return Ok(true);
    }
    use std::os::unix::fs::MetadataExt;
    let groups = groups_of(uid);
    Ok(access_allows(
        meta.uid(),
        meta.gid(),
        meta.mode(),
        uid,
        groups.as_deref(),
    ))
}

/// Unix read permission of mode `mode` (owner `owner`, group `group`) for
/// `uid`. `groups` is that user's group list; `None` means it is unknown,
/// and a group-read bit then counts as readable.
fn access_allows(owner: u32, group: u32, mode: u32, uid: u32, groups: Option<&[u32]>) -> bool {
    let mode = mode & 0o777;
    if owner == uid {
        return mode & 0o400 != 0;
    }
    if mode & 0o004 != 0 {
        return true;
    }
    if mode & 0o040 != 0 {
        return groups.is_none_or(|gs| gs.contains(&group));
    }
    false
}

/// Groups `uid` belongs to, primary included. `None` when the list cannot
/// be read (the caller fails closed).
fn groups_of(uid: u32) -> Option<Vec<u32>> {
    let account = account(uid).ok()?;
    let name = std::ffi::CString::new(account.name).ok()?;
    let mut len = 64i32;
    for _ in 0..8 {
        let mut groups = vec![0; len as usize];
        // SAFETY: getgrouplist writes at most `len` group ids into `groups`
        // and sets `len` to the number written, or to the size required.
        let rc = unsafe {
            libc::getgrouplist(name.as_ptr(), account.gid, groups.as_mut_ptr(), &mut len)
        };
        if rc >= 0 {
            groups.truncate(len.max(0) as usize);
            return Some(groups);
        }
        if len <= groups.len() as i32 {
            return None;
        }
    }
    None
}

fn loopback_check() -> Check {
    let address = LOOPBACK_PROBE.parse().expect("a socket address");
    let result = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2));
    loopback_verdict(result.map(|_| ()))
}

/// What a connect to the IPv4 loopback says about the unit's address deny.
fn loopback_verdict(result: std::io::Result<()>) -> Check {
    let name = "unit: the IPv4 loopback is denied".to_string();
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Check {
            name,
            passed: true,
            detail: "127.0.0.1 refused by the address filter".into(),
        },
        Err(e) => Check {
            name,
            passed: false,
            detail: format!("127.0.0.1 is reachable ({e})"),
        },
        Ok(()) => Check {
            name,
            passed: false,
            detail: "127.0.0.1 is reachable (connected)".into(),
        },
    }
}

/// A host socket that accepts a connection fails the check; an absent one
/// is not checked.
fn socket_check(path: &str) -> Option<Check> {
    if std::fs::symlink_metadata(path).is_err() {
        return None;
    }
    let connected = std::os::unix::net::UnixStream::connect(path).is_ok();
    Some(Check {
        name: format!("unit: {path} is inaccessible"),
        passed: !connected,
        detail: if connected {
            "a connection was accepted".into()
        } else {
            "connect refused".into()
        },
    })
}

/// The resolver must not be on the IPv4 loopback, which the unit denies.
fn resolver_check(resolv_conf: &str) -> Check {
    let loopback: Vec<&str> = resolv_conf
        .lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .map(str::trim)
        .filter(|ns| ns.starts_with("127."))
        .collect();
    let has_any = resolv_conf
        .lines()
        .any(|l| l.trim().starts_with("nameserver"));
    Check {
        name: "unit: the resolver is off the IPv4 loopback".into(),
        passed: loopback.is_empty() && has_any,
        detail: if !has_any {
            "no nameserver in /etc/resolv.conf".into()
        } else if loopback.is_empty() {
            "resolv.conf names no 127.x resolver".into()
        } else {
            format!("resolv.conf names {}", loopback.join(", "))
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_denied_loopback_passes() {
        let denied = std::io::Error::from_raw_os_error(libc::EPERM);
        assert!(loopback_verdict(Err(denied)).passed);
        let refused = std::io::Error::from_raw_os_error(libc::ECONNREFUSED);
        assert!(
            !loopback_verdict(Err(refused)).passed,
            "nothing listening is not denied"
        );
        assert!(!loopback_verdict(Ok(())).passed);
    }

    #[test]
    fn a_loopback_resolver_fails_and_a_public_one_passes() {
        assert!(!resolver_check("nameserver 127.0.0.53\noptions edns0\n").passed);
        assert!(resolver_check("nameserver 9.9.9.9\nnameserver 2620:fe::fe\n").passed);
        assert!(!resolver_check("# empty\n").passed);
    }

    #[test]
    fn an_accepting_host_socket_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bus");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let check = socket_check(path.to_str().unwrap()).unwrap();
        assert!(!check.passed);
        assert!(socket_check("/nonexistent/pulse-null-socket").is_none());
    }

    const PROBE_UNIT: &str = include_str!("../../../templates/systemd/pulse-null-probe@.service");
    const VERIFY_UNIT: &str =
        include_str!("../../../templates/systemd/pulse-null-probe-verify@.service");
    const EXPORT_UNIT: &str =
        include_str!("../../../templates/systemd/pulse-null-probe-export@.service");

    /// The lines between the CONFINEMENT markers.
    fn confinement_block(unit: &str) -> &str {
        let start = unit.find("# >>> CONFINEMENT").expect("start marker");
        let end = unit.find("# <<< CONFINEMENT <<<").expect("end marker");
        &unit[start..end]
    }

    /// The verify unit checks what the run gets: the same confinement.
    #[test]
    fn the_verify_unit_confines_exactly_as_the_probe_unit() {
        assert_eq!(
            confinement_block(PROBE_UNIT),
            confinement_block(VERIFY_UNIT)
        );
    }

    /// The run denies the IPv4 loopback and resolves off it; only the
    /// export reaches 127.0.0.1, and nothing else.
    #[test]
    fn only_the_export_unit_reaches_the_ipv4_loopback() {
        let block = confinement_block(PROBE_UNIT);
        assert!(block.contains("IPAddressDeny=127.0.0.0/8"));
        assert!(
            block.contains("BindReadOnlyPaths=/etc/pulse-null/probe-resolv.conf:/etc/resolv.conf")
        );
        assert!(!block.contains("IPAddressAllow"), "nothing re-allowed");
        assert!(PROBE_UNIT.contains("Requires=pulse-null-probe-export@%i.service"));
        assert!(EXPORT_UNIT.contains("IPAddressDeny=any"));
        assert!(EXPORT_UNIT.contains("IPAddressAllow=127.0.0.1/32"));
        assert!(EXPORT_UNIT.contains("ExecStart=/usr/local/bin/pulse-null probe export-graph"));
    }

    /// Outside the unit (as in the test suite) a run must be refused.
    #[test]
    fn this_test_process_is_not_confined() {
        assert!(require_unit().is_err());
    }

    #[test]
    fn the_probe_user_must_not_be_the_pulse_and_the_record_must_be_private() {
        assert!(identity_refusal(10, "pulse", "pulse-probe", 10, false)
            .unwrap()
            .contains("not as the live pulse"));
        assert!(identity_refusal(20, "ubuntu", "pulse-probe", 10, false)
            .unwrap()
            .contains("not the probe user"));
        assert!(identity_refusal(20, "pulse-probe", "pulse-probe", 10, true)
            .unwrap()
            .contains("readable by the live pulse"));
        assert!(identity_refusal(20, "pulse-probe", "pulse-probe", 10, false).is_none());
    }

    #[test]
    fn a_0700_directory_is_not_readable_by_another_uid() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let meta = std::fs::metadata(dir.path()).unwrap();
        let other = meta.uid().wrapping_add(1);
        assert!(
            !access_allows(meta.uid(), meta.gid(), meta.mode(), other, Some(&[])),
            "mode 0700 is not readable by another uid"
        );
        assert!(access_allows(
            meta.uid(),
            meta.gid(),
            meta.mode(),
            meta.uid(),
            Some(&[])
        ));
        assert!(access_allows(1, 1, 0o755, 2, Some(&[])));
        assert!(access_allows(1, 5, 0o750, 2, Some(&[5])));
        assert!(!access_allows(1, 5, 0o750, 2, Some(&[9])));
        assert!(
            access_allows(1, 5, 0o750, 2, None),
            "unknown groups fail closed"
        );
        assert!(
            !record_readable_by(dir.path(), other).unwrap(),
            "the directory this test created is not readable by another uid"
        );
        assert!(record_readable_by(dir.path(), meta.uid()).unwrap());
        assert!(!record_readable_by(&dir.path().join("missing"), other).unwrap());
    }
}
