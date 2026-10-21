//! The probe read fence (PN-144 audit #1): what a probe child may read.
//!
//! The Landlock sandbox denies every read it is not told to allow, so this
//! module is the allow-list, kept as data so it can be read, reviewed and
//! documented (`docs/methods/probe-runner.md`, "Read fence"):
//!
//! ```text
//!  system      /usr /bin /sbin /lib /lib32 /lib64 /libx32
//!  /etc        a named list of loader, name-service, TLS, locale and shell
//!              files — never /etc as a whole (unit drop-ins, env files and
//!              service configs live there)
//!  /proc       host-wide read-only files (cpuinfo, meminfo, …, sys/) — no
//!              per-process directory, so no other process's environ
//!  /sys        cgroup limits, cpu and clock topology, hugepage settings
//!  devices     /dev/urandom /dev/random (read); /dev/null … (read-write)
//!  the CLI     the directory of its resolved binary (and of a script's
//!              interpreter), plus what the adapter names for its install
//!  the item    its own scratch directory (read-write)
//! ```
//!
//! Everything else is unreadable to the child: the live pulse home, its
//! `secrets/` and `record/`, the graph server's credential directory, the
//! live user's `~/.config`, `~/.ssh` and vendor directories, the other
//! items and the base copy. An install directory that would contain one of
//! those is refused rather than allowed.

use std::path::{Path, PathBuf};

use crate::cli_provider::adapter::CliAdapter;

/// System directories: binaries, libraries, shared data.
pub const SYSTEM_READS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32",
];

/// Entries of `/etc` a CLI and the tools it runs need: the dynamic loader,
/// users and groups (for `getpwuid`), name resolution, TLS roots, time zone,
/// locale, terminal and shell start-up files, MIME and magic databases, the
/// system git config.
pub const ETC_READS: &[&str] = &[
    "ld.so.cache",
    "ld.so.conf",
    "ld.so.conf.d",
    "nsswitch.conf",
    "passwd",
    "group",
    "hosts",
    "host.conf",
    "hostname",
    "resolv.conf",
    "gai.conf",
    "networks",
    "protocols",
    "services",
    "ssl",
    "ca-certificates",
    "ca-certificates.conf",
    "pki",
    "gnutls",
    "localtime",
    "timezone",
    "locale.alias",
    "locale.conf",
    "os-release",
    "lsb-release",
    "debian_version",
    "alternatives",
    "inputrc",
    "terminfo",
    "profile",
    "profile.d",
    "bash.bashrc",
    "shells",
    "mime.types",
    "magic",
    "magic.mime",
    "gitconfig",
];

/// Host-wide, read-only files under `/proc`. No per-process directory is
/// allowed, so a child cannot read another process's environment or memory
/// map (its own `/proc/self` is not readable either).
pub const PROC_READS: &[&str] = &[
    "/proc/cpuinfo",
    "/proc/meminfo",
    "/proc/stat",
    "/proc/version",
    "/proc/loadavg",
    "/proc/uptime",
    "/proc/filesystems",
    "/proc/sys",
];

/// `/sys` entries runtimes read for their memory and CPU limits, the clock
/// source and allocator tuning.
pub const SYS_READS: &[&str] = &[
    "/sys/fs/cgroup",
    "/sys/devices/system",
    "/sys/kernel/mm/transparent_hugepage",
];

/// Devices a child may read but not write.
pub const DEVICE_READS: &[&str] = &["/dev/urandom", "/dev/random"];

/// Devices a child may read and write.
pub const DEVICE_WRITES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/tty",
    "/dev/ptmx",
    "/dev/pts",
];

/// Live paths a probe child must never read, with what they are. Used to
/// refuse an install directory that would expose one, and by
/// `probe verify-isolation` to show each is unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenyList {
    pub paths: Vec<(String, PathBuf)>,
}

impl DenyList {
    /// The live pulse home (and within it `secrets/` and `record/`), the
    /// graph server's credential directory, and the live user's
    /// configuration, keys and vendor directory.
    #[must_use]
    pub fn live(
        live_root: &Path,
        home: &Path,
        graph_credential_dir: Option<&Path>,
        vendor_home: Option<&Path>,
    ) -> Self {
        let mut paths = vec![
            ("live pulse home".to_string(), live_root.to_path_buf()),
            ("live secrets/".to_string(), live_root.join("secrets")),
            ("live record/".to_string(), live_root.join("record")),
            ("live user's ~/.config".to_string(), home.join(".config")),
            ("live user's ~/.ssh".to_string(), home.join(".ssh")),
            (USER_HOME.to_string(), home.to_path_buf()),
        ];
        if let Some(dir) = graph_credential_dir {
            paths.push(("graph credential dir".to_string(), dir.to_path_buf()));
        }
        if let Some(vendor) = vendor_home {
            paths.push((VENDOR_HOME.to_string(), vendor.to_path_buf()));
        }
        Self { paths }
    }

    /// The first denied path `root` would expose: one beneath it, or `root`
    /// itself.
    fn exposed_by(&self, root: &Path) -> Option<&(String, PathBuf)> {
        let root = canonical(root);
        self.paths
            .iter()
            .find(|(_, denied)| canonical(denied).starts_with(&root))
    }

    /// The first denied path `root` sits inside, skipping the ones labelled
    /// in `allowed` (the user's home and vendor dir, for an install the
    /// adapter declares there).
    fn containing(&self, root: &Path, allowed: &[&str]) -> Option<&(String, PathBuf)> {
        let root = canonical(root);
        self.paths
            .iter()
            .filter(|(label, _)| !allowed.contains(&label.as_str()))
            .find(|(_, denied)| root.starts_with(canonical(denied)))
    }
}

/// The deny-list labels a declared install may sit inside.
const USER_HOME: &str = "live user's home";
const VENDOR_HOME: &str = "live vendor dir";

/// Why a read fence could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FenceError {
    #[error("the agent CLI '{0}' was not found")]
    NoBinary(String),
    #[error(
        "the agent CLI's install directory {root} contains the {what} ({denied}); a probe child \
         may not read it, so the probe refuses to run — install the CLI outside it"
    )]
    ExposesLive {
        root: String,
        what: String,
        denied: String,
    },
    #[error(
        "the agent CLI's install directory {root} {why}; an install a probe child runs from \
         must be one only root can change, or one the adapter declares — the probe refuses to run"
    )]
    UntrustedInstall { root: String, why: String },
}

/// Everything a probe child of `adapter` may read, for the CLI at `bin`
/// (as configured: a path or a name on `PATH`). The item's own directory is
/// not in it: it is the child's write root.
pub fn read_roots(
    adapter: &dyn CliAdapter,
    bin: &str,
    home: &Path,
    deny: &DenyList,
) -> Result<Vec<PathBuf>, FenceError> {
    let mut roots = system_reads();
    let declared: Vec<PathBuf> = adapter
        .probe_install_reads(home)
        .iter()
        .map(|d| canonical(d))
        .collect();
    let me = Identity::current();
    for install in install_roots(adapter, bin, home)? {
        check_install(&install, &declared, deny, home, me.as_ref())?;
        roots.push(install);
    }
    Ok(roots)
}

/// Whether a directory a probe child reads and runs from is safe to give
/// it (audit R2-12). Never one that holds a denied path. An undeclared one
/// (the binary's or interpreter's own directory) must also not sit in the
/// user's home or any denied path, nor be writable, it or any directory
/// above it, by the user the runner runs as; a directory the adapter
/// declares for its install (grok's under `~/.grok`) may sit in the home
/// and the vendor dir, and be the user's own.
fn check_install(
    install: &Path,
    declared: &[PathBuf],
    deny: &DenyList,
    home: &Path,
    me: Option<&Identity>,
) -> Result<(), FenceError> {
    if let Some((what, denied)) = deny.exposed_by(install) {
        return Err(FenceError::ExposesLive {
            root: install.display().to_string(),
            what: what.clone(),
            denied: denied.display().to_string(),
        });
    }
    let canonical_install = canonical(install);
    let is_declared = declared.iter().any(|d| canonical_install.starts_with(d));
    let allowed: &[&str] = if is_declared {
        &[USER_HOME, VENDOR_HOME]
    } else {
        &[]
    };
    let untrusted = |why: String| FenceError::UntrustedInstall {
        root: install.display().to_string(),
        why,
    };
    if let Some((what, denied)) = deny.containing(install, allowed) {
        return Err(untrusted(format!(
            "is inside the {what} ({})",
            denied.display()
        )));
    }
    if is_declared {
        return Ok(());
    }
    if canonical_install.starts_with(canonical(home)) {
        return Err(untrusted("is inside the user's home".into()));
    }
    if let Some(dir) = writable_ancestor(&canonical_install, me) {
        return Err(untrusted(format!(
            "can be changed by this user ({} is writable)",
            dir.display()
        )));
    }
    Ok(())
}

/// The first of `install` and the directories above it that `me` can
/// write, if any (`None` for root, `me` being `None`).
fn writable_ancestor<'p>(install: &'p Path, me: Option<&Identity>) -> Option<&'p Path> {
    let me = me?;
    for dir in install.ancestors() {
        if let Ok(meta) = std::fs::metadata(dir) {
            if me.can_write(&meta) {
                return Some(dir);
            }
        }
    }
    None
}

/// Who the runner runs as, for the writability check. `None` for root,
/// which may write anywhere and never runs probes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    uid: u32,
    gids: Vec<u32>,
}

impl Identity {
    fn current() -> Option<Self> {
        // SAFETY: geteuid/getegid have no preconditions.
        let uid = unsafe { libc::geteuid() };
        if uid == 0 {
            return None;
        }
        let mut gids = vec![unsafe { libc::getegid() }];
        // SAFETY: a first call with size 0 returns the count only.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if count > 0 {
            let mut groups = vec![0 as libc::gid_t; usize::try_from(count).unwrap_or(0)];
            // SAFETY: `groups` holds `count` entries.
            let got = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
            groups.truncate(usize::try_from(got.max(0)).unwrap_or(0));
            gids.extend(groups);
        }
        Some(Self { uid, gids })
    }

    /// Can this identity write to (or replace entries in) the directory?
    fn can_write(&self, meta: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        let mode = meta.mode();
        (meta.uid() == self.uid && mode & 0o200 != 0)
            || (self.gids.contains(&meta.gid()) && mode & 0o020 != 0)
            || mode & 0o002 != 0
    }
}

/// The system part of the fence: system dirs, the named `/etc` entries,
/// the host-wide `/proc` and `/sys` files and the read-only devices. What
/// the throwaway graph server reads too.
#[must_use]
pub fn system_reads() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = SYSTEM_READS.iter().map(PathBuf::from).collect();
    roots.extend(ETC_READS.iter().map(|name| Path::new("/etc").join(name)));
    roots.extend(
        PROC_READS
            .iter()
            .chain(SYS_READS)
            .chain(DEVICE_READS)
            .map(PathBuf::from),
    );
    roots
}

/// The CLI's own install: the directory of its resolved binary, the
/// directory of a script's interpreter, and what the adapter adds.
fn install_roots(
    adapter: &dyn CliAdapter,
    bin: &str,
    home: &Path,
) -> Result<Vec<PathBuf>, FenceError> {
    let resolved = resolve_bin(bin).ok_or_else(|| FenceError::NoBinary(bin.to_string()))?;
    let mut roots = Vec::new();
    if let Some(dir) = resolved.parent() {
        roots.push(dir.to_path_buf());
    }
    if let Some(interpreter) = script_interpreter(&resolved) {
        if let Some(dir) = interpreter.parent() {
            roots.push(dir.to_path_buf());
        }
    }
    roots.extend(adapter.probe_install_reads(home));
    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// `bin` as a canonical path: a path is resolved through its links, a bare
/// name is looked up on `PATH` first.
#[must_use]
pub fn resolve_bin(bin: &str) -> Option<PathBuf> {
    let path = if bin.contains('/') {
        PathBuf::from(bin)
    } else {
        which::which(bin).ok()?
    };
    path.canonicalize().ok()
}

/// The interpreter a `#!` script runs under, resolved (`#!/usr/bin/env node`
/// looks `node` up on `PATH`). `None` for a binary.
fn script_interpreter(path: &Path) -> Option<PathBuf> {
    use std::io::Read;
    let mut head = [0u8; 256];
    let read = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    let first_line = std::str::from_utf8(&head[..read])
        .ok()?
        .strip_prefix("#!")?
        .lines()
        .next()?
        .to_string();
    let mut words = first_line.split_whitespace();
    let program = words.next()?;
    let interpreter = if program.ends_with("/env") {
        words.find(|w| !w.starts_with('-'))?
    } else {
        program
    };
    resolve_bin(interpreter)
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_provider::adapters::claude::Claude;

    fn deny(home: &Path) -> DenyList {
        DenyList::live(&home.join("pulse"), home, None, Some(&home.join(".vendor")))
    }

    #[test]
    fn the_fence_holds_system_dirs_and_the_cli_install_only() {
        let home = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let bin = install.path().join("cli");
        std::fs::write(&bin, "binary").unwrap();
        let roots = read_roots(
            &Claude,
            &bin.display().to_string(),
            home.path(),
            &deny(home.path()),
        )
        .unwrap();
        assert!(roots.contains(&PathBuf::from("/usr")));
        assert!(roots.contains(&PathBuf::from("/etc/ld.so.cache")));
        assert!(roots.contains(&install.path().canonicalize().unwrap()));
        assert!(!roots.contains(&PathBuf::from("/etc")), "never /etc whole");
        assert!(
            !roots.contains(&PathBuf::from("/proc")),
            "never /proc whole"
        );
        assert!(!roots.iter().any(|r| r.starts_with(home.path())));
    }

    #[test]
    fn an_install_that_would_expose_a_live_store_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("cli");
        std::fs::write(&bin, "binary").unwrap();
        let err = read_roots(
            &Claude,
            &bin.display().to_string(),
            home.path(),
            &deny(home.path()),
        )
        .unwrap_err();
        assert!(matches!(err, FenceError::ExposesLive { .. }), "{err}");
        assert!(read_roots(&Claude, "/nonexistent/cli", home.path(), &deny(home.path())).is_err());
    }

    /// R2-12: an undeclared install inside the home, or one this user can
    /// change, is refused; a declared one in the vendor dir is allowed.
    #[test]
    fn untrusted_install_dirs_are_refused_unless_declared() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let vendor = home.path().join(".vendor");
        let declared_dir = vendor.join("downloads");
        std::fs::create_dir_all(&declared_dir).unwrap();
        let deny = deny(home.path());
        let in_home = home.path().join("bin");
        std::fs::create_dir_all(&in_home).unwrap();
        assert!(matches!(
            check_install(&in_home, &[], &deny, home.path(), None),
            Err(FenceError::UntrustedInstall { .. })
        ));
        let declared = [declared_dir.canonicalize().unwrap()];
        assert!(check_install(&declared_dir, &declared, &deny, home.path(), None).is_ok());
        // A declared dir still may not sit in the live pulse home.
        let in_pulse = home.path().join("pulse/bin");
        std::fs::create_dir_all(&in_pulse).unwrap();
        let declared_pulse = [in_pulse.canonicalize().unwrap()];
        assert!(check_install(&in_pulse, &declared_pulse, &deny, home.path(), None).is_err());

        // Writable by the running user: an undeclared dir outside home.
        let outside = tempfile::tempdir().unwrap();
        let dir = outside.path().join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        use std::os::unix::fs::MetadataExt;
        let owner = Identity {
            uid: meta.uid(),
            gids: vec![],
        };
        assert!(owner.can_write(&meta), "owner with u+w");
        let stranger = Identity {
            uid: meta.uid() + 4242,
            gids: vec![],
        };
        assert!(!stranger.can_write(&meta));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(stranger.can_write(&std::fs::metadata(&dir).unwrap()), "o+w");
        assert!(matches!(
            check_install(&dir, &[], &deny, home.path(), Some(&stranger)),
            Err(FenceError::UntrustedInstall { .. })
        ));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let group = Identity {
            uid: meta.uid() + 4242,
            gids: vec![meta.gid()],
        };
        assert!(!group.can_write(&std::fs::metadata(&dir).unwrap()), "g-w");
    }

    #[test]
    fn a_writable_directory_above_an_install_is_refused_by_name() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let home = tempfile::tempdir().unwrap();
        let deny = deny(home.path());
        let outside = tempfile::tempdir().unwrap();
        let parent = outside.path().join("opt");
        let dir = parent.join("cli/bin");
        std::fs::create_dir_all(&dir).unwrap();
        for d in [&parent, &parent.join("cli"), &dir] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let owner = std::fs::metadata(&dir).unwrap().uid();
        let stranger = Identity {
            uid: owner + 4242,
            gids: vec![],
        };
        // The temp dir itself (and /tmp) are world-writable: an install
        // under them is refused at the first such directory up the tree.
        let refused = check_install(&dir, &[], &deny, home.path(), Some(&stranger)).unwrap_err();
        let FenceError::UntrustedInstall { why, .. } = &refused else {
            panic!("{refused:?}");
        };
        assert!(why.starts_with("can be changed by this user ("), "{why}");
        let canonical = dir.canonicalize().unwrap();
        let first_world_writable = canonical
            .ancestors()
            .find(|a| std::fs::metadata(a).unwrap().mode() & 0o002 != 0);
        assert!(first_world_writable.is_some(), "/tmp is world-writable");
        assert_eq!(
            writable_ancestor(&canonical, Some(&stranger)),
            first_world_writable
        );
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            writable_ancestor(&canonical, Some(&stranger)),
            Some(parent.canonicalize().unwrap().as_path()),
            "the nearest writable directory is the one named"
        );
        assert_eq!(
            writable_ancestor(&canonical, None),
            None,
            "root writes anywhere"
        );
    }

    #[test]
    fn an_undeclared_install_nobody_here_can_write_is_trusted() {
        let home = tempfile::tempdir().unwrap();
        let deny = deny(home.path());
        let stranger = Identity {
            uid: u32::MAX - 1,
            gids: vec![],
        };
        // /usr/bin: root-owned, not group- or world-writable.
        assert!(check_install(
            Path::new("/usr/bin"),
            &[],
            &deny,
            home.path(),
            Some(&stranger)
        )
        .is_ok());
        assert_eq!(
            writable_ancestor(Path::new("/usr/bin"), Some(&stranger)),
            None
        );
    }

    #[test]
    fn a_script_brings_its_interpreter_dir() {
        let install = tempfile::tempdir().unwrap();
        let script = install.path().join("cli");
        std::fs::write(&script, "#!/usr/bin/env sh\necho hi\n").unwrap();
        let interpreter = script_interpreter(&script).unwrap();
        assert!(interpreter.ends_with("sh") || interpreter.ends_with("dash"));
        std::fs::write(&script, "plain").unwrap();
        assert_eq!(script_interpreter(&script), None);
    }
}
