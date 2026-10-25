//! `<ledger dir>/anchors.log`. Append one line and `sync_all`.
//!
//! The file is opened with `O_NOFOLLOW` on a directory descriptor. It must be
//! a regular file, one link, not group- or world-writable, and owned by root
//! when this command runs as root. Otherwise it must be owned by the user
//! running the command.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::{format_line, parse_history, Anchor, AnchorError, AnchorLine, BoxFuture};

const HISTORY_CAP: u64 = 2 * 1024 * 1024;

pub struct LocalFileAnchor {
    dir: std::path::PathBuf,
    /// `Some` replaces the owner rule. Tests use it.
    owner: Option<u32>,
}

impl LocalFileAnchor {
    #[must_use]
    pub fn new(ledger_dir: &Path) -> Self {
        Self {
            dir: ledger_dir.to_path_buf(),
            owner: None,
        }
    }

    /// Expect this uid instead of the usual owner rule.
    #[cfg(test)]
    #[must_use]
    pub fn expect_owner(ledger_dir: &Path, owner: u32) -> Self {
        Self {
            dir: ledger_dir.to_path_buf(),
            owner: Some(owner),
        }
    }
}

impl Anchor for LocalFileAnchor {
    fn name(&self) -> &'static str {
        "local"
    }

    fn history<'a>(
        &'a self,
        _pulse: &'a str,
    ) -> BoxFuture<'a, Result<Vec<AnchorLine>, AnchorError>> {
        let dir = self.dir.clone();
        let owner = self.owner;
        Box::pin(async move { read_history(&dir, owner) })
    }

    fn publish<'a>(&'a self, line: &'a AnchorLine) -> BoxFuture<'a, Result<(), AnchorError>> {
        let dir = self.dir.clone();
        let owner = self.owner;
        let text = format_line(line);
        Box::pin(async move { append_line(&dir, owner, &text) })
    }
}

/// Exclusive advisory lock on `<dir>/anchors.lock` for one `anchor` run.
///
/// The returned file holds the lock until it is dropped.
#[must_use = "the lock is released when the file is dropped"]
pub fn lock_ledger(dir: &Path) -> Result<File, AnchorError> {
    let dirfd = open_dir(dir)?;
    let file = open_at(
        dirfd.as_raw_fd(),
        "anchors.lock",
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        true,
    )?;
    let meta = file
        .metadata()
        .map_err(|_| AnchorError::Local("anchors.lock could not be checked".into()))?;
    if !meta.is_file() {
        return Err(AnchorError::Local(
            "anchors.lock must be a regular file".into(),
        ));
    }
    // SAFETY: `flock` on a descriptor this function owns.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(file);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) || err.raw_os_error() == Some(libc::EAGAIN) {
        Err(AnchorError::Busy)
    } else {
        Err(AnchorError::Local(
            "anchors.lock could not be locked".into(),
        ))
    }
}

fn read_history(dir: &Path, owner: Option<u32>) -> Result<Vec<AnchorLine>, AnchorError> {
    let dirfd = open_dir(dir)?;
    let file = match open_at(
        dirfd.as_raw_fd(),
        "anchors.log",
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        false,
    ) {
        Ok(file) => file,
        Err(AnchorError::Local(reason)) if reason == "missing" => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    check_file(&file, owner)?;
    let meta = file
        .metadata()
        .map_err(|_| AnchorError::Local("anchors.log could not be read".into()))?;
    if meta.len() > HISTORY_CAP {
        return Err(AnchorError::TooLarge);
    }
    let mut text = String::new();
    (&file)
        .take(HISTORY_CAP)
        .read_to_string(&mut text)
        .map_err(|_| AnchorError::Local("anchors.log could not be read".into()))?;
    parse_history(&text)
}

fn append_line(dir: &Path, owner: Option<u32>, text: &str) -> Result<(), AnchorError> {
    let dirfd = open_dir(dir)?;
    let mut file = open_at(
        dirfd.as_raw_fd(),
        "anchors.log",
        libc::O_RDWR | libc::O_APPEND | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        true,
    )?;
    check_file(&file, owner)?;
    let mut payload = Vec::with_capacity(text.len() + 1);
    payload.extend_from_slice(text.as_bytes());
    payload.push(b'\n');
    let wrote = file
        .write(&payload)
        .map_err(|_| AnchorError::Local("anchors.log could not be written".into()))?;
    if wrote != payload.len() {
        return Err(AnchorError::Local(
            "anchors.log could not be written in one write".into(),
        ));
    }
    file.sync_all()
        .map_err(|_| AnchorError::Local("anchors.log could not be written".into()))
}

fn open_dir(path: &Path) -> Result<File, AnchorError> {
    let bytes = path.as_os_str().as_encoded_bytes();
    let name = std::ffi::CString::new(bytes)
        .map_err(|_| AnchorError::Local("the receipt directory path is not usable".into()))?;
    // SAFETY: `name` is nul-terminated. `O_PATH` does not read the directory.
    // `O_NOFOLLOW` refuses a symlink at the directory itself.
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(AnchorError::Local(
            "the receipt directory is not a real directory".into(),
        ));
    }
    // SAFETY: `open` succeeded, so this process owns `fd`.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_at(dirfd: i32, name: &str, flags: i32, create: bool) -> Result<File, AnchorError> {
    let c_name = std::ffi::CString::new(name)
        .map_err(|_| AnchorError::Local(format!("{name} is not a usable name")))?;
    let mode = if create { 0o644 } else { 0 };
    // SAFETY: `dirfd` is an open directory this process owns, and `c_name` is
    // nul-terminated. `O_NOFOLLOW` refuses a symlink at this name.
    let fd = unsafe { libc::openat(dirfd, c_name.as_ptr(), flags, mode) };
    if fd >= 0 {
        // SAFETY: `openat` succeeded, so this process owns `fd`.
        return Ok(unsafe { File::from_raw_fd(fd) });
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ENOENT) {
        return Err(AnchorError::Local("missing".into()));
    }
    if err.raw_os_error() == Some(libc::ELOOP) {
        return Err(AnchorError::Local(format!(
            "{name} is a symlink; replace it with a regular file owned by root, mode 0644"
        )));
    }
    Err(AnchorError::Local(format!("{name} could not be opened")))
}

fn check_file(file: &File, owner: Option<u32>) -> Result<(), AnchorError> {
    let meta = file
        .metadata()
        .map_err(|_| AnchorError::Local("anchors.log could not be checked".into()))?;
    if !meta.is_file() {
        return Err(AnchorError::Local(
            "anchors.log must be a regular file".into(),
        ));
    }
    if meta.nlink() != 1 {
        return Err(AnchorError::Local(
            "anchors.log has more than one link; replace it with a single regular file".into(),
        ));
    }
    if !owner_ok(owner, meta.uid()) {
        return Err(AnchorError::Local(owner_message(owner).into()));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(AnchorError::Local(
            "anchors.log is writable by the group or others; set mode 0644".into(),
        ));
    }
    Ok(())
}

fn owner_ok(expected: Option<u32>, uid: u32) -> bool {
    match expected {
        Some(expected) => uid == expected,
        None if euid() == 0 => uid == 0,
        None => uid == euid(),
    }
}

fn owner_message(expected: Option<u32>) -> &'static str {
    let root = match expected {
        Some(uid) => uid == 0,
        None => euid() == 0,
    };
    if root {
        "anchors.log must be owned by root"
    } else {
        "anchors.log must be owned by the user running this command"
    }
}

fn euid() -> u32 {
    // SAFETY: `geteuid` does not change process state and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn sample(seq: u64) -> AnchorLine {
        AnchorLine {
            pulse: "echo".into(),
            ledger_id: "0123456789abcdef".into(),
            seq,
            head: "ab".repeat(32),
            ts: "2026-10-25T00:00:00.000Z".into(),
        }
    }

    fn mode(path: &Path, bits: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
    }

    #[tokio::test]
    async fn a_local_anchor_appends_syncs_and_reads_history() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = LocalFileAnchor::new(dir.path());
        assert!(anchor.history("echo").await.unwrap().is_empty());
        anchor.publish(&sample(1)).await.unwrap();
        anchor.publish(&sample(2)).await.unwrap();
        let history = anchor.history("echo").await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].seq, 2);
        let text = std::fs::read_to_string(dir.path().join("anchors.log")).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(text.lines().count(), 2);
        let mut torn = text.clone();
        torn.push_str("v1 partial");
        std::fs::write(dir.path().join("anchors.log"), &torn).unwrap();
        mode(&dir.path().join("anchors.log"), 0o644);
        let history = anchor.history("echo").await.unwrap();
        assert_eq!(history.len(), 2, "a partial tail is not a line");
    }

    #[tokio::test]
    async fn a_symlink_a_hard_link_a_writable_file_and_the_wrong_owner_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("anchors.log")).unwrap();
        let anchor = LocalFileAnchor::new(dir.path());
        let err = anchor.publish(&sample(1)).await.unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"");

        let dangling = tempfile::tempdir().unwrap();
        let missing = dangling.path().join("missing");
        std::os::unix::fs::symlink(&missing, dangling.path().join("anchors.log")).unwrap();
        let anchor = LocalFileAnchor::new(dangling.path());
        assert!(anchor.publish(&sample(1)).await.is_err());
        assert!(!missing.exists(), "a dangling symlink must not be created");

        let linked = tempfile::tempdir().unwrap();
        let log = linked.path().join("anchors.log");
        std::fs::write(&log, b"kept\n").unwrap();
        mode(&log, 0o644);
        std::fs::hard_link(&log, linked.path().join("extra")).unwrap();
        let anchor = LocalFileAnchor::new(linked.path());
        let err = anchor.publish(&sample(1)).await.unwrap_err();
        assert!(err.to_string().contains("more than one link"), "{err}");
        assert_eq!(std::fs::read(&log).unwrap(), b"kept\n");

        let open = tempfile::tempdir().unwrap();
        let log = open.path().join("anchors.log");
        std::fs::write(&log, b"kept\n").unwrap();
        mode(&log, 0o666);
        let anchor = LocalFileAnchor::new(open.path());
        let err = anchor.publish(&sample(1)).await.unwrap_err();
        assert!(err.to_string().contains("0644"), "{err}");
        assert_eq!(std::fs::read(&log).unwrap(), b"kept\n");

        let owned = tempfile::tempdir().unwrap();
        let log = owned.path().join("anchors.log");
        std::fs::write(&log, b"kept\n").unwrap();
        mode(&log, 0o644);
        let anchor = LocalFileAnchor::expect_owner(owned.path(), 0);
        let err = anchor.publish(&sample(1)).await.unwrap_err();
        assert!(err.to_string().contains("owned by root"), "{err}");
        assert_eq!(std::fs::read(&log).unwrap(), b"kept\n");
        assert!(anchor.history("echo").await.is_err());
    }

    #[test]
    fn a_second_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _held = lock_ledger(dir.path()).unwrap();
        let err = lock_ledger(dir.path()).unwrap_err();
        assert!(matches!(err, AnchorError::Busy), "{err}");
        assert!(err.to_string().contains("already running"), "{err}");
    }
}
