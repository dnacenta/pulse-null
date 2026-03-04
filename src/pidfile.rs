use std::fs;
use std::path::{Path, PathBuf};

const PID_FILENAME: &str = ".pulse-null.pid";

/// Get the PID file path for an entity root directory.
pub fn path(root_dir: &Path) -> PathBuf {
    root_dir.join(PID_FILENAME)
}

/// Write the current process PID to the PID file.
pub fn write(root_dir: &Path) -> std::io::Result<()> {
    let pid = std::process::id();
    fs::write(path(root_dir), pid.to_string())
}

/// Read the PID from the PID file, if it exists and is valid.
pub fn read(root_dir: &Path) -> Option<u32> {
    fs::read_to_string(path(root_dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Remove the PID file.
pub fn remove(root_dir: &Path) {
    let _ = fs::remove_file(path(root_dir));
}

/// Check if a process with the given PID is still alive.
pub fn is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{}", pid)).exists()
}

/// Send SIGTERM to the given PID. Returns true if the signal was sent.
pub fn kill(pid: u32) -> bool {
    // SAFETY: sending SIGTERM to a process ID is a standard Unix operation
    unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 }
}
