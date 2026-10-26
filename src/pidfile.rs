//! The daemon's pidfile (`.pulse-null.pid`).
//!
//! A current file is one JSON line: the pid, the process start time, the pid
//! namespace, the boot id, the real uid, and the systemd unit when this
//! process was started as a service. `down` signals only a record that still
//! names that same process, and only through a pidfd opened before the
//! check. An older file that is just the pid is [`Recorded::Legacy`] and is
//! never signalled.

use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const PID_FILENAME: &str = ".pulse-null.pid";
const PID_NAME: &CStr = c".pulse-null.pid";
/// A pidfile is one short JSON line. Anything larger is not a record we wrote.
const MAX_PIDFILE_BYTES: u64 = 4096;

/// One JSON line written by [`write`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidRecord {
    pub v: u8,
    pub pid: u32,
    /// `/proc/<pid>/stat` field 22, the process start time.
    pub start: u64,
    /// Inode of the process's pid namespace (`/proc/self/ns/pid`, followed).
    pub pidns: u64,
    pub boot_id: String,
    /// Real uid, the first number on the `Uid:` line of `/proc/<pid>/status`.
    pub uid: u32,
    /// Set only when `INVOCATION_ID` is present and the cgroup names a service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// What [`read`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    Record(PidRecord),
    Legacy(u32),
}

impl Recorded {
    pub(crate) fn pid(&self) -> u32 {
        match self {
            Self::Legacy(pid) | Self::Record(PidRecord { pid, .. }) => *pid,
        }
    }
}

/// A process pinned by `pidfd_open`. Dropping it closes the handle.
#[derive(Debug)]
pub struct Pidfd {
    fd: OwnedFd,
}

impl AsRawFd for Pidfd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// The outcome of [`verify`].
#[derive(Debug)]
pub enum Verdict {
    Ours(Pidfd),
    Gone,
    Mismatch(&'static str),
    Legacy(u32),
    Unverifiable(String),
}

/// The `/proc` facts [`verify`] needs. Tests supply a fake.
pub trait ProcView {
    fn boot_id(&self) -> io::Result<String>;
    fn pidns(&self) -> io::Result<u64>;
    fn start_time(&self, pid: u32) -> io::Result<u64>;
    fn uid(&self, pid: u32) -> io::Result<u32>;
    fn open_pidfd(&self, pid: u32) -> io::Result<Pidfd>;
}

/// `/proc` as this process sees it.
pub struct LiveProc;

impl ProcView for LiveProc {
    fn boot_id(&self) -> io::Result<String> {
        let text = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        Ok(text.trim().to_string())
    }

    fn pidns(&self) -> io::Result<u64> {
        // The path is a symlink. Follow it: the target inode is the
        // namespace, and it is the same for every process in that namespace.
        // The symlink's own inode is not.
        Ok(fs::metadata("/proc/self/ns/pid")?.ino())
    }

    fn start_time(&self, pid: u32) -> io::Result<u64> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        parse_start(&stat)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "process start time"))
    }

    fn uid(&self, pid: u32) -> io::Result<u32> {
        let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
        parse_uid(&status).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "uid"))
    }

    fn open_pidfd(&self, pid: u32) -> io::Result<Pidfd> {
        let pid =
            i32::try_from(pid).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid"))?;
        // SAFETY: pidfd_open takes a pid and flags and returns a descriptor or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let raw = i32::try_from(raw).map_err(|_| io::Error::other("pidfd"))?;
        // SAFETY: `raw` is a pidfd this function just opened.
        Ok(Pidfd {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        })
    }
}

/// Path of the pidfile inside a pulse root.
pub fn path(root_dir: &Path) -> PathBuf {
    root_dir.join(PID_FILENAME)
}

/// Write this process's record. A symlink at the pidfile name is replaced,
/// not followed: the bytes go to a new file created with `O_EXCL|O_NOFOLLOW`
/// and renamed over the name.
pub fn write(root_dir: &Path) -> io::Result<()> {
    write_record(root_dir, &self_record()?)
}

/// Write `rec` the same way [`write`] does.
pub(crate) fn write_record(root_dir: &Path, rec: &PidRecord) -> io::Result<()> {
    let mut line =
        serde_json::to_vec(rec).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    line.push(b'\n');
    replace(root_dir, &line)
}

/// Read the pidfile without following a symlink. `None` when it is missing,
/// a symlink, not version 1, or neither a record nor a bare pid.
pub fn read(root_dir: &Path) -> Option<Recorded> {
    parse_recorded(&read_bytes(root_dir).ok()?)
}

/// Remove the pidfile when it is a record for this process. A legacy file,
/// or a record for some other pid, is left in place.
pub fn remove(root_dir: &Path) {
    if let Some(Recorded::Record(rec)) = read(root_dir) {
        if rec.pid == std::process::id() {
            delete(root_dir);
        }
    }
}

/// Unlink the pidfile name. A symlink is removed as a directory entry, not
/// followed. Used when the record is [`Verdict::Gone`].
pub(crate) fn delete(root_dir: &Path) {
    let Ok(dir) = open_directory(root_dir) else {
        return;
    };
    // SAFETY: `dir` is an open directory. unlinkat removes this directory
    // entry and does not follow a symlink at the name.
    unsafe { libc::unlinkat(dir.as_raw_fd(), PID_NAME.as_ptr(), 0) };
}

/// Check the record against `sys`.
///
/// Order: refuse pid 0 and 1, then the boot id (a different boot is
/// [`Verdict::Gone`], an unreadable boot is [`Verdict::Unverifiable`]), then
/// the pid namespace. Only then open a pidfd, and only then re-read the
/// start time and real uid. The re-read is of the process the pidfd names;
/// a start time that no longer matches is a reused pid and is not signalled.
pub fn verify(rec: &PidRecord, sys: &dyn ProcView) -> Verdict {
    if rec.v != 1 {
        return Verdict::Unverifiable(format!("pidfile version {}", rec.v));
    }
    if rec.pid <= 1 || i32::try_from(rec.pid).is_err() {
        return Verdict::Mismatch("pid");
    }
    match sys.boot_id() {
        Ok(id) if id == rec.boot_id => {}
        Ok(_) => return Verdict::Gone,
        Err(err) => return Verdict::Unverifiable(err.to_string()),
    }
    match sys.pidns() {
        Ok(ns) if ns == rec.pidns => {}
        Ok(_) => return Verdict::Mismatch("pid namespace"),
        Err(err) => return Verdict::Unverifiable(err.to_string()),
    }
    let fd = match sys.open_pidfd(rec.pid) {
        Ok(fd) => fd,
        Err(err) if is_gone(&err) => return Verdict::Gone,
        Err(err) => return Verdict::Unverifiable(err.to_string()),
    };
    if let Err(verdict) = confirm(sys.start_time(rec.pid), rec.start, "start time") {
        return verdict;
    }
    if let Err(verdict) = confirm(sys.uid(rec.pid), rec.uid, "uid") {
        return verdict;
    }
    Verdict::Ours(fd)
}

/// [`verify`] for a whole file, including a legacy bare pid.
pub(crate) fn judge(recorded: &Recorded, sys: &dyn ProcView) -> Verdict {
    match recorded {
        Recorded::Legacy(pid) => Verdict::Legacy(*pid),
        Recorded::Record(rec) => verify(rec, sys),
    }
}

/// Why `pid` must not be signalled, for `down` and `status`.
pub(crate) fn refusal(pid: u32, verdict: &Verdict) -> String {
    match verdict {
        Verdict::Mismatch(reason) => format!("will not signal pid {pid}: {reason}"),
        Verdict::Legacy(legacy) => format!(
            "will not signal pid {legacy}: legacy pidfile; stop it with systemctl or by hand"
        ),
        Verdict::Unverifiable(detail) => {
            format!("will not signal pid {pid}: {detail}; stop it with systemctl or kill")
        }
        Verdict::Ours(_) | Verdict::Gone => format!("will not signal pid {pid}"),
    }
}

/// `SIGTERM` through the pidfd. A reused pid is not reached.
pub fn terminate(fd: &Pidfd) -> io::Result<()> {
    // SAFETY: a signal through the pidfd: it reaches that process or none.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Wait until the process pinned by `fd` exits, or `timeout` passes.
/// `Ok(true)` once it has exited. The pidfd becomes readable on exit, so a
/// reused pid number cannot look like the same process.
pub(crate) fn wait_exit(fd: &Pidfd, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1);
        let mut pollfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pollfd` refers to one descriptor this process owns.
        let rc = unsafe { libc::poll(&mut pollfd, 1, ms) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if rc == 0 {
            return Ok(false);
        }
        if pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            return Ok(true);
        }
        if pollfd.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "pidfd"));
        }
    }
}

/// The service name at the end of a cgroup line, when `invocation_id` is set.
fn unit_from_cgroup(cgroup: &str, invocation_id: bool) -> Option<String> {
    if !invocation_id {
        return None;
    }
    let path = cgroup.rsplit(':').next()?;
    let name = path.rsplit('/').next()?;
    name.ends_with(".service").then(|| name.to_string())
}

/// Whether a process with `pid` still has a `/proc` directory.
pub fn is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// `SIGTERM` by pid. Surreal's own pidfile still uses this; `down` does not.
pub fn kill(pid: u32) -> bool {
    // SAFETY: sending SIGTERM to a process ID is a standard Unix operation.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 }
}

fn self_record() -> io::Result<PidRecord> {
    let live = LiveProc;
    let pid = std::process::id();
    let invocation = std::env::var_os("INVOCATION_ID").is_some();
    let unit = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| unit_from_cgroup(line, invocation))
        });
    Ok(PidRecord {
        v: 1,
        pid,
        start: live.start_time(pid)?,
        pidns: live.pidns()?,
        boot_id: live.boot_id()?,
        uid: live.uid(pid)?,
        unit,
    })
}

/// Field 22 of `/proc/<pid>/stat`. `comm` may contain spaces and parentheses,
/// so the fields after the last `)` are what counts.
fn parse_start(stat: &str) -> Option<u64> {
    let rest = stat.rsplit_once(')')?.1.trim_start();
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Real uid: the first number on the `Uid:` line.
fn parse_uid(status: &str) -> Option<u32> {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

fn confirm<T: PartialEq>(got: io::Result<T>, want: T, why: &'static str) -> Result<(), Verdict> {
    match got {
        Ok(value) if value == want => Ok(()),
        Ok(_) => Err(Verdict::Mismatch(why)),
        Err(err) if is_gone(&err) => Err(Verdict::Gone),
        Err(err) => Err(Verdict::Unverifiable(err.to_string())),
    }
}

/// `pidfd_open` returns `ESRCH` for a pid that is gone. A later `/proc` read
/// returns `ENOENT` when the directory disappeared before the re-read.
fn is_gone(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::ESRCH | libc::ENOENT))
        || err.kind() == io::ErrorKind::NotFound
}

fn parse_recorded(bytes: &[u8]) -> Option<Recorded> {
    let text = std::str::from_utf8(bytes).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(rec) = serde_json::from_str::<PidRecord>(trimmed) {
        return (rec.v == 1).then_some(Recorded::Record(rec));
    }
    Some(Recorded::Legacy(trimmed.parse().ok()?))
}

fn read_bytes(root: &Path) -> io::Result<Vec<u8>> {
    let dir = open_directory(root)?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open directory. `O_NOFOLLOW` refuses a symlink at
    // the pidfile name, so a link cannot redirect the read.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), PID_NAME.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` succeeded, so this process owns `fd`.
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pidfile is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_PIDFILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PIDFILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pidfile is too large",
        ));
    }
    Ok(bytes)
}

fn replace(root: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = open_directory(root)?;
    let name = CString::new(temp_name())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "temporary name"))?;
    // Armed before create, so a later error still unlinks the temp file.
    let mut temp = TempFile {
        dirfd: dir.as_raw_fd(),
        name: Some(name),
    };
    let file = create_exclusive(dir.as_raw_fd(), temp.name.as_deref().unwrap())?;
    fchmod_0644(&file)?;
    (&file).write_all(bytes)?;
    rename_over(dir.as_raw_fd(), temp.name.as_deref().unwrap())?;
    temp.name = None;
    Ok(())
}

fn temp_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".pulse-null.pid.{}.{nanos}", std::process::id())
}

fn open_directory(root: &Path) -> io::Result<OwnedFd> {
    let c_path = CString::new(root.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "pulse root contains a NUL byte",
        )
    })?;
    // SAFETY: `c_path` is NUL-terminated. The result is checked and owned below.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` succeeded, so this process owns `fd`.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn create_exclusive(dirfd: RawFd, name: &CStr) -> io::Result<File> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dirfd` is an open directory and `name` is NUL-terminated.
    // `O_EXCL|O_NOFOLLOW` refuses a symlink at this name.
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags, 0o644) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` succeeded, so this process owns `fd`.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn fchmod_0644(file: &File) -> io::Result<()> {
    // SAFETY: `file` is a descriptor this process owns. Create mode is
    // masked by umask, so set 0644 on the inode itself.
    let rc = unsafe { libc::fchmod(file.as_raw_fd(), 0o644) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn rename_over(dirfd: RawFd, from: &CStr) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated children of this directory fd.
    // renameat replaces the destination entry and does not follow a symlink.
    let rc = unsafe { libc::renameat(dirfd, from.as_ptr(), dirfd, PID_NAME.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Unlinks its name unless [`replace`] cleared it after a successful rename.
struct TempFile {
    dirfd: RawFd,
    name: Option<CString>,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            // SAFETY: `name` is the temp file this write created in `dirfd`.
            unsafe { libc::unlinkat(self.dirfd, name.as_ptr(), 0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs::File;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn sample() -> PidRecord {
        PidRecord {
            v: 1,
            pid: 4242,
            start: 100,
            pidns: 7,
            boot_id: "boot-a".into(),
            uid: 1000,
            unit: None,
        }
    }

    struct Script {
        boot: Result<String, i32>,
        pidns: Result<u64, i32>,
        start: Result<u64, i32>,
        uid: Result<u32, i32>,
        open: Result<(), i32>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Script {
        fn matching(rec: &PidRecord) -> Self {
            Self {
                boot: Ok(rec.boot_id.clone()),
                pidns: Ok(rec.pidns),
                start: Ok(rec.start),
                uid: Ok(rec.uid),
                open: Ok(()),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn note(&self, name: &'static str) {
            self.calls.borrow_mut().push(name);
        }

        fn saw(&self, name: &str) -> bool {
            self.calls.borrow().contains(&name)
        }

        fn before(&self, first: &str, second: &str) {
            let calls = self.calls.borrow();
            let a = calls.iter().position(|call| *call == first).unwrap();
            let b = calls.iter().position(|call| *call == second).unwrap();
            assert!(a < b, "{calls:?}");
        }
    }

    impl ProcView for Script {
        fn boot_id(&self) -> io::Result<String> {
            self.note("boot_id");
            self.boot.clone().map_err(io::Error::from_raw_os_error)
        }

        fn pidns(&self) -> io::Result<u64> {
            self.note("pidns");
            self.pidns.map_err(io::Error::from_raw_os_error)
        }

        fn start_time(&self, _pid: u32) -> io::Result<u64> {
            self.note("start_time");
            self.start.map_err(io::Error::from_raw_os_error)
        }

        fn uid(&self, _pid: u32) -> io::Result<u32> {
            self.note("uid");
            self.uid.map_err(io::Error::from_raw_os_error)
        }

        fn open_pidfd(&self, _pid: u32) -> io::Result<Pidfd> {
            self.note("open_pidfd");
            self.open.map_err(io::Error::from_raw_os_error)?;
            let file = File::open("/dev/null")?;
            Ok(Pidfd {
                fd: OwnedFd::from(file),
            })
        }
    }

    #[test]
    fn pidfile_round_trips_self_record() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path()).unwrap();
        let mode = fs::metadata(path(dir.path())).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o644);
        let Recorded::Record(rec) = read(dir.path()).unwrap() else {
            panic!("self record was read as legacy");
        };
        assert_eq!(rec.v, 1);
        assert_eq!(rec.pid, std::process::id());
        let followed = fs::metadata("/proc/self/ns/pid").unwrap().ino();
        assert_eq!(rec.pidns, followed);
        assert_ne!(
            followed,
            fs::symlink_metadata("/proc/self/ns/pid").unwrap().ino()
        );
        assert!(matches!(verify(&rec, &LiveProc), Verdict::Ours(_)));

        fs::write(path(dir.path()), "1").unwrap();
        assert_eq!(read(dir.path()), Some(Recorded::Legacy(1)));
        let wrong_version = r#"{"v":2,"pid":2,"start":1,"pidns":1,"boot_id":"x","uid":1}"#;
        fs::write(path(dir.path()), wrong_version).unwrap();
        assert!(read(dir.path()).is_none());
    }

    #[test]
    fn pid_one_and_zero_are_refused() {
        for pid in [0, 1, u32::MAX] {
            let mut rec = sample();
            rec.pid = pid;
            let script = Script::matching(&rec);
            assert!(matches!(verify(&rec, &script), Verdict::Mismatch("pid")));
            assert!(script.calls.borrow().is_empty(), "pid {pid} opened a pidfd");
        }
    }

    #[test]
    fn start_time_mismatch_is_refused() {
        let mut after = Vec::new();
        after.push("S".to_string());
        for n in 1..19 {
            after.push(n.to_string());
        }
        after.push("12345".to_string());
        let stat = format!("42 (we)ird (name) {}", after.join(" "));
        assert_eq!(parse_start(&stat), Some(12345));

        let rec = sample();
        let mut script = Script::matching(&rec);
        script.start = Ok(rec.start + 1);
        assert!(matches!(
            verify(&rec, &script),
            Verdict::Mismatch("start time")
        ));
        script.before("open_pidfd", "start_time");
        assert!(!script.saw("uid"));
    }

    #[test]
    fn different_boot_id_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let rec = sample();
        write_record(dir.path(), &rec).unwrap();
        let mut script = Script::matching(&rec);
        script.boot = Ok("other-boot".into());
        assert!(matches!(verify(&rec, &script), Verdict::Gone));
        assert!(!script.saw("open_pidfd"));
        assert!(crate::cli::down::stop_with(dir.path(), &script).is_ok());
        assert!(!path(dir.path()).exists());
        assert!(!script.saw("open_pidfd"));
    }

    #[test]
    fn different_pidns_is_refused() {
        let rec = sample();
        let mut script = Script::matching(&rec);
        script.pidns = Ok(rec.pidns + 1);
        assert!(matches!(
            verify(&rec, &script),
            Verdict::Mismatch("pid namespace")
        ));
        assert!(!script.saw("open_pidfd"));
    }

    #[test]
    fn uid_mismatch_is_refused() {
        assert_eq!(parse_uid("Name:\tcat\nUid:\t1000\t4\t4\t4\n"), Some(1000));
        let rec = sample();
        let mut script = Script::matching(&rec);
        script.uid = Ok(rec.uid + 1);
        assert!(matches!(verify(&rec, &script), Verdict::Mismatch("uid")));
        script.before("open_pidfd", "start_time");
        script.before("start_time", "uid");
    }

    #[test]
    fn systemd_unit_from_cgroup_line() {
        let line = "0::/system.slice/pulse-null@echo.service";
        assert_eq!(
            unit_from_cgroup(line, true).as_deref(),
            Some("pulse-null@echo.service")
        );
        assert_eq!(unit_from_cgroup(line, false), None);
        assert_eq!(
            unit_from_cgroup("0::/user.slice/session-1.scope", true),
            None
        );
    }

    #[test]
    fn write_replaces_symlinked_pidfile_without_following() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, path(dir.path())).unwrap();
        assert!(read(dir.path()).is_none(), "a symlink is not a pid");
        write(dir.path()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        let meta = fs::symlink_metadata(path(dir.path())).unwrap();
        assert!(meta.is_file());
        assert!(matches!(read(dir.path()), Some(Recorded::Record(_))));
    }

    #[test]
    fn unreadable_proc_is_unverifiable() {
        let dir = tempfile::tempdir().unwrap();
        let rec = sample();
        write_record(dir.path(), &rec).unwrap();
        let mut script = Script::matching(&rec);
        script.boot = Err(libc::EPERM);
        assert!(matches!(verify(&rec, &script), Verdict::Unverifiable(_)));
        let err = crate::cli::down::stop_with(dir.path(), &script).unwrap_err();
        assert!(err.contains("stop it with systemctl or kill"), "{err}");
        assert!(path(dir.path()).exists());
        assert!(!script.saw("open_pidfd"));
    }
}
