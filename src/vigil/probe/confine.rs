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

use std::path::Path;
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

/// `Ok` when every confinement check passes; else why the run is refused.
pub fn require() -> Result<(), String> {
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
        assert!(require().is_err());
    }
}
