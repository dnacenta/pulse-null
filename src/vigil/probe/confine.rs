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
    let mut checks = vec![loopback_check(), v6_https_check()];
    checks.extend(HOST_SOCKETS.iter().filter_map(|path| socket_check(path)));
    let resolv = crate::fs_safe::read_bounded(Path::new("/etc/resolv.conf"), 64 * 1024)
        .ok()
        .flatten()
        .unwrap_or_default();
    checks.push(resolver_check(&resolv));
    checks.push(public_address_check());
    checks
}

/// `Ok` when the process is the probe user (not the live pulse), the probe
/// record is not readable by the pulse's uid, `secrets/` is not searchable
/// by this process, and every confinement check passes. `record_dir` is
/// `record/probes`. `live_root` is the pulse directory.
pub fn require(
    expected_user: &str,
    pulse_uid: u32,
    record_dir: &Path,
    live_root: &Path,
) -> Result<(), String> {
    let euid = euid();
    let name = account(euid).map(|a| a.name).unwrap_or_default();
    let readable = record_readable_by(record_dir, pulse_uid)?;
    if let Some(reason) = identity_refusal(euid, &name, expected_user, pulse_uid, readable) {
        return Err(reason);
    }
    secrets_refusal(&live_root.join("secrets"), euid)?;
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
/// directory is treated as readable (fail closed). A named entry on the
/// directory or on a file in it is refused: the mode bits do not show it.
fn record_readable_by(dir: &Path, uid: u32) -> Result<bool, String> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("cannot stat {}: {e}", dir.display())),
    };
    if meta.file_type().is_symlink() {
        return Ok(true);
    }
    match record_list(dir)? {
        RecordList::Named => {
            return Err(
                "the probe record carries an access list; named entries are refused".into(),
            );
        }
        RecordList::Symlink => return Ok(true),
        RecordList::Plain => {}
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

/// What an access-list walk of the record found.
enum RecordList {
    /// Mode bits only, or no list.
    Plain,
    /// A named user or group entry, beyond the base mode.
    Named,
    /// A symlink inside the record. The caller treats it as readable.
    Symlink,
}

/// The record directory and the files under it. A read of the list that
/// fails for any reason other than "no list" fails closed.
fn record_list(dir: &Path) -> Result<RecordList, String> {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if meta.file_type().is_symlink() {
            return Ok(RecordList::Symlink);
        }
        if access_list_named(&path)? {
            return Ok(RecordList::Named);
        }
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path)
                .map_err(|e| format!("cannot list {}: {e}", path.display()))?
            {
                let entry = entry.map_err(|e| format!("cannot list {}: {e}", path.display()))?;
                pending.push(entry.path());
            }
        }
    }
    Ok(RecordList::Plain)
}

/// `true` when the access list has a named user or group entry. `false`
/// when there is no list. An unreadable list is an error.
fn access_list_named(path: &Path) -> Result<bool, String> {
    match read_access_list(path)? {
        None => Ok(false),
        Some(bytes) => list_has_named_entry(&bytes),
    }
}

fn read_access_list(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("{} is not a path", path.display()))?;
    let name = std::ffi::CString::new("system.posix_acl_access").expect("a fixed attribute name");
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: lgetxattr reads at most `buf.len()` bytes into `buf` and
        // does not follow a symlink. A negative return leaves the error in errno.
        let rc = unsafe {
            libc::lgetxattr(
                c_path.as_ptr(),
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if rc >= 0 {
            buf.truncate(rc as usize);
            return Ok(Some(buf));
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ERANGE) {
            let next = buf.len().saturating_mul(2);
            if next > 64 * 1024 {
                return Err(format!("access list on {} does not fit", path.display()));
            }
            buf.resize(next, 0);
            continue;
        }
        if access_list_error(&err).is_none() {
            return Ok(None);
        }
        return Err(format!(
            "cannot read the access list on {}: {err}",
            path.display()
        ));
    }
}

/// `None` only for a missing list. Every other read error fails closed.
fn access_list_error(err: &std::io::Error) -> Option<&std::io::Error> {
    if err.raw_os_error() == Some(libc::ENODATA) {
        None
    } else {
        Some(err)
    }
}

/// Named user (`0x02`) and named group (`0x08`) entries are beyond the
/// base mode. Anything that is not a version-2 list fails closed.
fn list_has_named_entry(bytes: &[u8]) -> Result<bool, String> {
    if bytes.len() < 4 || !(bytes.len() - 4).is_multiple_of(8) {
        return Err("access list is not readable".into());
    }
    let version = u32::from_le_bytes(bytes[0..4].try_into().expect("four bytes"));
    if version != 2 {
        return Err("access list is not readable".into());
    }
    let (entries, _) = bytes[4..].as_chunks::<8>();
    Ok(entries.iter().any(|entry| {
        let tag = u16::from_le_bytes([entry[0], entry[1]]);
        tag == 0x02 || tag == 0x08
    }))
}

const ACL_USER_OBJ: u16 = 0x01;
const ACL_USER: u16 = 0x02;
const ACL_GROUP_OBJ: u16 = 0x04;
const ACL_GROUP: u16 = 0x08;
const ACL_MASK: u16 = 0x10;
const ACL_OTHER: u16 = 0x20;
const ACL_EXECUTE: u16 = 0x01;

struct AclEntry {
    tag: u16,
    perm: u16,
    id: u32,
}

/// Refuse when the probe user can search `secrets`. A missing directory is
/// not a refusal. A symlink is. An access list that cannot be read fails
/// closed.
fn secrets_refusal(secrets: &Path, probe_uid: u32) -> Result<(), String> {
    if secrets_traversable(secrets, probe_uid)? {
        Err(
            "secrets/ is traversable by the probe user; the probe must not be able to search it"
                .into(),
        )
    } else {
        Ok(())
    }
}

/// Whether `probe_uid` can search `secrets`. A missing directory is not
/// traversable. A symlink is. An access list that cannot be read is an error.
fn secrets_traversable(secrets: &Path, probe_uid: u32) -> Result<bool, String> {
    let meta = match std::fs::symlink_metadata(secrets) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("cannot stat {}: {e}", secrets.display())),
    };
    if meta.file_type().is_symlink() {
        return Ok(true);
    }
    use std::os::unix::fs::MetadataExt;
    let groups = groups_of(probe_uid);
    match read_access_list(secrets)? {
        None => Ok(mode_executable(
            meta.uid(),
            meta.gid(),
            meta.mode(),
            probe_uid,
            groups.as_deref(),
        )),
        Some(bytes) => acl_executable(&bytes, meta.uid(), meta.gid(), probe_uid, groups.as_deref()),
    }
}

/// Unix search permission of mode `mode` for `uid`, when there is no access
/// list. `groups` of `None` means the list is unknown: a group or other
/// execute bit then counts as searchable.
fn mode_executable(owner: u32, group: u32, mode: u32, uid: u32, groups: Option<&[u32]>) -> bool {
    let mode = mode & 0o777;
    if owner == uid {
        return mode & 0o100 != 0;
    }
    let group_x = mode & 0o010 != 0;
    let other_x = mode & 0o001 != 0;
    match groups {
        None => group_x || other_x,
        Some(gs) if gs.contains(&group) => group_x,
        Some(_) => other_x,
    }
}

fn parse_access_list(bytes: &[u8]) -> Result<Vec<AclEntry>, String> {
    if bytes.len() < 4 || !(bytes.len() - 4).is_multiple_of(8) {
        return Err("access list is not readable".into());
    }
    let version = u32::from_le_bytes(bytes[0..4].try_into().expect("four bytes"));
    if version != 2 {
        return Err("access list is not readable".into());
    }
    let (entries, _) = bytes[4..].as_chunks::<8>();
    Ok(entries
        .iter()
        .map(|entry| AclEntry {
            tag: u16::from_le_bytes([entry[0], entry[1]]),
            perm: u16::from_le_bytes([entry[2], entry[3]]),
            id: u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]),
        })
        .collect())
}

fn acl_perm(entries: &[AclEntry], tag: u16) -> Option<u16> {
    entries
        .iter()
        .find(|entry| entry.tag == tag)
        .map(|entry| entry.perm)
}

/// Whether `uid` can search a directory whose access list is `bytes`.
/// A named user entry replaces the group and other classes for that uid.
/// The mask (or the owning-group entry, when there is no mask) limits
/// every named and group entry. Unknown groups fail closed.
fn acl_executable(
    bytes: &[u8],
    owner: u32,
    group: u32,
    uid: u32,
    groups: Option<&[u32]>,
) -> Result<bool, String> {
    let entries = parse_access_list(bytes)?;
    let user_obj = acl_perm(&entries, ACL_USER_OBJ).ok_or("access list is not readable")?;
    let group_obj = acl_perm(&entries, ACL_GROUP_OBJ).ok_or("access list is not readable")?;
    let other = acl_perm(&entries, ACL_OTHER).ok_or("access list is not readable")?;
    let mask = acl_perm(&entries, ACL_MASK).unwrap_or(group_obj);
    if uid == owner {
        return Ok(user_obj & ACL_EXECUTE != 0);
    }
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.tag == ACL_USER && entry.id == uid)
    {
        return Ok(entry.perm & mask & ACL_EXECUTE != 0);
    }
    let group_exec = |perm: u16| perm & mask & ACL_EXECUTE != 0;
    match groups {
        None => {
            let any_group = group_exec(group_obj)
                || entries
                    .iter()
                    .any(|entry| entry.tag == ACL_GROUP && group_exec(entry.perm));
            Ok(any_group || other & ACL_EXECUTE != 0)
        }
        Some(gs) => {
            let mut matched = false;
            let mut granted = 0u16;
            if gs.contains(&group) {
                matched = true;
                granted |= group_obj;
            }
            for entry in entries
                .iter()
                .filter(|entry| entry.tag == ACL_GROUP && gs.contains(&entry.id))
            {
                matched = true;
                granted |= entry.perm;
            }
            if matched {
                Ok(group_exec(granted))
            } else {
                Ok(other & ACL_EXECUTE != 0)
            }
        }
    }
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

/// `[::1]:443` must be denied. The unit allows `::1` so the throwaway can
/// listen, and Landlock allows TCP 443 on every address. Nothing in the run
/// connects to `[::1]:443`; a connect that is not `EPERM` means the address
/// filter is not holding that port.
const V6_HTTPS: &str = "[::1]:443";

fn v6_https_check() -> Check {
    let address = V6_HTTPS.parse().expect("a socket address");
    let result = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2));
    connect_verdict("unit: [::1]:443 is denied", "[::1]:443", result.map(|_| ()))
}

/// What a connect says about an address the unit must deny. Only `EPERM`
/// (the filter) passes; a refusal from nothing listening does not.
fn connect_verdict(name: &str, target: &str, result: std::io::Result<()>) -> Check {
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Check {
            name: name.to_string(),
            passed: true,
            detail: format!("{target} refused by the address filter"),
        },
        Err(e) => Check {
            name: name.to_string(),
            passed: false,
            detail: format!("{target} is reachable ({e})"),
        },
        Ok(()) => Check {
            name: name.to_string(),
            passed: false,
            detail: format!("{target} is reachable (connected)"),
        },
    }
}

/// What a connect to the IPv4 loopback says about the unit's address deny.
fn loopback_verdict(result: std::io::Result<()>) -> Check {
    connect_verdict("unit: the IPv4 loopback is denied", "127.0.0.1", result)
}

/// The host's own public addresses must be denied by the per-host drop-in.
/// Ranges the unit template already denies (loopback, private, CGNAT,
/// link-local, ULA) are not this check. No public address is a pass: there
/// is nothing for the drop-in to name. A failure to list addresses fails
/// closed.
fn public_address_check() -> Check {
    let name = "unit: the host's public addresses are denied";
    match public_addrs() {
        Err(detail) => Check {
            name: name.to_string(),
            passed: false,
            detail,
        },
        Ok(addrs) if addrs.is_empty() => Check {
            name: name.to_string(),
            passed: true,
            detail: "this host has no public address".into(),
        },
        Ok(addrs) => {
            let open: Vec<String> = addrs
                .iter()
                .filter(|addr| !address_denied(**addr))
                .map(|addr| addr.to_string())
                .collect();
            Check {
                name: name.to_string(),
                passed: open.is_empty(),
                detail: if open.is_empty() {
                    "public addresses refused by the address filter".into()
                } else {
                    format!("reachable: {}", open.join(", "))
                },
            }
        }
    }
}

fn address_denied(addr: std::net::IpAddr) -> bool {
    let target = std::net::SocketAddr::new(addr, 9);
    let result = std::net::TcpStream::connect_timeout(&target, Duration::from_secs(2));
    result.is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
}

/// Public addresses on this host: not a range the unit template already denies.
fn public_addrs() -> Result<Vec<std::net::IpAddr>, String> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs writes a list we free with freeifaddrs.
    let rc = unsafe { libc::getifaddrs(&mut head) };
    if rc != 0 {
        return Err(format!(
            "cannot list host addresses ({})",
            std::io::Error::last_os_error()
        ));
    }
    let mut addrs = Vec::new();
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: `cur` is a node in the list getifaddrs just returned.
        let ifa = unsafe { &*cur };
        if let Some(addr) = ifa_addr(ifa.ifa_addr) {
            if needs_public_drop_in(addr) {
                addrs.push(addr);
            }
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: `head` is the list getifaddrs returned.
    unsafe { libc::freeifaddrs(head) };
    addrs.sort();
    addrs.dedup();
    Ok(addrs)
}

fn ifa_addr(ptr: *const libc::sockaddr) -> Option<std::net::IpAddr> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` is a sockaddr from getifaddrs, family-checked before the cast.
    let family = unsafe { (*ptr).sa_family };
    if family == libc::AF_INET as libc::sa_family_t {
        let sin = unsafe { &*(ptr.cast::<libc::sockaddr_in>()) };
        return Some(std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)).into());
    }
    if family == libc::AF_INET6 as libc::sa_family_t {
        let sin6 = unsafe { &*(ptr.cast::<libc::sockaddr_in6>()) };
        return Some(std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr).into());
    }
    None
}

/// Whether `addr` is outside the ranges the unit template denies, so only
/// the per-host drop-in can cover it.
fn needs_public_drop_in(addr: std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            let cgnat = a == 100 && (64..128).contains(&b);
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_unspecified()
                && !ip.is_broadcast()
                && !ip.is_multicast()
                && !cgnat
        }
        std::net::IpAddr::V6(ip) => {
            let ula = (ip.octets()[0] & 0xfe) == 0xfc;
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_unicast_link_local()
                && !ula
        }
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

    /// The run denies by default and allows the resolvers, the throwaway
    /// and the public Internet (TCP 443 is Landlock's limit; the address
    /// filter cannot name a port). Only the export reaches 127.0.0.1.
    #[test]
    fn only_the_export_unit_reaches_the_ipv4_loopback() {
        let block = confinement_block(PROBE_UNIT);
        assert!(block.contains("IPAddressDeny=any"));
        assert!(block.contains("IPAddressDeny=127.0.0.0/8"));
        assert!(block.contains("IPAddressAllow=::1"));
        assert!(block.contains("IPAddressAllow=9.9.9.9"));
        assert!(block.contains("IPAddressAllow=1.1.1.1"));
        assert!(block.contains("IPAddressAllow=2620:fe::fe"));
        assert!(block.contains("InaccessiblePaths=-/run/user"));
        assert!(
            block.contains("BindReadOnlyPaths=/etc/pulse-null/probe-resolv.conf:/etc/resolv.conf")
        );
        assert!(PROBE_UNIT.contains("Requires=pulse-null-probe-export@%i.service"));
        assert!(EXPORT_UNIT.contains("IPAddressDeny=any"));
        assert!(EXPORT_UNIT.contains("IPAddressAllow=127.0.0.1/32"));
        assert!(
            EXPORT_UNIT.contains("ExecStart=/usr/local/bin/pulse-null probe export-graph --if-due")
        );
        assert!(EXPORT_UNIT
            .contains("LoadCredential=root-password:/opt/pulse-null/surrealdb/root-password"));
        assert!(
            !PROBE_UNIT.contains("root-password"),
            "the run unit has no path to the graph root password"
        );
        assert!(
            !VERIFY_UNIT.contains("root-password"),
            "the verify unit has no path to the graph root password"
        );
    }

    #[test]
    fn a_public_address_needs_the_drop_in_and_a_private_one_does_not() {
        use std::net::IpAddr;
        let public: IpAddr = "203.0.113.5".parse().unwrap();
        let private: IpAddr = "10.1.2.3".parse().unwrap();
        let cgnat: IpAddr = "100.64.0.1".parse().unwrap();
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        let ula: IpAddr = "fd00::1".parse().unwrap();
        let link: IpAddr = "fe80::1".parse().unwrap();
        assert!(needs_public_drop_in(public));
        assert!(needs_public_drop_in(v6));
        assert!(!needs_public_drop_in(private));
        assert!(!needs_public_drop_in(cgnat));
        assert!(!needs_public_drop_in(loopback));
        assert!(!needs_public_drop_in(ula));
        assert!(!needs_public_drop_in(link));
        let denied = std::io::Error::from_raw_os_error(libc::EPERM);
        assert!(connect_verdict("v6", "[::1]:443", Err(denied)).passed);
        assert!(!connect_verdict("v6", "[::1]:443", Ok(())).passed);
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

    #[test]
    fn a_named_access_list_entry_is_refused_and_a_missing_list_is_not() {
        let base = access_list_bytes(&[(0x01, 7), (0x04, 5), (0x20, 0)]);
        assert!(!list_has_named_entry(&base).unwrap());
        let named = access_list_bytes(&[(0x01, 7), (0x02, 4), (0x04, 5), (0x10, 7), (0x20, 0)]);
        assert!(list_has_named_entry(&named).unwrap());
        assert!(
            list_has_named_entry(&[0, 0, 0, 1]).is_err(),
            "a bad list fails closed"
        );
        let enodata = std::io::Error::from_raw_os_error(libc::ENODATA);
        assert!(access_list_error(&enodata).is_none());
        let io = std::io::Error::from_raw_os_error(libc::EIO);
        assert!(access_list_error(&io).is_some());

        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        write_access_list(dir.path(), &named);
        let err = record_readable_by(dir.path(), 1).unwrap_err();
        assert!(err.contains("access list"), "{err}");

        let plain = tempfile::tempdir().unwrap();
        std::fs::set_permissions(plain.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = plain.path().join("items.jsonl");
        std::fs::write(&file, b"{}\n").unwrap();
        write_access_list(&file, &named);
        let err = record_readable_by(plain.path(), 1).unwrap_err();
        assert!(err.contains("access list"), "{err}");
    }

    #[test]
    fn a_secrets_directory_the_probe_can_search_is_refused() {
        let missing = tempfile::tempdir().unwrap();
        assert!(secrets_refusal(&missing.path().join("secrets"), 1).is_ok());

        let dir = tempfile::tempdir().unwrap();
        let secrets = dir.path().join("secrets");
        std::fs::create_dir(&secrets).unwrap();
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = std::fs::metadata(&secrets).unwrap().uid();
        let probe = owner.wrapping_add(1);
        assert!(
            secrets_refusal(&secrets, probe).is_ok(),
            "mode 0700 is not searchable by another uid"
        );
        let owner_err = secrets_refusal(&secrets, owner).unwrap_err();
        assert!(owner_err.contains("traversable"), "{owner_err}");

        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o701)).unwrap();
        assert!(
            secrets_refusal(&secrets, probe)
                .unwrap_err()
                .contains("traversable"),
            "other-execute is a refusal"
        );

        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o700)).unwrap();
        write_access_list(
            &secrets,
            &access_list_with_ids(&[
                (0x01, 7, 0),
                (0x02, 1, probe),
                (0x04, 0, 0),
                (0x10, 7, 0),
                (0x20, 0, 0),
            ]),
        );
        assert!(
            secrets_refusal(&secrets, probe)
                .unwrap_err()
                .contains("traversable"),
            "a named execute entry is a refusal"
        );
        write_access_list(
            &secrets,
            &access_list_with_ids(&[
                (0x01, 7, 0),
                (0x02, 1, probe),
                (0x04, 0, 0),
                (0x10, 6, 0),
                (0x20, 0, 0),
            ]),
        );
        assert!(
            secrets_refusal(&secrets, probe).is_ok(),
            "a mask without execute removes the named grant"
        );
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_access_list(
            &secrets,
            &access_list_with_ids(&[
                (0x01, 7, 0),
                (0x02, 0, probe),
                (0x04, 5, 0),
                (0x10, 7, 0),
                (0x20, 5, 0),
            ]),
        );
        assert!(
            secrets_refusal(&secrets, probe).is_ok(),
            "a named deny replaces other-execute"
        );

        let link = dir.path().join("secrets-link");
        std::os::unix::fs::symlink(&secrets, &link).unwrap();
        assert!(secrets_refusal(&link, probe)
            .unwrap_err()
            .contains("traversable"));

        assert!(
            acl_executable(&[0, 0, 0, 1], 1, 2, 9, Some(&[])).is_err(),
            "a bad list fails closed"
        );

        let group = access_list_with_ids(&[
            (0x01, 7, 0),
            (0x04, 0, 0),
            (0x08, 1, 50),
            (0x10, 7, 0),
            (0x20, 0, 0),
        ]);
        assert!(acl_executable(&group, 1, 2, 9, Some(&[50])).unwrap());
        assert!(!acl_executable(&group, 1, 2, 9, Some(&[9])).unwrap());
        assert!(
            acl_executable(&group, 1, 2, 9, None).unwrap(),
            "unknown groups fail closed"
        );
    }

    /// A version-2 access list. `id` is unused for the base entries and a
    /// fixed uid for a named one; the kernel accepts either.
    fn access_list_bytes(entries: &[(u16, u16)]) -> Vec<u8> {
        access_list_with_ids(
            &entries
                .iter()
                .map(|(tag, perm)| (*tag, *perm, 1000))
                .collect::<Vec<_>>(),
        )
    }

    fn access_list_with_ids(entries: &[(u16, u16, u32)]) -> Vec<u8> {
        let mut bytes = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in entries {
            bytes.extend(tag.to_le_bytes());
            bytes.extend(perm.to_le_bytes());
            bytes.extend(id.to_le_bytes());
        }
        bytes
    }

    fn write_access_list(path: &Path, bytes: &[u8]) {
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let name = std::ffi::CString::new("system.posix_acl_access").unwrap();
        let rc = unsafe {
            libc::lsetxattr(
                c_path.as_ptr(),
                name.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
            )
        };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    }
}
