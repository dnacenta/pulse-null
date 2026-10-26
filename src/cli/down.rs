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
pub(crate) fn stop_with(root: &Path, sys: &dyn ProcView) -> Result<(), String> {
    let Some(recorded) = pidfile::read(root) else {
        eprintln!("No running pulse found (no PID file).");
        return Ok(());
    };
    if let Recorded::Record(rec) = &recorded {
        if let Some(unit) = rec.unit.as_deref().filter(|unit| !unit.is_empty()) {
            return Err(format!("sudo systemctl stop {unit}"));
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
}
