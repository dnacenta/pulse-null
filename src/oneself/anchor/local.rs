//! `<ledger dir>/anchors.log`. Append one line and `sync_all`.
//!
//! The file is opened with `O_NOFOLLOW` on a directory descriptor. It must be
//! a regular file, one link, not group- or world-writable, and owned by root
//! when this command runs as root. Otherwise it must be owned by the user
//! running the command.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
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
    flock_exclusive(&file)?;
    Ok(file)
}

/// `LOCK_NB` once in production, so a second real run fails immediately.
/// Tests retry for about two seconds: a forked child can hold the descriptor
/// until it execs, and that window is not a second anchor run.
fn flock_exclusive(file: &File) -> Result<(), AnchorError> {
    #[cfg(test)]
    {
        flock_with_retry(file)
    }
    #[cfg(not(test))]
    {
        if lock_now(file)? {
            Ok(())
        } else {
            Err(AnchorError::Busy)
        }
    }
}

/// `Ok(true)` is locked. `Ok(false)` is busy.
fn lock_now(file: &File) -> Result<bool, AnchorError> {
    // SAFETY: `flock` on a descriptor this function owns.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    let busy =
        err.raw_os_error() == Some(libc::EWOULDBLOCK) || err.raw_os_error() == Some(libc::EAGAIN);
    if busy {
        Ok(false)
    } else {
        Err(AnchorError::Local(
            "anchors.lock could not be locked".into(),
        ))
    }
}

/// About two seconds of `LOCK_NB` when another process holds the lock.
///
/// A forked child can inherit the descriptor until it execs. That is not a
/// second anchor run. A lock already held in this process is refused at once,
/// which is what a second real run looks like.
#[cfg(test)]
fn flock_with_retry(file: &File) -> Result<(), AnchorError> {
    let mut left = 40u32;
    loop {
        if lock_now(file)? {
            return Ok(());
        }
        if held_by_this_process(file) {
            return Err(AnchorError::Busy);
        }
        left -= 1;
        if left == 0 {
            return Err(AnchorError::Busy);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// True when this process, not a child, is the one holding `file`'s lock.
#[cfg(test)]
fn held_by_this_process(file: &File) -> bool {
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let inode = meta.ino();
    let dev = meta.dev();
    let major = libc::major(dev);
    let minor = libc::minor(dev);
    let me = std::process::id();
    let Ok(text) = std::fs::read_to_string("/proc/locks") else {
        return false;
    };
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 || parts[1] != "FLOCK" || parts[3] != "WRITE" {
            continue;
        }
        let Ok(pid) = parts[4].parse::<u32>() else {
            continue;
        };
        let mut id = parts[5].split(':');
        let (Some(maj), Some(min), Some(ino)) = (id.next(), id.next(), id.next()) else {
            continue;
        };
        let Ok(maj) = u32::from_str_radix(maj, 16) else {
            continue;
        };
        let Ok(min) = u32::from_str_radix(min, 16) else {
            continue;
        };
        let Ok(ino) = ino.parse::<u64>() else {
            continue;
        };
        if pid == me && ino == inode && maj == major && min == minor {
            return true;
        }
    }
    false
}

fn read_history(dir: &Path, owner: Option<u32>) -> Result<Vec<AnchorLine>, AnchorError> {
    let dirfd = open_dir(dir)?;
    let file = match open_at(
        dirfd.as_raw_fd(),
        "anchors.log",
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
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
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(AnchorError::Local(partial_tail().into()));
    }
    parse_history(&text)
}

fn partial_tail() -> &'static str {
    "anchors.log ends without a newline; add the newline or remove the partial line, then run anchor again"
}

fn append_line(dir: &Path, owner: Option<u32>, text: &str) -> Result<(), AnchorError> {
    let dirfd = open_dir(dir)?;
    let mut file = open_at(
        dirfd.as_raw_fd(),
        "anchors.log",
        libc::O_RDWR
            | libc::O_APPEND
            | libc::O_CREAT
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC,
        true,
    )?;
    check_file(&file, owner)?;
    if !ends_with_newline(&mut file)? {
        return Err(AnchorError::Local(partial_tail().into()));
    }
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

fn ends_with_newline(file: &mut File) -> Result<bool, AnchorError> {
    let meta = file
        .metadata()
        .map_err(|_| AnchorError::Local("anchors.log could not be checked".into()))?;
    if meta.len() == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))
        .map_err(|_| AnchorError::Local("anchors.log could not be checked".into()))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)
        .map_err(|_| AnchorError::Local("anchors.log could not be checked".into()))?;
    Ok(byte[0] == b'\n')
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
        let log = dir.path().join("anchors.log");
        std::fs::write(&log, &torn).unwrap();
        mode(&log, 0o644);
        let err = anchor.history("echo").await.unwrap_err();
        assert!(err.to_string().contains("anchors.log"), "{err}");
        assert!(err.to_string().contains("newline"), "{err}");
        let err = anchor.publish(&sample(3)).await.unwrap_err();
        assert!(err.to_string().contains("anchors.log"), "{err}");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), torn);
    }

    #[tokio::test]
    async fn a_fifo_is_refused_before_a_blocking_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anchors.log");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `name` is a nul-terminated path that does not yet exist.
        let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
        let anchor = LocalFileAnchor::new(dir.path());
        let started = std::time::Instant::now();
        let history =
            tokio::time::timeout(std::time::Duration::from_secs(2), anchor.history("echo"))
                .await
                .expect("history must not block on a fifo");
        let err = history.unwrap_err();
        assert!(err.to_string().contains("regular file"), "{err}");
        let published = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            anchor.publish(&sample(1)),
        )
        .await
        .expect("publish must not block on a fifo");
        let err = published.unwrap_err();
        assert!(err.to_string().contains("regular file"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
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
        let _process = crate::process_state::Hold::acquire();
        let dir = tempfile::tempdir().unwrap();
        let _held = lock_ledger(dir.path()).unwrap();
        let started = std::time::Instant::now();
        let err = lock_ledger(dir.path()).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert!(matches!(err, AnchorError::Busy), "{err}");
        assert!(err.to_string().contains("already running"), "{err}");
    }
}
