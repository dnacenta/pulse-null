//! `pulse-null receipts install`.
//!
//! Creates `<dir>/<pulse>/` as root:root mode 0755, `receipts.jsonl` as
//! root:`<group>` mode 0640 with one `genesis` row, the append-only flag on
//! that file, and an empty `anchors.log` as root:root mode 0644. Every
//! component is opened with `openat` and `O_NOFOLLOW`. A second run that
//! finds this layout changes nothing.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use super::chain::{self, GENESIS_PREV};
use super::event::ReceiptEvent;
use super::store::MAX_RECEIPT_BYTES;
use crate::config::validate::is_plain_pulse_name;

const DIR_MODE: u32 = 0o755;
const RECEIPT_MODE: u32 = 0o640;
const ANCHOR_MODE: u32 = 0o644;

/// What `install` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A genesis row was written into the receipt file.
    Installed(PathBuf),
    /// The receipt bytes were left as they were. The mode, the owner, the
    /// append-only flag, the directory, or `anchors.log` was adjusted.
    Adopted(PathBuf),
    /// The layout was already in place.
    Unchanged,
}

/// Why `install` wrote nothing, or stopped.
#[derive(Debug)]
pub enum InstallError {
    NotRoot,
    PulseName,
    GroupName,
    GroupNotFound,
    Symlink,
    OtherPulse,
    RefuseAdopt,
    Broken {
        line: usize,
        reason: &'static str,
    },
    /// `openat` returned `ENOENT`. Callers create the file or map this to a
    /// message. It is not returned from [`install`].
    Missing,
    Local(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRoot => formatter.write_str("install must run as root"),
            Self::PulseName => formatter.write_str("pulse must be letters, digits, - or _"),
            Self::GroupName => formatter.write_str("group must be letters, digits, - or _"),
            Self::GroupNotFound => formatter.write_str("group was not found"),
            Self::Symlink => formatter.write_str("will not follow a symlink"),
            Self::OtherPulse => formatter.write_str("genesis names another pulse"),
            Self::RefuseAdopt => {
                formatter.write_str("will not adopt an existing receipt file; pass --adopt")
            }
            Self::Broken { line, reason } => {
                write!(formatter, "broken line {line}: {reason}")
            }
            Self::Missing => formatter.write_str("a path component is missing"),
            Self::Local(text) => formatter.write_str(text),
        }
    }
}

impl std::error::Error for InstallError {}

/// Root check, group lookup, ownership, and the append-only flag.
///
/// Tests substitute a fake. The command uses [`LiveHost`].
pub trait Host {
    fn is_root(&self) -> bool;
    fn group_gid(&self, name: &str) -> Result<u32, InstallError>;
    fn owner(&self, file: &File) -> Result<(u32, u32), InstallError>;
    fn chown(&self, file: &File, uid: u32, gid: u32) -> Result<(), InstallError>;
    fn append_flag(&self, file: &File) -> Result<bool, InstallError>;
    fn set_append(&self, file: &File) -> Result<(), InstallError>;
}

/// The real uid, the group database, `fchown`, and `FS_APPEND_FL`.
#[derive(Debug, Default)]
pub struct LiveHost;

impl Host for LiveHost {
    fn is_root(&self) -> bool {
        // SAFETY: `geteuid` does not change process state and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    fn group_gid(&self, name: &str) -> Result<u32, InstallError> {
        lookup_group(name)
    }

    fn owner(&self, file: &File) -> Result<(u32, u32), InstallError> {
        let meta = file.metadata().map_err(io_err)?;
        Ok((meta.uid(), meta.gid()))
    }

    fn chown(&self, file: &File, uid: u32, gid: u32) -> Result<(), InstallError> {
        // SAFETY: `fchown` changes the owner of this open descriptor.
        let rc = unsafe { libc::fchown(file.as_raw_fd(), uid, gid) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io_err(io::Error::last_os_error()))
        }
    }

    fn append_flag(&self, file: &File) -> Result<bool, InstallError> {
        let flags = get_flags(file)?;
        Ok(flags & FS_APPEND_FL != 0)
    }

    fn set_append(&self, file: &File) -> Result<(), InstallError> {
        let mut flags = get_flags(file)?;
        flags |= FS_APPEND_FL;
        set_flags(file, flags)
    }
}

/// Create the layout, or report that it is already there.
pub fn install(
    pulse: &str,
    group: &str,
    dir: &Path,
    adopt: bool,
    host: &dyn Host,
) -> Result<Outcome, InstallError> {
    if !host.is_root() {
        return Err(InstallError::NotRoot);
    }
    if !is_plain_pulse_name(pulse) {
        return Err(InstallError::PulseName);
    }
    if !is_plain_pulse_name(group) {
        return Err(InstallError::GroupName);
    }
    let gid = host.group_gid(group)?;
    let (pulse_dir, dirfd) = prepare_tree(dir, pulse, host)?;
    // Refuse a symlink or a hard link at `anchors.log` before the receipt
    // file is created or written. A missing `anchors.log` is created later.
    anchors_usable(dirfd.as_raw_fd())?;
    let receipt = match open_name(
        dirfd.as_raw_fd(),
        "receipts.jsonl",
        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        None,
    ) {
        Ok(file) => file,
        Err(InstallError::Missing) => create_receipt(dirfd.as_raw_fd())?,
        Err(err) => return Err(err),
    };
    let meta = receipt.metadata().map_err(io_err)?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(InstallError::Local(
            "receipts.jsonl must be one regular file".into(),
        ));
    }
    let step = if meta.len() == 0 {
        ReceiptStep::Write
    } else {
        decide(&receipt, pulse, gid, adopt, host)?
    };
    let mut wrote = false;
    let mut adopted = false;
    match step {
        ReceiptStep::Write => {
            write_genesis(&receipt, pulse)?;
            wrote = true;
            finish_receipt(&receipt, gid, host)?;
        }
        ReceiptStep::Fix => {
            if finish_receipt(&receipt, gid, host)? {
                adopted = true;
            }
        }
        ReceiptStep::Ready => {}
    }
    let dir_changed = ensure_mode_owner(&reopen_dir(dirfd.as_raw_fd())?, DIR_MODE, 0, 0, host)?;
    let anchors_changed = ensure_anchors(dirfd.as_raw_fd(), host)?;
    drop(dirfd);
    if wrote {
        Ok(Outcome::Installed(pulse_dir))
    } else if adopted || dir_changed || anchors_changed {
        Ok(Outcome::Adopted(pulse_dir))
    } else {
        Ok(Outcome::Unchanged)
    }
}

enum ReceiptStep {
    Write,
    Fix,
    Ready,
}

fn create_receipt(dirfd: i32) -> Result<File, InstallError> {
    match open_name(
        dirfd,
        "receipts.jsonl",
        libc::O_RDWR
            | libc::O_CREAT
            | libc::O_EXCL
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC,
        Some(RECEIPT_MODE),
    ) {
        Ok(file) => Ok(file),
        Err(InstallError::Missing) => Err(InstallError::Local(
            "receipts.jsonl could not be created".into(),
        )),
        Err(InstallError::Local(text)) if text == "exists" => open_name(
            dirfd,
            "receipts.jsonl",
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            None,
        ),
        Err(err) => Err(err),
    }
}

fn decide(
    file: &File,
    pulse: &str,
    gid: u32,
    adopt: bool,
    host: &dyn Host,
) -> Result<ReceiptStep, InstallError> {
    let meta = file.metadata().map_err(io_err)?;
    if !meta.is_file() {
        return Err(InstallError::Local(
            "receipts.jsonl must be a regular file".into(),
        ));
    }
    if meta.nlink() != 1 {
        return Err(InstallError::Local(
            "receipts.jsonl has more than one link".into(),
        ));
    }
    if meta.len() > MAX_RECEIPT_BYTES {
        return Err(InstallError::Local(
            "receipts.jsonl is larger than 256 MiB".into(),
        ));
    }
    if meta.len() == 0 {
        return Ok(ReceiptStep::Write);
    }
    let bytes = read_fd(file, meta.len())?;
    if genesis_pulse(&bytes).is_some_and(|name| name != pulse) {
        return Err(InstallError::OtherPulse);
    }
    let report = chain::verify_lines(bytes.as_slice());
    if let Some(note) = report.first_break {
        if adopt {
            return Err(InstallError::Broken {
                line: note.line,
                reason: note.reason,
            });
        }
        return Err(InstallError::RefuseAdopt);
    }
    if genesis_pulse(&bytes).as_deref() != Some(pulse) {
        return Err(if adopt {
            InstallError::Broken {
                line: 1,
                reason: "genesis",
            }
        } else {
            InstallError::RefuseAdopt
        });
    }
    if receipt_ready(file, gid, host)? {
        Ok(ReceiptStep::Ready)
    } else if adopt {
        Ok(ReceiptStep::Fix)
    } else {
        Err(InstallError::RefuseAdopt)
    }
}

fn receipt_ready(file: &File, gid: u32, host: &dyn Host) -> Result<bool, InstallError> {
    let meta = file.metadata().map_err(io_err)?;
    let (uid, file_gid) = host.owner(file)?;
    Ok(meta.mode() & 0o777 == RECEIPT_MODE
        && uid == 0
        && file_gid == gid
        && host.append_flag(file)?)
}

fn finish_receipt(file: &File, gid: u32, host: &dyn Host) -> Result<bool, InstallError> {
    let changed = ensure_mode_owner(file, RECEIPT_MODE, 0, gid, host)?;
    let flagged = ensure_append(file, host)?;
    Ok(changed || flagged)
}

fn write_genesis(file: &File, pulse: &str) -> Result<(), InstallError> {
    let bytes = genesis_bytes(pulse)?;
    write_all_at(file, &bytes)?;
    file.sync_all().map_err(io_err)?;
    Ok(())
}

fn genesis_bytes(pulse: &str) -> Result<Vec<u8>, InstallError> {
    let row = chain::Row {
        v: 1,
        seq: 0,
        prev: GENESIS_PREV.to_string(),
        ts: now_ts(),
        e: serde_json::to_value(ReceiptEvent::Genesis {
            pulse: pulse.to_string(),
        })
        .map_err(|_| InstallError::Local("genesis row could not be built".into()))?,
    };
    let value = serde_json::to_value(&row)
        .map_err(|_| InstallError::Local("genesis row could not be built".into()))?;
    let mut text = chain::canonical(&value)
        .map_err(|_| InstallError::Local("genesis row could not be built".into()))?;
    text.push('\n');
    Ok(text.into_bytes())
}

fn now_ts() -> String {
    let now = chrono::Utc::now();
    let millis = now.timestamp_subsec_millis();
    format!("{}.{:03}Z", now.format("%Y-%m-%dT%H:%M:%S"), millis)
}

fn genesis_pulse(bytes: &[u8]) -> Option<String> {
    let line = bytes.split(|byte| *byte == b'\n').next().unwrap_or(bytes);
    if line.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let event = value.get("e")?;
    if event.get("kind")?.as_str() != Some("genesis") {
        return None;
    }
    event.get("pulse")?.as_str().map(str::to_string)
}

fn anchors_usable(dirfd: i32) -> Result<(), InstallError> {
    let file = match open_name(
        dirfd,
        "anchors.log",
        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        None,
    ) {
        Ok(file) => file,
        Err(InstallError::Missing) => return Ok(()),
        Err(err) => return Err(err),
    };
    let meta = file.metadata().map_err(io_err)?;
    if !meta.is_file() {
        return Err(InstallError::Local(
            "anchors.log must be a regular file".into(),
        ));
    }
    if meta.nlink() != 1 {
        return Err(InstallError::Local(
            "anchors.log has more than one link".into(),
        ));
    }
    Ok(())
}

fn ensure_anchors(dirfd: i32, host: &dyn Host) -> Result<bool, InstallError> {
    let file = match open_name(
        dirfd,
        "anchors.log",
        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        None,
    ) {
        Ok(file) => file,
        Err(InstallError::Missing) => open_name(
            dirfd,
            "anchors.log",
            libc::O_RDWR
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC,
            Some(ANCHOR_MODE),
        )?,
        Err(err) => return Err(err),
    };
    let meta = file.metadata().map_err(io_err)?;
    if !meta.is_file() {
        return Err(InstallError::Local(
            "anchors.log must be a regular file".into(),
        ));
    }
    if meta.nlink() != 1 {
        return Err(InstallError::Local(
            "anchors.log has more than one link".into(),
        ));
    }
    ensure_mode_owner(&file, ANCHOR_MODE, 0, 0, host)
}

fn ensure_mode_owner(
    file: &File,
    mode: u32,
    uid: u32,
    gid: u32,
    host: &dyn Host,
) -> Result<bool, InstallError> {
    let meta = file.metadata().map_err(io_err)?;
    let mut changed = false;
    if meta.mode() & 0o777 != mode {
        // SAFETY: `fchmod` sets the mode of this open descriptor.
        let rc = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
        if rc != 0 {
            return Err(io_err(io::Error::last_os_error()));
        }
        changed = true;
    }
    let (cur_uid, cur_gid) = host.owner(file)?;
    if cur_uid != uid || cur_gid != gid {
        host.chown(file, uid, gid)?;
        changed = true;
    }
    Ok(changed)
}

fn ensure_append(file: &File, host: &dyn Host) -> Result<bool, InstallError> {
    if host.append_flag(file)? {
        return Ok(false);
    }
    host.set_append(file)?;
    Ok(true)
}

fn prepare_tree(dir: &Path, pulse: &str, host: &dyn Host) -> Result<(PathBuf, File), InstallError> {
    let absolute = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir().map_err(io_err)?.join(dir)
    };
    let mut names = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(InstallError::Local("path must not contain ..".into()));
            }
        }
    }
    names.push(OsStr::new(pulse).to_os_string());
    let root = open_root()?;
    let mut current = root;
    let mut built = PathBuf::from("/");
    for name in &names {
        built.push(name);
        match open_child_dir(current.as_raw_fd(), name) {
            Ok(next) => {
                // An existing directory is adjusted only after the receipt
                // file is accepted. A directory this loop creates is adjusted
                // below, because it did not exist to hold a foreign file.
                current = next;
            }
            Err(InstallError::Missing) => {
                mkdir_child(current.as_raw_fd(), name)?;
                let next = open_child_dir(current.as_raw_fd(), name)?;
                let rw = open_child_rdonly(current.as_raw_fd(), name)?;
                ensure_mode_owner(&rw, DIR_MODE, 0, 0, host)?;
                current = next;
            }
            Err(err) => return Err(err),
        }
    }
    Ok((built, current))
}

fn reopen_dir(dirfd: i32) -> Result<File, InstallError> {
    // SAFETY: `dirfd` is an open directory. `O_RDONLY` is enough for `fchmod`.
    let fd = unsafe {
        libc::openat(
            dirfd,
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io_err(io::Error::last_os_error()));
    }
    // SAFETY: `openat` succeeded, so this process owns `fd`.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_root() -> Result<File, InstallError> {
    // SAFETY: the path is `/`. The flags do not follow a symlink.
    let fd = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io_err(io::Error::last_os_error()));
    }
    // SAFETY: `open` succeeded, so this process owns `fd`.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_child_dir(dirfd: i32, name: &OsStr) -> Result<File, InstallError> {
    open_at(
        dirfd,
        name,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        None,
    )
}

fn open_child_rdonly(dirfd: i32, name: &OsStr) -> Result<File, InstallError> {
    open_at(
        dirfd,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        None,
    )
}

fn mkdir_child(dirfd: i32, name: &OsStr) -> Result<(), InstallError> {
    let c_name = c_string(name)?;
    // SAFETY: `dirfd` is an open directory and `c_name` is nul-terminated.
    // The new directory is a child of this descriptor.
    let rc = unsafe { libc::mkdirat(dirfd, c_name.as_ptr(), DIR_MODE as libc::mode_t) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EEXIST) {
        return Ok(());
    }
    Err(io_err(err))
}

fn open_name(dirfd: i32, name: &str, flags: i32, mode: Option<u32>) -> Result<File, InstallError> {
    open_at(dirfd, OsStr::new(name), flags, mode)
}

fn open_at(dirfd: i32, name: &OsStr, flags: i32, mode: Option<u32>) -> Result<File, InstallError> {
    let c_name = c_string(name)?;
    let mode = mode.unwrap_or(0) as libc::mode_t;
    // SAFETY: `dirfd` is an open directory and `c_name` is nul-terminated.
    // `O_NOFOLLOW` refuses a symlink at this name.
    let fd = unsafe { libc::openat(dirfd, c_name.as_ptr(), flags, mode) };
    if fd >= 0 {
        // SAFETY: `openat` succeeded, so this process owns `fd`.
        return Ok(unsafe { File::from_raw_fd(fd) });
    }
    Err(open_err(io::Error::last_os_error()))
}

fn c_string(name: &OsStr) -> Result<CString, InstallError> {
    CString::new(name.as_bytes()).map_err(|_| InstallError::Local("path is not usable".into()))
}

fn open_err(err: io::Error) -> InstallError {
    match err.raw_os_error() {
        Some(libc::ENOENT) => InstallError::Missing,
        Some(libc::ELOOP) | Some(libc::ENOTDIR) => InstallError::Symlink,
        Some(libc::EEXIST) => InstallError::Local("exists".into()),
        _ => io_err(err),
    }
}

fn read_fd(mut file: &File, len: u64) -> Result<Vec<u8>, InstallError> {
    file.seek(SeekFrom::Start(0)).map_err(io_err)?;
    let mut bytes = Vec::new();
    file.take(len).read_to_end(&mut bytes).map_err(io_err)?;
    if bytes.len() as u64 != len {
        return Err(InstallError::Local(
            "receipts.jsonl could not be read".into(),
        ));
    }
    Ok(bytes)
}

fn write_all_at(mut file: &File, bytes: &[u8]) -> Result<(), InstallError> {
    file.seek(SeekFrom::Start(0)).map_err(io_err)?;
    let mut writer = file;
    writer.write_all(bytes).map_err(io_err)?;
    Ok(())
}

fn io_err(err: io::Error) -> InstallError {
    InstallError::Local(err.to_string())
}

fn lookup_group(name: &str) -> Result<u32, InstallError> {
    let c_name = CString::new(name).map_err(|_| InstallError::GroupName)?;
    let mut size = 16 * 1024;
    for _ in 0..4 {
        let mut buf = vec![0u8; size];
        let mut group = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut result: *mut libc::group = std::ptr::null_mut();
        // SAFETY: `c_name` is nul-terminated. `group` and `buf` live for this
        // call, and `result` is only read after `getgrnam_r` returns.
        let rc = unsafe {
            libc::getgrnam_r(
                c_name.as_ptr(),
                group.as_mut_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            size = size.saturating_mul(2);
            continue;
        }
        if rc != 0 {
            return Err(io_err(io::Error::from_raw_os_error(rc)));
        }
        if result.is_null() {
            return Err(InstallError::GroupNotFound);
        }
        // SAFETY: `result` points at `group`, which this function owns.
        return Ok(unsafe { (*result).gr_gid });
    }
    Err(InstallError::Local("group entry is too large".into()))
}

/// `_IOR('f', 1, long)` and `_IOW('f', 2, long)` on 64-bit Linux.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const FS_IOC_GETFLAGS: libc::Ioctl = 0x8008_6601;
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const FS_IOC_SETFLAGS: libc::Ioctl = 0x4008_6602;
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const FS_APPEND_FL: libc::c_long = 0x20;

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn get_flags(file: &File) -> Result<libc::c_long, InstallError> {
    let mut flags: libc::c_long = 0;
    // SAFETY: the request writes one `long` into `flags`. The fd is open.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) };
    if rc == 0 {
        Ok(flags)
    } else {
        Err(io_err(io::Error::last_os_error()))
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn set_flags(file: &File, mut flags: libc::c_long) -> Result<(), InstallError> {
    // SAFETY: the request reads one `long` from `flags`. The fd is open.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_SETFLAGS, &mut flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(InstallError::Local(
            "could not set the append-only flag".into(),
        ))
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn get_flags(_file: &File) -> Result<libc::c_long, InstallError> {
    Err(InstallError::Local(
        "the append-only flag is only set on 64-bit Linux".into(),
    ))
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn set_flags(_file: &File, _flags: libc::c_long) -> Result<(), InstallError> {
    Err(InstallError::Local(
        "the append-only flag is only set on 64-bit Linux".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    struct Fake {
        root: bool,
        gid: Option<u32>,
        chowns: Mutex<Vec<(PathBuf, u32, u32)>>,
        appends: Mutex<Vec<PathBuf>>,
        owners: Mutex<HashMap<PathBuf, (u32, u32)>>,
        flags: Mutex<HashMap<PathBuf, bool>>,
    }

    impl Fake {
        fn root_group(gid: u32) -> Self {
            Self {
                root: true,
                gid: Some(gid),
                chowns: Mutex::new(Vec::new()),
                appends: Mutex::new(Vec::new()),
                owners: Mutex::new(HashMap::new()),
                flags: Mutex::new(HashMap::new()),
            }
        }
    }

    impl Host for Fake {
        fn is_root(&self) -> bool {
            self.root
        }

        fn group_gid(&self, _name: &str) -> Result<u32, InstallError> {
            self.gid.ok_or(InstallError::GroupNotFound)
        }

        fn owner(&self, file: &File) -> Result<(u32, u32), InstallError> {
            let path = fd_path(file);
            if let Some(pair) = self.owners.lock().unwrap().get(&path) {
                return Ok(*pair);
            }
            let meta = file.metadata().map_err(io_err)?;
            Ok((meta.uid(), meta.gid()))
        }

        fn chown(&self, file: &File, uid: u32, gid: u32) -> Result<(), InstallError> {
            let path = fd_path(file);
            self.chowns.lock().unwrap().push((path.clone(), uid, gid));
            self.owners.lock().unwrap().insert(path, (uid, gid));
            Ok(())
        }

        fn append_flag(&self, file: &File) -> Result<bool, InstallError> {
            let path = fd_path(file);
            Ok(self
                .flags
                .lock()
                .unwrap()
                .get(&path)
                .copied()
                .unwrap_or(false))
        }

        fn set_append(&self, file: &File) -> Result<(), InstallError> {
            let path = fd_path(file);
            self.appends.lock().unwrap().push(path.clone());
            self.flags.lock().unwrap().insert(path, true);
            Ok(())
        }
    }

    fn fd_path(file: &File) -> PathBuf {
        std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap()
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().mode() & 0o777
    }

    fn run(
        dir: &Path,
        pulse: &str,
        group: &str,
        adopt: bool,
        host: &Fake,
    ) -> Result<Outcome, InstallError> {
        install(pulse, group, dir, adopt, host)
    }

    #[test]
    fn receipt_install_refuses_when_not_root() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake {
            root: false,
            ..Fake::root_group(10)
        };
        let err = run(dir.path(), "echo", "pulse", false, &host).unwrap_err();
        assert!(matches!(err, InstallError::NotRoot));
        assert!(!dir.path().join("echo").exists());
        assert!(host.chowns.lock().unwrap().is_empty());
        assert!(host.appends.lock().unwrap().is_empty());
    }

    #[test]
    fn receipt_install_refuses_a_bad_pulse_or_group_name() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake::root_group(10);
        for (pulse, group) in [
            ("a/b", "pulse"),
            (".hidden", "pulse"),
            ("echo", "has space"),
            ("echo", ".."),
        ] {
            let err = run(dir.path(), pulse, group, false, &host).unwrap_err();
            assert!(
                matches!(err, InstallError::PulseName | InstallError::GroupName),
                "{pulse} {group}: {err}"
            );
        }
        assert!(!dir.path().join("echo").exists());
    }

    #[test]
    fn receipt_install_refuses_an_unknown_group() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake {
            gid: None,
            ..Fake::root_group(10)
        };
        let err = run(dir.path(), "echo", "pulse", false, &host).unwrap_err();
        assert!(matches!(err, InstallError::GroupNotFound));
        assert!(!dir.path().join("echo").exists());
    }

    #[test]
    fn receipt_install_records_modes_and_the_append_flag() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake::root_group(42);
        let outcome = run(dir.path(), "echo", "pulse", false, &host).unwrap();
        let pulse = dir.path().join("echo");
        assert_eq!(outcome, Outcome::Installed(pulse.clone()));
        assert_eq!(mode_of(&pulse), DIR_MODE);
        let receipt = pulse.join("receipts.jsonl");
        let anchors = pulse.join("anchors.log");
        assert_eq!(mode_of(&receipt), RECEIPT_MODE);
        assert_eq!(mode_of(&anchors), ANCHOR_MODE);
        assert_eq!(std::fs::metadata(&anchors).unwrap().len(), 0);
        let text = std::fs::read_to_string(&receipt).unwrap();
        let report = chain::verify_lines(text.as_bytes());
        assert!(report.first_break.is_none(), "{report:?}");
        assert_eq!(report.rows, 1);
        assert_eq!(genesis_pulse(text.as_bytes()).as_deref(), Some("echo"));
        let chowns = host.chowns.lock().unwrap();
        assert!(chowns
            .iter()
            .any(|(path, uid, gid)| path.ends_with("echo") && *uid == 0 && *gid == 0));
        assert!(chowns
            .iter()
            .any(|(path, uid, gid)| path.ends_with("receipts.jsonl") && *uid == 0 && *gid == 42));
        assert!(chowns
            .iter()
            .any(|(path, uid, gid)| path.ends_with("anchors.log") && *uid == 0 && *gid == 0));
        assert_eq!(host.appends.lock().unwrap().len(), 1);
        assert!(host.appends.lock().unwrap()[0].ends_with("receipts.jsonl"));
    }

    #[test]
    fn receipt_install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake::root_group(42);
        run(dir.path(), "echo", "pulse", false, &host).unwrap();
        let receipt = dir.path().join("echo/receipts.jsonl");
        let before = std::fs::read(&receipt).unwrap();
        let chowns = host.chowns.lock().unwrap().len();
        let appends = host.appends.lock().unwrap().len();
        let again = run(dir.path(), "echo", "pulse", false, &host).unwrap();
        assert_eq!(again, Outcome::Unchanged);
        assert_eq!(std::fs::read(&receipt).unwrap(), before);
        assert_eq!(host.chowns.lock().unwrap().len(), chowns);
        assert_eq!(host.appends.lock().unwrap().len(), appends);
    }

    #[test]
    fn receipt_install_refuses_a_non_empty_foreign_file() {
        let dir = tempfile::tempdir().unwrap();
        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        std::fs::write(&receipt, b"not a receipt\n").unwrap();
        let mode = mode_of(&pulse);
        let host = Fake::root_group(42);
        let err = run(dir.path(), "echo", "pulse", false, &host).unwrap_err();
        assert!(matches!(err, InstallError::RefuseAdopt), "{err}");
        assert_eq!(std::fs::read(&receipt).unwrap(), b"not a receipt\n");
        assert_eq!(mode_of(&pulse), mode);
        assert!(!pulse.join("anchors.log").exists());
        assert!(host.appends.lock().unwrap().is_empty());
    }

    #[test]
    fn receipt_install_refuses_a_genesis_that_names_another_pulse() {
        let dir = tempfile::tempdir().unwrap();
        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        let bytes = genesis_bytes("other").unwrap();
        std::fs::write(&receipt, &bytes).unwrap();
        let host = Fake::root_group(42);
        let err = run(dir.path(), "echo", "pulse", true, &host).unwrap_err();
        assert!(matches!(err, InstallError::OtherPulse), "{err}");
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
        assert!(host.appends.lock().unwrap().is_empty());
        let without = run(dir.path(), "echo", "pulse", false, &host).unwrap_err();
        assert!(matches!(without, InstallError::OtherPulse));
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
    }

    #[test]
    fn receipt_install_adopt_refuses_a_broken_chain() {
        let dir = tempfile::tempdir().unwrap();
        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        let mut bytes = genesis_bytes("echo").unwrap();
        bytes.extend_from_slice(b"not-a-row\n");
        std::fs::write(&receipt, &bytes).unwrap();
        let host = Fake::root_group(42);
        let err = run(dir.path(), "echo", "pulse", true, &host).unwrap_err();
        match err {
            InstallError::Broken { line, .. } => assert_eq!(line, 2),
            other => panic!("{other}"),
        }
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
        assert!(host.appends.lock().unwrap().is_empty());
    }

    #[test]
    fn receipt_install_adopt_keeps_a_valid_chain_and_sets_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        let bytes = genesis_bytes("echo").unwrap();
        std::fs::write(&receipt, &bytes).unwrap();
        std::fs::set_permissions(&receipt, std::fs::Permissions::from_mode(0o666)).unwrap();
        let host = Fake::root_group(42);
        let refused = run(dir.path(), "echo", "pulse", false, &host).unwrap_err();
        assert!(matches!(refused, InstallError::RefuseAdopt), "{refused}");
        assert_eq!(mode_of(&receipt), 0o666);
        let outcome = run(dir.path(), "echo", "pulse", true, &host).unwrap();
        assert_eq!(outcome, Outcome::Adopted(pulse.clone()));
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
        assert_eq!(mode_of(&receipt), RECEIPT_MODE);
        assert_eq!(host.appends.lock().unwrap().len(), 1);
        assert!(pulse.join("anchors.log").exists());
    }

    #[test]
    fn receipt_install_refuses_a_symlink_in_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        let host = Fake::root_group(42);
        let err = run(&dir.path().join("link"), "echo", "pulse", false, &host).unwrap_err();
        assert!(matches!(err, InstallError::Symlink), "{err}");
        assert!(!real.join("echo").exists());

        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, pulse.join("receipts.jsonl")).unwrap();
        let err = run(dir.path(), "echo", "pulse", true, &host).unwrap_err();
        assert!(matches!(err, InstallError::Symlink), "{err}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        assert!(!pulse.join("anchors.log").exists());

        let again = dir.path().join("again");
        std::fs::create_dir(&again).unwrap();
        let target = dir.path().join("anchor-target");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, again.join("anchors.log")).unwrap();
        let err = run(dir.path(), "again", "pulse", false, &host).unwrap_err();
        assert!(matches!(err, InstallError::Symlink), "{err}");
        assert!(!again.join("receipts.jsonl").exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn receipt_install_templates_have_the_required_keys() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("templates/systemd");
        let conf = std::fs::read_to_string(root.join("pulse-null-receipts.conf")).unwrap();
        let service = std::fs::read_to_string(root.join("pulse-null-anchor@.service")).unwrap();
        let timer = std::fs::read_to_string(root.join("pulse-null-anchor@.timer")).unwrap();
        for text in [&conf, &service, &timer] {
            let lower = text.to_ascii_lowercase();
            assert!(!lower.contains("ghp_"), "{text}");
            assert!(!lower.contains("github_pat_"), "{text}");
            assert!(!lower.contains("bearer "), "{text}");
        }
        assert!(conf.contains("OpenFile=/var/lib/pulse-null/%i/receipts.jsonl:receipts:append"));
        assert!(conf.contains("Environment=PULSE_NULL_RECEIPTS=required"));
        assert!(conf.contains("253"));
        assert!(service.contains("ExecStart=/usr/local/bin/pulse-null receipts anchor --pulse %i"));
        assert!(service.contains("LoadCredential=anchor-token:/etc/pulse-null/anchor/token"));
        assert!(service.contains("User=root"));
        assert!(service.contains("/etc/pulse-null/anchor.toml"));
        assert!(timer.contains("OnCalendar=hourly"));
        assert!(timer.contains("pulse-null-anchor@%i.service"));
        if which_systemd_analyze() {
            for unit in ["pulse-null-anchor@.service", "pulse-null-anchor@.timer"] {
                let output = std::process::Command::new("systemd-analyze")
                    .arg("verify")
                    .arg(root.join(unit))
                    .output()
                    .unwrap();
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    output.status.success() || stderr.contains("is not executable"),
                    "{unit}\n{stderr}"
                );
                assert!(
                    !stderr.to_ascii_lowercase().contains("failed to parse"),
                    "{unit}\n{stderr}"
                );
            }
        }
    }

    fn which_systemd_analyze() -> bool {
        std::process::Command::new("systemd-analyze")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}
