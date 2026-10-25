//! Whether a receipt file is outside the pulse's write reach.
//!
//! Protected means every check is true: the fd came from systemd, the owner
//! is uid 0, the mode is not group- or world-writable, the append-only flag
//! is set, and the parent directory is owned by uid 0 and not group- or
//! world-writable. The daemon reads this when it opens the store. Tests use
//! [`FakeProbe`].

use std::fs::File;
use std::os::unix::fs::MetadataExt;

/// The five protection checks. `protected` is all of them true.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Protection {
    /// The fd was passed in by systemd, not opened from a path.
    pub systemd_fd: bool,
    /// File owner is uid 0.
    pub owner_root: bool,
    /// Not group-writable and not world-writable.
    pub mode_safe: bool,
    /// `FS_APPEND_FL` is set.
    pub append_flag: bool,
    /// Parent directory is uid 0 and not group- or world-writable.
    pub parent_safe: bool,
}

impl Protection {
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    #[must_use]
    pub fn is_protected(self) -> bool {
        self.systemd_fd && self.owner_root && self.mode_safe && self.append_flag && self.parent_safe
    }
}

/// How the store learns the protection state. The live probe looks at the
/// fd. A fake one returns a fixed report.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
pub trait ProtectionProbe {
    fn probe(&self, file: &File, from_systemd: bool) -> Protection;
}

/// `fstat`, the append-only flag, and the parent directory.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
#[derive(Debug, Default)]
pub struct LiveProbe;

impl ProtectionProbe for LiveProbe {
    fn probe(&self, file: &File, from_systemd: bool) -> Protection {
        let meta = file.metadata().ok();
        Protection {
            systemd_fd: from_systemd,
            owner_root: meta.as_ref().is_some_and(|meta| meta.uid() == 0),
            mode_safe: meta.as_ref().is_some_and(|meta| meta.mode() & 0o022 == 0),
            append_flag: append_flag(file),
            parent_safe: parent_is_safe(file),
        }
    }
}

/// A fixed report. `from_systemd` is ignored so a test can set each flag.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
#[derive(Debug, Clone)]
pub struct FakeProbe {
    pub systemd_fd: bool,
    pub owner_root: bool,
    pub mode_safe: bool,
    pub append_flag: bool,
    pub parent_safe: bool,
}

impl FakeProbe {
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    #[must_use]
    pub fn protected() -> Self {
        Self {
            systemd_fd: true,
            owner_root: true,
            mode_safe: true,
            append_flag: true,
            parent_safe: true,
        }
    }
}

impl ProtectionProbe for FakeProbe {
    fn probe(&self, _file: &File, _from_systemd: bool) -> Protection {
        Protection {
            systemd_fd: self.systemd_fd,
            owner_root: self.owner_root,
            mode_safe: self.mode_safe,
            append_flag: self.append_flag,
            parent_safe: self.parent_safe,
        }
    }
}

/// `_IOR('f', 1, long)` on 64-bit Linux (`linux/fs.h`).
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
const FS_IOC_GETFLAGS: libc::Ioctl = 0x8008_6601;

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
const FS_APPEND_FL: libc::c_long = 0x20;

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn append_flag(file: &File) -> bool {
    use std::os::fd::AsRawFd;
    let mut flags: libc::c_long = 0;
    // SAFETY: the request writes one `long` into `flags`. The fd is open.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) };
    rc == 0 && flags & FS_APPEND_FL != 0
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn append_flag(_file: &File) -> bool {
    false
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn parent_is_safe(file: &File) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(path) = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(parent) else {
        return false;
    };
    meta.uid() == 0 && meta.mode() & 0o022 == 0
}
