//! Test-only: writing a stand-in CLI binary that a test then spawns.
//!
//! Tests run on many threads, and any thread may fork a child (every CLI
//! test does). A child forked while this thread still holds the script open
//! for writing inherits that descriptor until it execs, and exec of a file
//! open for writing fails with `ETXTBSY` ("Text file busy"). So the script
//! is written to a temporary name, synced and closed, renamed into place,
//! and then run once as a probe, retrying on `ETXTBSY`, until it execs.
//! Once it has, no process can hold it open for writing: our own descriptor
//! is closed, so no later fork can inherit one.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The variable the probe run sets; every fake script exits at once on it.
pub(crate) const PROBE_ENV: &str = "PULSE_FAKE_CLI_PROBE";

/// The first line every fake script starts with after the shebang.
pub(crate) const PROBE_GUARD: &str = "[ -n \"$PULSE_FAKE_CLI_PROBE\" ] && exit 0";

/// `ETXTBSY` on Linux.
const TEXT_FILE_BUSY: i32 = 26;

/// How often, and how far apart, the probe retries `ETXTBSY`.
const PROBE_ATTEMPTS: u32 = 200;
const PROBE_BACKOFF: Duration = Duration::from_millis(10);

/// Write `body` (a bash script, without the shebang) to `path` as an
/// executable, and return once it is safe to spawn.
pub(crate) fn write_script(path: &Path, body: &str) {
    let tmp = path.with_extension("partial");
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&tmp)
            .unwrap();
        write!(file, "#!/usr/bin/env bash\n{PROBE_GUARD}\n{body}\n").unwrap();
        file.sync_all().unwrap();
    }
    std::fs::rename(&tmp, path).unwrap();
    wait_until_it_execs(path);
}

fn wait_until_it_execs(path: &Path) {
    for _ in 0..PROBE_ATTEMPTS {
        let probe = Command::new(path)
            .env(PROBE_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match probe {
            Ok(_) => return,
            Err(e) if e.raw_os_error() == Some(TEXT_FILE_BUSY) => {
                std::thread::sleep(PROBE_BACKOFF);
            }
            Err(e) => panic!("cannot run the fake CLI {}: {e}", path.display()),
        }
    }
    panic!("the fake CLI {} stayed busy", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_script_runs_and_the_probe_left_no_trace() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake");
        let marker = dir.path().join("ran");
        write_script(&bin, &format!("touch '{}'", marker.display()));
        assert!(!marker.exists(), "the probe ran the script's body");
        assert!(!dir.path().join("fake.partial").exists());
        let status = Command::new(&bin).status().unwrap();
        assert!(status.success());
        assert!(marker.exists());
    }
}
