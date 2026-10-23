//! Reading and writing files the pulse itself can write (PN-133).
//!
//! The pulse runs with write access to its own directory, so any file there
//! the runtime reads — on the boot path above all — may be a symlink out of
//! the directory, a FIFO that blocks the open forever, a device, or simply
//! huge. Every helper here opens with `O_NOFOLLOW | O_NONBLOCK` and checks
//! the file through the handle it got, so nothing can be swapped in between
//! the check and the use; writes go to a fresh `create_new` temp file that
//! is renamed into place, so a planted symlink at a fixed temp name cannot
//! redirect them.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Flags every open here carries: never follow a final symlink, never block
/// on a FIFO.
const SAFE_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_NONBLOCK;

fn refused(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{}: {why}", path.display()),
    )
}

/// Open `path` read-only if it is a regular file. `Ok(None)` when it does
/// not exist; an error when it is a symlink, a FIFO, a device or a
/// directory. Returns the handle and its metadata, so callers can apply
/// further checks (link count, owner) without a second lookup.
pub fn open_regular(path: &Path) -> io::Result<Option<(File, Metadata)>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(SAFE_FLAGS)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(refused(path, "is a symlink — refusing to follow it"))
        }
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(refused(path, "not a regular file"));
    }
    Ok(Some((file, meta)))
}

/// Read a regular file of at most `max` bytes as text (invalid UTF-8 is
/// replaced). `Ok(None)` when it does not exist; an error when it is not a
/// regular file or is larger than `max`.
pub fn read_bounded(path: &Path, max: u64) -> io::Result<Option<String>> {
    Ok(read_bounded_bytes(path, max)?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

/// [`read_bounded`], as bytes.
pub fn read_bounded_bytes(path: &Path, max: u64) -> io::Result<Option<Vec<u8>>> {
    let Some((file, _)) = open_regular(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(refused(path, &format!("larger than {max} bytes")));
    }
    Ok(Some(bytes))
}

/// The directory holding `rel` beneath `base`, opened one component at a
/// time without following any symlink (`O_NOFOLLOW` on every directory, not
/// just the last), so a link someone planted inside `base` can never lead
/// out of it. `base` itself is trusted. Returns the directory and the final
/// name.
fn dir_beneath(base: &Path, rel: &Path) -> io::Result<(std::os::fd::OwnedFd, std::ffi::CString)> {
    let (parts, name) = plain_parts(rel)?;
    let mut dir = open_base_dir(base)?;
    for part in parts {
        dir = open_dir_at(&dir, &c_name(rel, part)?)?;
    }
    Ok((dir, c_name(rel, name)?))
}

/// The directories and the final name of `rel`, which must be a plain
/// relative path (no `..`, no root, no prefix) naming a file.
fn plain_parts(rel: &Path) -> io::Result<(Vec<&std::ffi::OsStr>, &std::ffi::OsStr)> {
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for part in rel.components() {
        match part {
            std::path::Component::Normal(name) => parts.push(name),
            std::path::Component::CurDir => {}
            _ => return Err(refused(rel, "must be a plain relative path")),
        }
    }
    let name = parts.pop().ok_or_else(|| refused(rel, "names no file"))?;
    Ok((parts, name))
}

/// `part` of `rel` as a C string (refused when it holds a NUL byte).
fn c_name(rel: &Path, part: &std::ffi::OsStr) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(part.as_bytes()).map_err(|_| refused(rel, "has a NUL byte"))
}

/// Open the trusted `base` directory (it may be reached through a link).
fn open_base_dir(base: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    let base_c = std::ffi::CString::new(base.as_os_str().as_bytes())
        .map_err(|_| refused(base, "has a NUL byte"))?;
    // SAFETY: a NUL-terminated path; the result is checked and owned below.
    let fd = unsafe {
        libc::open(
            base_c.as_ptr(),
            libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_RDONLY,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a descriptor this function just opened.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Open the directory `name` in `dir`, refusing a symlink (`O_NOFOLLOW`).
fn open_dir_at(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::CStr,
) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_RDONLY;
    // SAFETY: `dir` is an open directory; `name` is NUL-terminated.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a descriptor this function just opened.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Read the regular file `rel` beneath `base` (at most `max` bytes), with
/// no symlink followed anywhere on the way ([`dir_beneath`]). `Ok(None)`
/// when it does not exist.
pub fn read_beneath(base: &Path, rel: &Path, max: u64) -> io::Result<Option<Vec<u8>>> {
    let Some(file) = open_beneath(base, rel)? else {
        return Ok(None);
    };
    read_regular(file, rel, max).map(Some)
}

/// Open `rel` beneath `base` read-only, with no symlink followed on the
/// way or at the end. `Ok(None)` when it, or a directory on the way, does
/// not exist.
fn open_beneath(base: &Path, rel: &Path) -> io::Result<Option<File>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let (dir, name) = match dir_beneath(base, rel) {
        Ok(found) => found,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // SAFETY: `dir` is an open directory; `name` is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | SAFE_FLAGS,
        )
    };
    if fd < 0 {
        return missing_is_none(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and is owned by the File.
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

/// `Ok(None)` for a file that does not exist, else the error.
fn missing_is_none<T>(e: io::Error) -> io::Result<Option<T>> {
    if e.kind() == io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(e)
    }
}

/// All of `file` (opened as `rel`), refused unless it is a regular file of
/// at most `max` bytes.
fn read_regular(file: File, rel: &Path, max: u64) -> io::Result<Vec<u8>> {
    if !file.metadata()?.is_file() {
        return Err(refused(rel, "not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(refused(rel, &format!("larger than {max} bytes")));
    }
    Ok(bytes)
}

/// Remove the entry `rel` beneath `base` (a file, or the link itself if it
/// is a symlink), with no symlink followed on the way ([`dir_beneath`]): a
/// directory swapped for a link out of `base` makes this fail, never reach
/// through. A missing entry is not an error.
pub fn unlink_beneath(base: &Path, rel: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let (dir, name) = match dir_beneath(base, rel) {
        Ok(found) => found,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    // SAFETY: `dir` is an open directory; `name` is NUL-terminated.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e);
        }
    }
    Ok(())
}

/// Open (creating if needed) a lock file for `flock`. Refuses a symlink or
/// anything that is not a regular file, so a planted FIFO cannot block the
/// open and a symlink cannot redirect the lock.
pub fn open_lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(SAFE_FLAGS)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => refused(path, "is a symlink — refusing to follow it"),
            _ => e,
        })?;
    if !file.metadata()?.is_file() {
        return Err(refused(path, "not a regular file"));
    }
    Ok(file)
}

/// Replace `path` with `content` atomically: write a fresh temp file next to
/// it (`create_new`, unique name, not following symlinks), then rename it
/// over `path`. A rename replaces a symlink at `path` rather than writing
/// through it.
pub fn write_atomic(path: &Path, content: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(content)?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_bounded_reads_a_small_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "hello").unwrap();
        assert_eq!(read_bounded(&path, 64).unwrap().as_deref(), Some("hello"));
        assert!(read_bounded(&dir.path().join("missing"), 64)
            .unwrap()
            .is_none());
    }

    #[test]
    fn read_bounded_refuses_symlinks_fifos_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "secret").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_bounded(&link, 64).is_err());

        let fifo = dir.path().join("fifo");
        let c = std::ffi::CString::new(fifo.to_string_lossy().into_owned()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path; mkfifo only creates a node.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(
            read_bounded(&fifo, 64).is_err(),
            "a FIFO must not block or read"
        );

        assert!(read_bounded(&target, 3).is_err());
    }

    #[test]
    fn open_lock_refuses_a_planted_fifo_or_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("lock");
        let c = std::ffi::CString::new(fifo.to_string_lossy().into_owned()).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(open_lock(&fifo).is_err());

        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "").unwrap();
        let link = dir.path().join("lock2");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_lock(&link).is_err());

        assert!(open_lock(&dir.path().join("fresh.lock")).is_ok());
    }

    /// A symlink planted where a temp file might go, or at the target
    /// itself, is never written through.
    #[test]
    fn write_atomic_never_writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, "untouched").unwrap();
        let path = dir.path().join("state.json");
        std::os::unix::fs::symlink(&outside, &path).unwrap();

        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    /// PN-144 R2-12: a directory inside the base swapped for a link out of
    /// it is never followed, to read or to unlink.
    #[test]
    fn beneath_reads_only_a_bounded_regular_file_by_a_plain_path() {
        use std::os::unix::ffi::OsStrExt;
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir(base.path().join("vendor")).unwrap();
        std::fs::write(base.path().join("vendor/login.json"), "copy").unwrap();
        let error = |rel: &Path, max: u64| read_beneath(base.path(), rel, max).unwrap_err();

        assert_eq!(
            read_beneath(base.path(), Path::new("./vendor/login.json"), 4).unwrap(),
            Some(b"copy".to_vec()),
            "a `.` component is plain"
        );
        assert!(error(Path::new("vendor/login.json"), 3)
            .to_string()
            .ends_with("larger than 3 bytes"));
        assert!(error(Path::new("vendor"), 1024)
            .to_string()
            .ends_with("not a regular file"));
        std::os::unix::fs::symlink("login.json", base.path().join("vendor/link.json")).unwrap();
        assert_eq!(
            error(Path::new("vendor/link.json"), 1024).raw_os_error(),
            Some(libc::ELOOP),
            "a link at the end is not followed either"
        );
        assert_eq!(
            read_beneath(base.path(), Path::new("vendor/missing.json"), 1024).unwrap(),
            None
        );
        assert_eq!(
            read_beneath(base.path(), Path::new("gone/login.json"), 1024).unwrap(),
            None
        );
        for rel in ["/etc/hosts", "vendor/../vendor/login.json"] {
            assert!(error(Path::new(rel), 1024)
                .to_string()
                .ends_with("must be a plain relative path"));
        }
        assert!(error(Path::new(""), 1024)
            .to_string()
            .ends_with("names no file"));
        let nul = Path::new(std::ffi::OsStr::from_bytes(b"vendor/lo\0gin.json"));
        assert!(error(nul, 1024).to_string().ends_with("has a NUL byte"));
        let nul_dir = Path::new(std::ffi::OsStr::from_bytes(b"ven\0dor/login.json"));
        assert!(error(nul_dir, 1024).to_string().ends_with("has a NUL byte"));
        let nul_base = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/ba\0se"));
        assert!(read_beneath(nul_base, Path::new("x"), 1)
            .unwrap_err()
            .to_string()
            .ends_with("has a NUL byte"));
        assert!(
            read_beneath(&base.path().join("vendor/login.json"), Path::new("x"), 1).is_err(),
            "a base that is not a directory"
        );
    }

    #[test]
    fn beneath_never_follows_a_link_out_of_the_base() {
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("login.json"), "live").unwrap();
        std::os::unix::fs::symlink(outside.path(), base.path().join("vendor")).unwrap();
        let rel = Path::new("vendor/login.json");
        assert!(read_beneath(base.path(), rel, 1024).is_err());
        assert!(unlink_beneath(base.path(), rel).is_err());
        assert!(
            outside.path().join("login.json").exists(),
            "the live file survives"
        );

        std::fs::remove_file(base.path().join("vendor")).unwrap();
        std::fs::create_dir(base.path().join("vendor")).unwrap();
        std::fs::write(base.path().join("vendor/login.json"), "copy").unwrap();
        assert_eq!(
            read_beneath(base.path(), rel, 1024).unwrap().unwrap(),
            b"copy"
        );
        unlink_beneath(base.path(), rel).unwrap();
        assert!(!base.path().join("vendor/login.json").exists());
        assert!(
            unlink_beneath(base.path(), rel).is_ok(),
            "a missing entry is fine"
        );
        assert!(read_beneath(base.path(), Path::new("../x"), 10).is_err());
    }
}
