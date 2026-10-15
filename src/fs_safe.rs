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
    let Some((file, _)) = open_regular(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(refused(path, &format!("larger than {max} bytes")));
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
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
}
