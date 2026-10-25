//! A Landlock sandbox for one CLI child (PN-144).
//!
//! Probes run a pulse turn in a scratch copy of the pulse while the live
//! daemon keeps writing to the real one, so a before/after diff can never
//! prove that a probe wrote nothing live. The sandbox makes it true by
//! construction instead: the child may read and execute only beneath the
//! read roots it is given (system directories, the CLI's own install), may
//! write only beneath the scratch directories it is given, and may open TCP
//! connections only to the ports it is given. Everything else Landlock ABI 4
//! can restrict is denied, so a live store is neither written nor read.
//!
//! The ruleset is built by the caller, in the parent. The child applies it
//! to itself between `fork` and `exec` (`no_new_privs`, then
//! `landlock_restrict_self`), after adding the one rule the parent cannot
//! write: read access to its own `/proc/self` (a process's `/proc/<pid>`
//! only exists once it does, and runtimes such as Bun abort without their
//! own `maps` and `stat`). Another process's `/proc/<pid>` stays unreadable.
//! The hook makes only raw syscalls on memory prepared before the fork.
//! Everything fails closed: a kernel without Landlock ABI 4 (the first with
//! TCP rules) refuses to build a sandbox at all, and if the child cannot
//! restrict itself it exits before `exec`, so nothing runs unsandboxed.
//!
//! A forked child holds copies of the parent's descriptors until it execs;
//! file locks in this process are therefore released explicitly
//! (`LOCK_UN`), never by closing alone.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use landlock::{
    Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreated, RulesetCreatedAttr, ABI,
};

/// The Landlock ABI the sandbox needs: version 4 adds TCP port rules.
pub const REQUIRED_ABI: i32 = 4;

/// The ABI the ruleset is written against. Rights newer than this are left
/// unhandled, which only ever means "not restricted by this sandbox".
const RULESET_ABI: ABI = ABI::V4;

/// Where a sandboxed child may read, write and connect. Everything else that
/// Landlock ABI 4 can restrict is denied.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sandbox {
    /// Directories (or single files) the child may read and execute beneath,
    /// never write. A read root that does not exist is skipped: system
    /// layouts differ, and a missing root only ever denies more.
    pub read_roots: Vec<PathBuf>,
    /// Directories (or single files such as `/dev/null`) the child may read,
    /// write and execute beneath. Paths that do not exist are an error, not a
    /// silent gap.
    pub write_roots: Vec<PathBuf>,
    /// TCP ports the child may connect to.
    pub tcp_connect: Vec<u16>,
    /// TCP ports the child may bind (a throwaway server the runner starts).
    /// Empty for a CLI child.
    pub tcp_bind: Vec<u16>,
    /// Run the child at background priority (nice 10, idle I/O class), so
    /// a measurement never competes with the live pulses for CPU or disk.
    pub background: bool,
}

/// Why a sandbox could not be built.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error(
        "Landlock is not available on this kernel ({0}); the probe sandbox refuses to run \
         without it"
    )]
    Unavailable(&'static str),
    #[error(
        "this kernel offers Landlock ABI {found}; the probe sandbox needs ABI {REQUIRED_ABI} \
         (TCP connect rules) and refuses to run without it"
    )]
    AbiTooOld { found: i32 },
    #[error("write root '{path}' cannot be opened for the sandbox: {reason}")]
    WriteRoot { path: String, reason: String },
    #[error("read root '{path}' cannot be opened for the sandbox: {reason}")]
    ReadRoot { path: String, reason: String },
    #[error("could not build the Landlock ruleset: {0}")]
    Ruleset(String),
}

/// What the running kernel offers, as reported by Landlock itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandlockSupport {
    /// The kernel has no Landlock (`ENOSYS`).
    NotImplemented,
    /// Landlock is built in but not enabled at boot (`EOPNOTSUPP`).
    NotEnabled,
    /// Landlock is enabled and speaks this ABI version.
    Abi(i32),
}

impl LandlockSupport {
    /// Ask the kernel which Landlock ABI it speaks.
    #[must_use]
    pub fn detect() -> Self {
        // SAFETY: `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`
        // reads no memory and creates no file descriptor; it only returns the
        // ABI version or a negative value with errno set.
        let version = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        if version > 0 {
            return Self::Abi(i32::try_from(version).unwrap_or(i32::MAX));
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EOPNOTSUPP) => Self::NotEnabled,
            _ => Self::NotImplemented,
        }
    }

    /// `Ok` when the sandbox can be enforced in full on this kernel.
    pub fn require(self) -> Result<(), SandboxError> {
        match self {
            Self::NotImplemented => Err(SandboxError::Unavailable("not built into the kernel")),
            Self::NotEnabled => Err(SandboxError::Unavailable("not enabled at boot")),
            Self::Abi(found) if found < REQUIRED_ABI => Err(SandboxError::AbiTooOld { found }),
            Self::Abi(_) => Ok(()),
        }
    }

    /// One line for status output.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::NotImplemented => "Landlock not built into the kernel".to_string(),
            Self::NotEnabled => "Landlock not enabled at boot".to_string(),
            Self::Abi(found) if found < REQUIRED_ABI => {
                format!("Landlock ABI {found} (needs {REQUIRED_ABI})")
            }
            Self::Abi(found) => format!("Landlock ABI {found}"),
        }
    }
}

/// `LANDLOCK_CREATE_RULESET_VERSION` from the kernel uapi.
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;

/// A ruleset built in the parent, ready to restrict one child.
#[derive(Debug)]
pub struct PreparedSandbox {
    ruleset: OwnedFd,
    background: bool,
}

impl Sandbox {
    /// Build the ruleset. Fails closed: no Landlock, an old ABI, or a write
    /// root that cannot be opened all refuse.
    pub fn prepare(&self) -> Result<PreparedSandbox, SandboxError> {
        LandlockSupport::detect().require()?;
        let ruleset = self.add_rules(handled_ruleset()?)?;
        let ruleset: Option<OwnedFd> = ruleset.into();
        let ruleset = ruleset.ok_or(SandboxError::Unavailable(
            "the ruleset was not created by the kernel",
        ))?;
        Ok(PreparedSandbox {
            ruleset,
            background: self.background,
        })
    }

    /// Every rule this sandbox allows: its read roots, write roots and
    /// ports.
    fn add_rules(&self, mut ruleset: RulesetCreated) -> Result<RulesetCreated, SandboxError> {
        for root in &self.read_roots {
            ruleset = add_read_root(ruleset, root)?;
        }
        for root in &self.write_roots {
            ruleset = add_write_root(ruleset, root)?;
        }
        ruleset = add_ports(ruleset, &self.tcp_connect, AccessNet::ConnectTcp)?;
        add_ports(ruleset, &self.tcp_bind, AccessNet::BindTcp)
    }
}

fn ruleset_error(e: landlock::RulesetError) -> SandboxError {
    SandboxError::Ruleset(e.to_string())
}

/// A ruleset that handles (denies unless a rule allows) every filesystem
/// and network right of [`RULESET_ABI`], as a hard requirement.
fn handled_ruleset() -> Result<RulesetCreated, SandboxError> {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(RULESET_ABI))
        .map_err(ruleset_error)?
        .handle_access(AccessNet::from_all(RULESET_ABI))
        .map_err(ruleset_error)?
        .create()
        .map_err(ruleset_error)
}

/// Allow reading and executing beneath `root`. A root that does not exist
/// is skipped (it only ever denies more); one that cannot be opened refuses.
fn add_read_root(ruleset: RulesetCreated, root: &Path) -> Result<RulesetCreated, SandboxError> {
    let read_root_error = |reason: String| SandboxError::ReadRoot {
        path: root.display().to_string(),
        reason,
    };
    let metadata = match std::fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ruleset),
        Err(e) => return Err(read_root_error(e.to_string())),
    };
    let fd = PathFd::new(root).map_err(|e| read_root_error(e.to_string()))?;
    let access = rights_for(AccessFs::from_read(RULESET_ABI), metadata.is_dir());
    ruleset
        .add_rule(PathBeneath::new(fd, access))
        .map_err(ruleset_error)
}

/// Allow everything beneath `root`. A root that does not exist refuses:
/// a missing write root is a gap, not a denial.
fn add_write_root(ruleset: RulesetCreated, root: &Path) -> Result<RulesetCreated, SandboxError> {
    let write_root_error = |reason: String| SandboxError::WriteRoot {
        path: root.display().to_string(),
        reason,
    };
    let metadata = std::fs::metadata(root).map_err(|e| write_root_error(e.to_string()))?;
    let fd = PathFd::new(root).map_err(|e| write_root_error(e.to_string()))?;
    let access = rights_for(AccessFs::from_all(RULESET_ABI), metadata.is_dir());
    ruleset
        .add_rule(PathBeneath::new(fd, access))
        .map_err(ruleset_error)
}

/// Allow `access` (connect or bind) on each of `ports`.
fn add_ports(
    mut ruleset: RulesetCreated,
    ports: &[u16],
    access: AccessNet,
) -> Result<RulesetCreated, SandboxError> {
    for port in ports {
        ruleset = ruleset
            .add_rule(NetPort::new(*port, access))
            .map_err(ruleset_error)?;
    }
    Ok(ruleset)
}

/// `rights` as a rule on a directory carries them; a rule on a single file
/// may only carry the rights a file has.
fn rights_for(rights: landlock::BitFlags<AccessFs>, is_dir: bool) -> landlock::BitFlags<AccessFs> {
    if is_dir {
        rights
    } else {
        rights & AccessFs::from_file(RULESET_ABI)
    }
}

impl PreparedSandbox {
    /// Spawn `cmd` inside this sandbox: the child restricts itself before
    /// it execs, so the program starts sandboxed and the parent never is.
    /// If the child cannot restrict itself the spawn fails and nothing runs.
    pub async fn spawn(
        self,
        mut cmd: tokio::process::Command,
    ) -> io::Result<tokio::process::Child> {
        let ruleset = self.ruleset.as_raw_fd();
        let background = self.background;
        // SAFETY: the hook runs in the forked child before exec. It calls
        // only `open`, `close`, `prctl` and the two Landlock syscalls, on a
        // NUL-terminated static path and a stack-allocated attribute; it
        // allocates nothing and takes no lock, so it is async-signal-safe.
        // `ruleset` stays open in the parent until `spawn` returns, which is
        // after the child has exec'd or failed.
        unsafe {
            cmd.pre_exec(move || {
                if background {
                    lower_priority();
                }
                restrict_child(ruleset)
            });
        }
        // The child leads a process group of its own, so what it starts and
        // leaves running can be killed with it ([`GroupGuard`]).
        cmd.process_group(0);
        let child = crate::process_state::during_fork(|| cmd.spawn());
        drop(self);
        child
    }

    /// Run `cmd` inside this sandbox to completion, capturing its output.
    pub async fn output(
        self,
        mut cmd: tokio::process::Command,
    ) -> io::Result<std::process::Output> {
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        self.spawn(cmd).await?.wait_with_output().await
    }
}

/// Kills a sandboxed child's whole process group when dropped, so nothing
/// it started outlives it (a background job, a daemon that did not
/// `setsid`). Under the probe unit systemd also kills the cgroup.
#[derive(Debug)]
pub struct GroupGuard(Option<libc::pid_t>);

impl GroupGuard {
    /// The guard for a child spawned by [`PreparedSandbox::spawn`] (which
    /// leads its own group, so its pid is the group id).
    #[must_use]
    pub fn of(child: &tokio::process::Child) -> Self {
        Self(child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()))
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(group) = self.0.filter(|g| *g > 1) {
            // SAFETY: killpg on the group this guard's child leads.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }
}

/// Wait for `child` and collect its output, reading at most `stdout_max`
/// and `stderr_max` bytes. Output past a bound kills the child's process
/// group and is an error: a child cannot make the runner hold unbounded
/// output in memory.
pub async fn wait_bounded(
    mut child: tokio::process::Child,
    stdout_max: usize,
    stderr_max: usize,
) -> io::Result<std::process::Output> {
    let group = GroupGuard::of(&child);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (out, err) = tokio::try_join!(
        read_bounded(stdout, stdout_max, &group),
        read_bounded(stderr, stderr_max, &group),
    )?;
    let status = child.wait().await?;
    if out.len() > stdout_max || err.len() > stderr_max {
        return Err(io::Error::other(format!(
            "the child wrote more than {stdout_max} bytes of output (or {stderr_max} of errors); \
             it was stopped"
        )));
    }
    Ok(std::process::Output {
        status,
        stdout: out,
        stderr: err,
    })
}

/// Read `pipe` to its end or to one byte past `max`, killing the group at
/// that point so the other pipe reaches its end too.
async fn read_bounded(
    pipe: Option<impl tokio::io::AsyncRead + Unpin>,
    max: usize,
    group: &GroupGuard,
) -> io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let Some(pipe) = pipe else {
        return Ok(buf);
    };
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    pipe.take(limit).read_to_end(&mut buf).await?;
    if buf.len() > max {
        if let Some(pgid) = group.0.filter(|g| *g > 1) {
            // SAFETY: killpg on the group the guard's child leads.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
    Ok(buf)
}

/// In the forked child: nice 10 and the idle I/O class. Best effort (a
/// failure leaves the default priority); raw syscalls only.
fn lower_priority() {
    /// `IOPRIO_WHO_PROCESS`, and the idle class shifted into place.
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    const IOPRIO_IDLE: libc::c_int = 3 << 13;
    // SAFETY: plain syscalls with integer arguments on the calling process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_IDLE);
    }
}

/// `struct landlock_path_beneath_attr` from the kernel uapi.
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// `LANDLOCK_RULE_PATH_BENEATH`.
const RULE_PATH_BENEATH: libc::c_int = 1;

/// `LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR`.
const READ_FILE_AND_DIR: u64 = (1 << 2) | (1 << 3);

/// In the forked child: allow reading its own `/proc/self`, then restrict
/// itself to `ruleset`. Only raw syscalls (see [`PreparedSandbox::spawn`]).
fn restrict_child(ruleset: RawFd) -> io::Result<()> {
    allow_own_proc(ruleset)?;
    restrict_self(ruleset)
}

/// Add the rule the parent cannot write: reading this process's own
/// `/proc/self`.
fn allow_own_proc(ruleset: RawFd) -> io::Result<()> {
    let own_proc = open_own_proc()?;
    let added = add_read_beneath(ruleset, own_proc);
    // SAFETY: closing the descriptor `open_own_proc` opened, on every path.
    unsafe { libc::close(own_proc) };
    added
}

/// An `O_PATH` descriptor on `/proc/self`; the caller closes it.
fn open_own_proc() -> io::Result<RawFd> {
    // SAFETY: a static NUL-terminated path; the caller closes the result.
    let own_proc = unsafe {
        libc::open(
            c"/proc/self".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if own_proc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(own_proc)
}

/// Add a rule to `ruleset` allowing files and directories beneath `dir` to
/// be read. The error is read before anything else can change `errno`.
fn add_read_beneath(ruleset: RawFd, dir: RawFd) -> io::Result<()> {
    let attr = PathBeneathAttr {
        allowed_access: READ_FILE_AND_DIR,
        parent_fd: dir,
    };
    // SAFETY: `attr` is a valid, packed `landlock_path_beneath_attr` that
    // lives for the call; `ruleset` is a Landlock ruleset descriptor.
    let added = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset,
            RULE_PATH_BENEATH,
            std::ptr::addr_of!(attr),
            0u32,
        )
    };
    if added != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `no_new_privs`, then enforce `ruleset` on this process for good.
fn restrict_self(ruleset: RawFd) -> io::Result<()> {
    set_no_new_privs()?;
    landlock_restrict_self(ruleset)
}

fn set_no_new_privs() -> io::Result<()> {
    // SAFETY: plain syscalls with integer arguments; no memory is shared.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn landlock_restrict_self(ruleset: RawFd) -> io::Result<()> {
    // SAFETY: an integer syscall; `ruleset` is a Landlock ruleset descriptor.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Tests that need Landlock fail on a kernel without it: a test that cannot
/// check anything must not pass (audit R2-13). A machine that knowingly
/// lacks Landlock (a CI runner) sets `PULSE_NULL_TESTS_WITHOUT_LANDLOCK=1`
/// to skip them, loudly. The production path refuses either way.
#[cfg(test)]
pub(crate) fn landlock_or_skip(test: &str) -> bool {
    match LandlockSupport::detect().require() {
        Ok(()) => true,
        Err(e) if std::env::var_os(NO_LANDLOCK_ENV).is_some_and(|v| v == "1") => {
            eprintln!("SKIPPED {test}: {e} ({NO_LANDLOCK_ENV}=1)");
            false
        }
        Err(e) => panic!(
            "{test} needs Landlock and cannot check anything without it: {e}. \
             Set {NO_LANDLOCK_ENV}=1 to skip the Landlock tests knowingly."
        ),
    }
}

/// The variable that lets the Landlock tests skip on a kernel without it.
#[cfg(test)]
const NO_LANDLOCK_ENV: &str = "PULSE_NULL_TESTS_WITHOUT_LANDLOCK";

#[cfg(test)]
mod tests {
    use super::*;

    /// What a plain shell needs to start: the system binaries and libraries
    /// and the loader's configuration.
    fn shell_reads() -> Vec<PathBuf> {
        ["/usr", "/lib", "/lib64", "/bin", "/etc/ld.so.cache"]
            .iter()
            .map(PathBuf::from)
            .collect()
    }

    #[test]
    fn old_or_missing_landlock_is_refused() {
        assert!(matches!(
            LandlockSupport::NotImplemented.require(),
            Err(SandboxError::Unavailable(_))
        ));
        assert!(matches!(
            LandlockSupport::NotEnabled.require(),
            Err(SandboxError::Unavailable(_))
        ));
        assert!(matches!(
            LandlockSupport::Abi(3).require(),
            Err(SandboxError::AbiTooOld { found: 3 })
        ));
        assert!(LandlockSupport::Abi(REQUIRED_ABI).require().is_ok());
        assert!(LandlockSupport::Abi(6).require().is_ok());
    }

    #[test]
    fn status_says_what_the_kernel_offers() {
        assert_eq!(
            LandlockSupport::NotImplemented.describe(),
            "Landlock not built into the kernel"
        );
        assert_eq!(
            LandlockSupport::NotEnabled.describe(),
            "Landlock not enabled at boot"
        );
        assert_eq!(
            LandlockSupport::Abi(3).describe(),
            format!("Landlock ABI 3 (needs {REQUIRED_ABI})")
        );
        assert_eq!(LandlockSupport::Abi(6).describe(), "Landlock ABI 6");
    }

    /// The first steps of `restrict_child`, run in the parent: they only add
    /// a rule to a ruleset nothing enforces. (The child runs them between
    /// fork and exec, where coverage is not counted; the sandbox tests below
    /// show their effect.)
    #[test]
    fn the_own_proc_rule_is_added_to_the_ruleset_or_refused() {
        if !landlock_or_skip("the_own_proc_rule_is_added_to_the_ruleset_or_refused") {
            return;
        }
        let prepared = Sandbox::default().prepare().unwrap();
        let ruleset = prepared.ruleset.as_raw_fd();
        assert!(allow_own_proc(ruleset).is_ok());
        let own_proc = open_own_proc().unwrap();
        let bad_ruleset = add_read_beneath(-1, own_proc).unwrap_err();
        assert_eq!(bad_ruleset.raw_os_error(), Some(libc::EBADF));
        // SAFETY: closing the descriptor opened above.
        unsafe { libc::close(own_proc) };
        assert_eq!(
            allow_own_proc(-1).unwrap_err().raw_os_error(),
            Some(libc::EBADF),
            "a failed rule still closes /proc/self and refuses"
        );
    }

    #[test]
    fn an_unreadable_read_root_is_refused_and_a_missing_one_skipped() {
        if !landlock_or_skip("an_unreadable_read_root_is_refused_and_a_missing_one_skipped") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let missing = Sandbox {
            read_roots: vec![dir.path().join("not-there")],
            ..Sandbox::default()
        };
        assert!(missing.prepare().is_ok());
        // A path through a file cannot be looked up (ENOTDIR, not ENOENT).
        std::fs::write(dir.path().join("file"), "x").unwrap();
        let through_a_file = Sandbox {
            read_roots: vec![dir.path().join("file/below")],
            ..Sandbox::default()
        };
        assert!(matches!(
            through_a_file.prepare(),
            Err(SandboxError::ReadRoot { .. })
        ));
    }

    #[test]
    fn a_missing_write_root_is_refused_not_ignored() {
        if !landlock_or_skip("a_missing_write_root_is_refused_not_ignored") {
            return;
        }
        let sandbox = Sandbox {
            write_roots: vec![PathBuf::from("/nonexistent/pulse-null-probe")],
            ..Sandbox::default()
        };
        assert!(matches!(
            sandbox.prepare(),
            Err(SandboxError::WriteRoot { .. })
        ));
    }

    #[tokio::test]
    async fn a_child_writes_only_beneath_its_roots() {
        if !landlock_or_skip("a_child_writes_only_beneath_its_roots") {
            return;
        }
        let allowed = tempfile::tempdir().unwrap();
        let denied = tempfile::tempdir().unwrap();
        let sandbox = Sandbox {
            read_roots: shell_reads(),
            write_roots: vec![allowed.path().to_path_buf(), PathBuf::from("/dev/null")],
            ..Sandbox::default()
        };
        let script = format!(
            "echo in > '{}/ok' && echo out > '{}/no'; echo done > /dev/null",
            allowed.path().display(),
            denied.path().display()
        );
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(&script);
        let output = sandbox.prepare().unwrap().output(cmd).await.unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(allowed.path().join("ok").exists(), "{stderr}");
        assert!(!denied.path().join("no").exists(), "{stderr}");
        assert!(stderr.contains("Permission denied"), "{stderr}");

        // Only the child was restricted: an ordinary child spawned
        // afterwards writes where the sandboxed one could not.
        let denied_path = denied.path().display().to_string();
        let status = crate::process_state::during_fork(|| {
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("echo out > '{denied_path}/no'"))
                .status()
        })
        .unwrap();
        assert!(status.success());
        assert!(denied.path().join("no").exists());
    }

    #[tokio::test]
    async fn the_parent_is_not_restricted() {
        if !landlock_or_skip("the_parent_is_not_restricted") {
            return;
        }
        let elsewhere = tempfile::tempdir().unwrap();
        let cmd = tokio::process::Command::new("/bin/true");
        // `/dev/null` is opened for the child's stdin by the restricted thread.
        let output = Sandbox {
            read_roots: shell_reads(),
            write_roots: vec![PathBuf::from("/dev/null")],
            ..Sandbox::default()
        }
        .prepare()
        .unwrap()
        .output(cmd)
        .await
        .unwrap();
        assert!(output.status.success());
        std::fs::write(elsewhere.path().join("parent"), "still free").unwrap();
    }

    #[tokio::test]
    async fn tcp_connects_only_to_allowed_ports() {
        if !landlock_or_skip("tcp_connects_only_to_allowed_ports") {
            return;
        }
        if !std::path::Path::new("/bin/bash").exists() {
            eprintln!("SKIPPED tcp_connects_only_to_allowed_ports: no /bin/bash");
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap().port();
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = other.local_addr().unwrap().port();
        let sandbox = Sandbox {
            read_roots: shell_reads(),
            write_roots: vec![PathBuf::from("/dev/null")],
            tcp_connect: vec![open],
            ..Sandbox::default()
        };
        let script = format!(
            "(exec 3<>/dev/tcp/127.0.0.1/{open}) && echo ALLOWED; \
             (exec 3<>/dev/tcp/127.0.0.1/{closed}) || echo DENIED"
        );
        let mut cmd = tokio::process::Command::new("/bin/bash");
        cmd.arg("-c").arg(&script);
        let output = sandbox.prepare().unwrap().output(cmd).await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("ALLOWED"), "{stdout} {stderr}");
        assert!(stdout.contains("DENIED"), "{stdout} {stderr}");
        assert!(stderr.contains("Permission denied"), "{stderr}");
    }

    #[tokio::test]
    async fn a_child_reads_only_beneath_its_roots() {
        if !landlock_or_skip("a_child_reads_only_beneath_its_roots") {
            return;
        }
        let readable = tempfile::tempdir().unwrap();
        let hidden = tempfile::tempdir().unwrap();
        std::fs::write(readable.path().join("open"), "visible").unwrap();
        std::fs::write(hidden.path().join("secret"), "fixture-not-a-secret").unwrap();
        let mut read_roots = shell_reads();
        read_roots.push(readable.path().to_path_buf());
        read_roots.push(PathBuf::from("/nonexistent/pulse-null-probe-read-root"));
        let sandbox = Sandbox {
            read_roots,
            write_roots: vec![PathBuf::from("/dev/null")],
            ..Sandbox::default()
        };
        let script = format!(
            "cat '{}/open'; cat '{}/secret' || echo HIDDEN; ls '{}' || echo UNLISTED; \
             echo x > '{}/new' || echo READONLY",
            readable.path().display(),
            hidden.path().display(),
            hidden.path().display(),
            readable.path().display(),
        );
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(&script);
        let output = sandbox.prepare().unwrap().output(cmd).await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("visible"), "{stdout} {stderr}");
        assert!(!stdout.contains("fixture-not-a-secret"), "{stdout}");
        assert!(stdout.contains("HIDDEN"), "{stdout} {stderr}");
        assert!(stdout.contains("UNLISTED"), "{stdout} {stderr}");
        assert!(stdout.contains("READONLY"), "a read root is never writable");
        assert!(!readable.path().join("new").exists());
    }

    #[tokio::test]
    async fn binding_is_refused_except_on_the_given_ports() {
        if !landlock_or_skip("binding_is_refused_except_on_the_given_ports") {
            return;
        }
        if !std::path::Path::new("/usr/bin/python3").exists() {
            eprintln!("SKIPPED binding_is_refused_except_on_the_given_ports: no python3");
            return;
        }
        let free = |()| {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        };
        let (allowed, other) = (free(()), free(()));
        let sandbox = Sandbox {
            read_roots: shell_reads(),
            write_roots: vec![PathBuf::from("/dev/null")],
            tcp_bind: vec![allowed],
            ..Sandbox::default()
        };
        let script = format!(
            "import socket\nfor p in ({allowed}, {other}):\n    s = socket.socket()\n    \
             try:\n        s.bind(('127.0.0.1', p)); print('BOUND', p)\n    \
             except OSError as e:\n        print('REFUSED', p, e.errno)\n"
        );
        let mut cmd = tokio::process::Command::new("/usr/bin/python3");
        cmd.arg("-I").arg("-c").arg(&script);
        let output = sandbox.prepare().unwrap().output(cmd).await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains(&format!("BOUND {allowed}")),
            "{stdout} {stderr}"
        );
        assert!(
            stdout.contains(&format!("REFUSED {other}")),
            "{stdout} {stderr}"
        );
    }

    #[tokio::test]
    async fn a_child_reads_its_own_proc_but_no_other_process() {
        if !landlock_or_skip("a_child_reads_its_own_proc_but_no_other_process") {
            return;
        }
        let parent = std::process::id();
        let sandbox = Sandbox {
            read_roots: shell_reads(),
            write_roots: vec![PathBuf::from("/dev/null")],
            ..Sandbox::default()
        };
        let script = format!(
            "read -r own < /proc/self/stat && echo OWN; \
             head -c 1 /proc/{parent}/environ >/dev/null 2>&1 || echo OTHER-DENIED; \
             head -c 1 /proc/{parent}/cmdline >/dev/null 2>&1 || echo CMDLINE-DENIED"
        );
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(&script);
        let output = sandbox.prepare().unwrap().output(cmd).await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("OWN"), "{stdout} {stderr}");
        assert!(stdout.contains("OTHER-DENIED"), "{stdout} {stderr}");
        assert!(stdout.contains("CMDLINE-DENIED"), "{stdout} {stderr}");
        // The rule names the child's own `/proc/<pid>` (the shell reads it
        // with a redirect); a grandchild has another pid and no such rule.
    }

    #[tokio::test]
    async fn a_background_child_runs_niced() {
        if !landlock_or_skip("a_background_child_runs_niced") {
            return;
        }
        let sandbox = Sandbox {
            read_roots: shell_reads(),
            write_roots: vec![PathBuf::from("/dev/null")],
            background: true,
            ..Sandbox::default()
        };
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("read -r stat < /proc/self/stat; echo \"$stat\"");
        let output = sandbox.prepare().unwrap().output(cmd).await.unwrap();
        let stat = String::from_utf8_lossy(&output.stdout);
        // Field 19 of /proc/<pid>/stat is the nice value (after the comm).
        let after_comm = stat.rsplit_once(") ").map(|(_, rest)| rest).unwrap_or("");
        let nice: i32 = after_comm
            .split_whitespace()
            .nth(16)
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        assert!(nice >= 10, "{stat}");
    }

    #[tokio::test]
    async fn output_past_the_bound_is_stopped_and_refused() {
        if !landlock_or_skip("output_past_the_bound_is_stopped_and_refused") {
            return;
        }
        let sandbox = Sandbox {
            read_roots: shell_reads(),
            write_roots: vec![PathBuf::from("/dev/null"), PathBuf::from("/dev/zero")],
            ..Sandbox::default()
        };
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("cat /dev/zero")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = sandbox.prepare().unwrap().spawn(cmd).await.unwrap();
        let waited = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            wait_bounded(child, 64 * 1024, 1024),
        )
        .await
        .expect("the bound stops the child");
        assert!(waited.is_err());
    }

    #[tokio::test]
    async fn what_a_child_leaves_running_dies_with_its_group() {
        if !landlock_or_skip("what_a_child_leaves_running_dies_with_its_group") {
            return;
        }
        let pids = tempfile::tempdir().unwrap();
        let sandbox = Sandbox {
            read_roots: shell_reads(),
            write_roots: vec![PathBuf::from("/dev/null"), pids.path().to_path_buf()],
            ..Sandbox::default()
        };
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!(
                "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}/bg'",
                pids.path().display()
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = sandbox.prepare().unwrap().spawn(cmd).await.unwrap();
        let output = wait_bounded(child, 1024, 1024).await.unwrap();
        assert!(output.status.success());
        let bg: i32 = std::fs::read_to_string(pids.path().join("bg"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The guard inside wait_bounded has dropped: the background job is gone.
        let mut gone = false;
        for _ in 0..50 {
            // SAFETY: signal 0 only checks that the pid exists.
            if unsafe { libc::kill(bg, 0) } != 0 {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(gone, "background job {bg} survived its group");
    }
}
