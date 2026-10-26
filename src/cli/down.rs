use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::pidfile::{self, Pidfd, ProcView, Recorded};

const STOP_WAIT: Duration = Duration::from_secs(10);
const SHARED_PROCESS: &str = "One process can serve several pulses; stopping it stops all of them.";

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    stop(&config.root_dir()?)?;
    Ok(())
}

pub(crate) fn stop(root: &Path) -> Result<(), String> {
    stop_with(root, &pidfile::LiveProc)
}

/// Decide whether `root`'s pidfile may be signalled. `sys` is `/proc` in
/// `down`, and a fake in tests.
///
/// The caller must be the uid stored in the record. Root is not that user:
/// the refusal names the uid and tells them to run
/// `sudo -u <user> pulse-null down`. Nothing is signalled and the file stays.
/// A systemd unit is never signalled. The `systemctl stop` line is printed
/// only for a [`pidfile::service_unit`]; any other unit text is omitted.
pub(crate) fn stop_with(root: &Path, sys: &dyn ProcView) -> Result<(), String> {
    let Some(recorded) = pidfile::read(root) else {
        eprintln!("No running pulse found (no PID file).");
        return Ok(());
    };
    if let Recorded::Record(rec) = &recorded {
        if let Some(unit) = rec.unit.as_deref().filter(|unit| !unit.is_empty()) {
            return Err(systemd_hint(unit));
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        let euid = unsafe { libc::geteuid() };
        if rec.uid != euid {
            return Err(other_user(rec.pid, rec.uid));
        }
    }
    let pid = recorded.pid();
    match pidfile::judge(&recorded, sys) {
        pidfile::Verdict::Ours(fd) => stop_ours(root, pid, &fd),
        pidfile::Verdict::Gone => {
            pidfile::delete(root);
            eprintln!("Pulse is not running (stale PID file, pid {pid}).");
            Ok(())
        }
        other => Err(pidfile::refusal(pid, &other)),
    }
}

/// `sudo systemctl stop <unit>` when `unit` is a plain service name.
/// Otherwise a fixed sentence that does not contain the raw value.
fn systemd_hint(unit: &str) -> String {
    if pidfile::service_unit(unit) {
        format!("sudo systemctl stop {unit}")
    } else {
        "pulse is managed by systemd; stop it with systemctl".to_string()
    }
}

/// Refusal when the record belongs to a uid other than the caller.
fn other_user(pid: u32, uid: u32) -> String {
    let user = passwd_name(uid)
        .filter(|name| safe_user(name))
        .unwrap_or_else(|| format!("#{uid}"));
    format!(
        "will not signal pid {pid}: record uid is {uid}; run `sudo -u {user} pulse-null down` as that user, or `systemctl stop <unit>` for a systemd pulse"
    )
}

fn safe_user(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphanumeric() || b == b'_' => {}
        _ => return false,
    }
    name.len() <= 32 && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn passwd_name(uid: u32) -> Option<String> {
    let mut buf = vec![0u8; 16 * 1024];
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes into `pwd` and `buf` and sets `result` to
    // `pwd` or null. The name is copied before `buf` drops.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: `result` points at `pwd`, which getpwuid_r has initialized.
    let name = unsafe { std::ffi::CStr::from_ptr((*result).pw_name) };
    name.to_str().ok().map(str::to_string)
}

fn stop_ours(root: &Path, pid: u32, fd: &Pidfd) -> Result<(), String> {
    println!("Stopping pulse (pid {pid})...");
    println!("{SHARED_PROCESS}");
    pidfile::terminate(fd).map_err(|err| format!("Failed to send SIGTERM to pid {pid}: {err}"))?;
    if pidfile::wait_exit(fd, STOP_WAIT)
        .map_err(|err| format!("Failed to wait for pid {pid}: {err}"))?
    {
        pidfile::delete(root);
        println!("Pulse stopped.");
    } else {
        eprintln!("Pulse did not stop within 10 seconds (pid {pid}).");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command, ExitStatus};

    use super::*;

    struct ChildGuard {
        child: Option<Child>,
    }

    impl ChildGuard {
        fn spawn() -> Self {
            let child = crate::process_state::during_fork(|| {
                Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .expect("spawn sleep")
            });
            Self { child: Some(child) }
        }

        fn id(&self) -> u32 {
            self.child.as_ref().unwrap().id()
        }

        fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            self.child.as_mut().unwrap().try_wait()
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn write_for(dir: &Path, pid: u32, unit: Option<&str>) {
        let live = pidfile::LiveProc;
        let rec = pidfile::PidRecord {
            v: 1,
            pid,
            start: live.start_time(pid).expect("child start time"),
            pidns: live.pidns().expect("pid namespace"),
            boot_id: live.boot_id().expect("boot id"),
            uid: live.uid(pid).expect("child uid"),
            unit: unit.map(str::to_string),
        };
        pidfile::write_record(dir, &rec).unwrap();
    }

    #[test]
    fn down_stops_a_verified_child() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = ChildGuard::spawn();
        write_for(dir.path(), child.id(), None);
        let result = stop_with(dir.path(), &pidfile::LiveProc);
        assert!(result.is_ok(), "{result:?}");
        let status = child
            .try_wait()
            .unwrap()
            .expect("verified child is still running");
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert!(pidfile::read(dir.path()).is_none());
    }

    #[test]
    fn down_on_systemd_record_prints_systemctl_and_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = ChildGuard::spawn();
        write_for(dir.path(), child.id(), Some("pulse-null@echo.service"));
        let err = stop_with(dir.path(), &pidfile::LiveProc).unwrap_err();
        assert_eq!(err, "sudo systemctl stop pulse-null@echo.service");
        assert!(child.try_wait().unwrap().is_none(), "signalled the child");
        assert!(pidfile::read(dir.path()).is_some());
    }

    #[test]
    fn legacy_bare_pid_is_never_signalled() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = ChildGuard::spawn();
        let pid = child.id();
        std::fs::write(pidfile::path(dir.path()), pid.to_string()).unwrap();
        let err = stop_with(dir.path(), &pidfile::LiveProc).unwrap_err();
        assert!(err.contains(&pid.to_string()), "{err}");
        assert!(err.contains("systemctl"), "{err}");
        assert!(err.contains("by hand"), "{err}");
        assert!(child.try_wait().unwrap().is_none(), "signalled the child");
        assert!(pidfile::path(dir.path()).is_file());

        let status = crate::cli::status::report(dir.path(), &pidfile::LiveProc).unwrap();
        assert!(!status.starts_with("RUNNING"), "{status}");
        assert!(status.contains(&pid.to_string()), "{status}");
        assert!(status.contains("by hand"), "{status}");
        assert!(
            child.try_wait().unwrap().is_none(),
            "status signalled the child"
        );
    }

    #[test]
    fn down_refuses_record_uid_that_is_not_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = ChildGuard::spawn();
        let live = pidfile::LiveProc;
        let pid = child.id();
        let uid = live.uid(pid).expect("child uid").wrapping_add(1);
        let rec = pidfile::PidRecord {
            v: 1,
            pid,
            start: live.start_time(pid).expect("child start time"),
            pidns: live.pidns().expect("pid namespace"),
            boot_id: live.boot_id().expect("boot id"),
            uid,
            unit: None,
        };
        pidfile::write_record(dir.path(), &rec).unwrap();
        let err = stop_with(dir.path(), &live).unwrap_err();
        assert!(err.contains(&uid.to_string()), "{err}");
        assert!(err.contains("sudo -u"), "{err}");
        assert!(err.contains("pulse-null down"), "{err}");
        assert!(err.contains("systemctl stop"), "{err}");
        assert!(child.try_wait().unwrap().is_none(), "signalled the child");
        assert!(pidfile::read(dir.path()).is_some());
    }

    #[test]
    fn forged_unit_with_shell_metacharacters_is_not_printed() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = ChildGuard::spawn();
        let raw = "pulse-null@x.service; touch /tmp/PWNED";
        write_for(dir.path(), child.id(), Some(raw));
        let (err, stdout, stderr) = captured(|| stop_with(dir.path(), &pidfile::LiveProc));
        let err = err.unwrap_err();
        assert!(!err.contains(raw), "{err}");
        assert!(!stdout.contains(raw), "{stdout}");
        assert!(!stderr.contains(raw), "{stderr}");
        assert_eq!(err, "pulse is managed by systemd; stop it with systemctl");
        assert!(child.try_wait().unwrap().is_none(), "signalled the child");
        assert!(pidfile::read(dir.path()).is_some());
    }

    /// Stdout and stderr of `body`, so a hint cannot hide on either stream.
    fn captured<T>(body: impl FnOnce() -> T) -> (T, String, String) {
        let _hold = crate::process_state::Hold::acquire();
        let mut out = [0; 2];
        let mut err = [0; 2];
        // SAFETY: pipe creates a connected pair. The result is checked.
        unsafe {
            assert_eq!(libc::pipe(out.as_mut_ptr()), 0);
            assert_eq!(libc::pipe(err.as_mut_ptr()), 0);
        }
        // SAFETY: dup copies a standard stream. A negative result is refused below.
        let saved_out = unsafe { libc::dup(libc::STDOUT_FILENO) };
        let saved_err = unsafe { libc::dup(libc::STDERR_FILENO) };
        assert!(saved_out >= 0 && saved_err >= 0);
        let restore = Restore {
            saved_out,
            saved_err,
            out_w: out[1],
            err_w: err[1],
        };
        // SAFETY: both descriptors are open. dup2 redirects the standard streams
        // onto the pipe write ends for the body of this test.
        unsafe {
            assert_eq!(libc::dup2(out[1], libc::STDOUT_FILENO), libc::STDOUT_FILENO);
            assert_eq!(libc::dup2(err[1], libc::STDERR_FILENO), libc::STDERR_FILENO);
        }
        let value = body();
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let _ = std::io::Write::flush(&mut std::io::stderr());
        drop(restore);
        let stdout = read_all(out[0]);
        let stderr = read_all(err[0]);
        // SAFETY: these are the pipe read ends this test opened.
        unsafe {
            libc::close(out[0]);
            libc::close(err[0]);
        }
        (value, stdout, stderr)
    }

    fn read_all(fd: i32) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            // SAFETY: `fd` is an open pipe read end and `tmp` is writable.
            let n = unsafe { libc::read(fd, tmp.as_mut_ptr().cast(), tmp.len()) };
            let Ok(n) = usize::try_from(n) else {
                break;
            };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    struct Restore {
        saved_out: i32,
        saved_err: i32,
        out_w: i32,
        err_w: i32,
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: the saved descriptors are the original standard streams,
            // and the write ends are the pipes this test opened.
            unsafe {
                libc::dup2(self.saved_out, libc::STDOUT_FILENO);
                libc::dup2(self.saved_err, libc::STDERR_FILENO);
                libc::close(self.saved_out);
                libc::close(self.saved_err);
                libc::close(self.out_w);
                libc::close(self.err_w);
            }
        }
    }
}
