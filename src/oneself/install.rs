//! `pulse-null receipts install`.
//!
//! Creates `<dir>/<pulse>/` as root:root mode 0755, `receipts.jsonl` as
//! root:`<group>` mode 0640 with one `genesis` row, the append-only flag on
//! that file, and an empty `anchors.log` as root:root mode 0644. Every
//! component is opened with `openat` and `O_NOFOLLOW`. A second run that
//! finds this layout changes nothing and prints `already installed; nothing
//! changed`. An existing file is opened read-only. The same inode is kept
//! only when the file is already owned by root, has no group or world write
//! bit, and has no ACL. Anything else that `--adopt` accepts is copied onto
//! a new inode. Stop the pulse before `--adopt`.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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
    /// The verified chain was copied onto a new inode.
    Replaced(PathBuf),
    /// The same inode was kept and its mode, owner, or append-only flag changed.
    ///
    /// `half` means the owner and mode already matched and only the flag was
    /// set. A half-finished install cannot be told from an existing receipt
    /// file, so [`summary`] does not say installed. `layout` means the
    /// directory or `anchors.log` was also created or repaired.
    Repaired {
        path: PathBuf,
        half: bool,
        layout: bool,
    },
    /// The receipt file was already in place. The directory or `anchors.log`
    /// was created or repaired.
    Layout(PathBuf),
    /// The layout was already in place.
    Unchanged,
}

const HALF_NOTE: &str = "a half-finished install cannot be told from an existing receipt file, so this is not reported as installed";

/// The line `receipts install` prints for this outcome.
pub fn summary(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Installed(path) => format!("installed {}", path.display()),
        Outcome::Replaced(path) => format!("adopted {}; inode replaced", path.display()),
        Outcome::Repaired { path, half, layout } => {
            let detail = if *half {
                "append-only flag set"
            } else {
                "mode or owner repaired"
            };
            let mut line = format!("adopted {}; {detail}", path.display());
            if *layout {
                line.push_str("; directory or anchors.log created or repaired");
            }
            if *half {
                line.push_str("; ");
                line.push_str(HALF_NOTE);
            }
            line
        }
        Outcome::Layout(path) => format!(
            "adopted {}; directory or anchors.log created or repaired",
            path.display()
        ),
        Outcome::Unchanged => "already installed; nothing changed".to_string(),
    }
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
    /// `receipts.jsonl` or `anchors.log` exists and is not owned by root.
    NotRootOwned {
        file: &'static str,
    },
    /// The pulse directory is not owned by root, or it is group- or world-writable.
    DirUnsafe,
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
            Self::NotRootOwned { file } => {
                if *file == "anchors.log" {
                    write!(
                        formatter,
                        "{file} is not owned by root; stop the pulse and move or chown the file"
                    )
                } else {
                    write!(
                        formatter,
                        "{file} is not owned by root; stop the pulse and move the file aside, or pass --adopt"
                    )
                }
            }
            Self::DirUnsafe => formatter.write_str(
                "pulse directory must be owned by root and not group- or world-writable",
            ),
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
    /// Clear `FS_APPEND_FL` on this descriptor.
    fn clear_append(&self, file: &File) -> Result<(), InstallError>;
    /// Fail before any receipt byte is written when this filesystem cannot
    /// store `FS_APPEND_FL`.
    fn append_supported(&self, dir: &File) -> Result<(), InstallError>;
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
        let flags = get_flags(file).map_err(flag_io)?;
        Ok(flags & FS_APPEND_FL != 0)
    }

    fn set_append(&self, file: &File) -> Result<(), InstallError> {
        let mut flags = get_flags(file).map_err(flag_io)?;
        flags |= FS_APPEND_FL;
        set_flags(file, flags)
    }

    fn clear_append(&self, file: &File) -> Result<(), InstallError> {
        let flags = get_flags(file).map_err(flag_io)?;
        set_flags(file, flags & !FS_APPEND_FL)
    }

    fn append_supported(&self, dir: &File) -> Result<(), InstallError> {
        match get_flags(dir) {
            Ok(_) => Ok(()),
            Err(err) if unsupported_flags(&err) => Err(InstallError::Local(format!(
                "filesystem does not support the append-only flag (FS_APPEND_FL): {err}"
            ))),
            Err(err) => Err(flag_io(err)),
        }
    }
}

const OPEN_READ: i32 =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY;
const OPEN_NEW: i32 = libc::O_RDWR
    | libc::O_CREAT
    | libc::O_EXCL
    | libc::O_NOFOLLOW
    | libc::O_NONBLOCK
    | libc::O_CLOEXEC
    | libc::O_NOCTTY;
const OPEN_WRITE: i32 =
    libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY;

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
    let (pulse_dir, dirfd, created_dirs) = prepare_tree(dir, pulse, host)?;
    let mut created_files = Vec::new();
    let result = install_in(
        &pulse_dir,
        &dirfd,
        pulse,
        gid,
        adopt,
        host,
        &mut created_files,
    );
    match result {
        Ok(outcome) => Ok(outcome),
        Err(err) => Err(with_cleanup(err, &created_files, &created_dirs, host)),
    }
}

fn install_in(
    pulse_dir: &Path,
    dirfd: &File,
    pulse: &str,
    gid: u32,
    adopt: bool,
    host: &dyn Host,
    created_files: &mut Vec<PathBuf>,
) -> Result<Outcome, InstallError> {
    let dir_rw = reopen_dir(dirfd.as_raw_fd())?;
    ensure_dir_safe(&dir_rw, host)?;
    // A filesystem that cannot store the flag fails here, before a genesis
    // row or an owner change on a receipt file.
    host.append_supported(&dir_rw)?;
    inspect_anchors(dirfd.as_raw_fd(), host)?;
    let pending = place_receipt(
        dirfd.as_raw_fd(),
        pulse_dir,
        pulse,
        gid,
        adopt,
        host,
        created_files,
    )?;
    // `anchors.log` exists before the receipt file gets `FS_APPEND_FL`, so a
    // failure while creating it can still unlink a receipt file this run created.
    let dir_changed = ensure_mode_owner(&dir_rw, DIR_MODE, 0, 0, host)?;
    let anchors_changed = ensure_anchors(dirfd.as_raw_fd(), pulse_dir, host, created_files)?;
    let done = seal_receipt(dirfd.as_raw_fd(), pending, gid, host)?;
    let path = pulse_dir.to_path_buf();
    let layout = dir_changed || anchors_changed;
    Ok(match done {
        ReceiptDone::Wrote => Outcome::Installed(path),
        ReceiptDone::Replaced => Outcome::Replaced(path),
        ReceiptDone::Repaired { half } => Outcome::Repaired { path, half, layout },
        ReceiptDone::Ready => {
            if layout {
                Outcome::Layout(path)
            } else {
                Outcome::Unchanged
            }
        }
    })
}

enum ReceiptDone {
    Wrote,
    Replaced,
    Repaired { half: bool },
    Ready,
}

enum Pending {
    Wrote,
    Replaced,
    NeedsRepair,
    NeedsFlag,
    Ready,
}

fn ensure_dir_safe(dir: &File, host: &dyn Host) -> Result<(), InstallError> {
    let (uid, _) = host.owner(dir)?;
    let mode = dir.metadata().map_err(io_err)?.mode();
    if uid != 0 || mode & 0o022 != 0 {
        Err(InstallError::DirUnsafe)
    } else {
        Ok(())
    }
}

fn place_receipt(
    dirfd: i32,
    pulse_dir: &Path,
    pulse: &str,
    gid: u32,
    adopt: bool,
    host: &dyn Host,
    created_files: &mut Vec<PathBuf>,
) -> Result<Pending, InstallError> {
    match open_read(dirfd, "receipts.jsonl") {
        Err(InstallError::Missing) => {
            let file = create_exclusive(dirfd, "receipts.jsonl", 0o600)?;
            created_files.push(pulse_dir.join("receipts.jsonl"));
            write_genesis(&file, pulse)?;
            ensure_mode_owner(&file, RECEIPT_MODE, 0, gid, host)?;
            Ok(Pending::Wrote)
        }
        Ok(file) => existing_receipt(dirfd, &file, pulse, gid, adopt, host),
        Err(err) => Err(err),
    }
}

fn existing_receipt(
    dirfd: i32,
    file: &File,
    pulse: &str,
    gid: u32,
    adopt: bool,
    host: &dyn Host,
) -> Result<Pending, InstallError> {
    let meta = require_regular(file, "receipts.jsonl")?;
    if !keeps_inode(file, host)? {
        return replace_or_refuse(dirfd, file, &meta, pulse, gid, adopt, host);
    }
    if meta.len() == 0 {
        let writing = open_write(dirfd, "receipts.jsonl")?;
        require_regular(&writing, "receipts.jsonl")?;
        write_genesis(&writing, pulse)?;
        ensure_mode_owner(&writing, RECEIPT_MODE, 0, gid, host)?;
        return Ok(Pending::Wrote);
    }
    match decide(file, pulse, gid, adopt, host)? {
        ReceiptStep::Write => Err(InstallError::Local(
            "receipt file changed while it was checked".into(),
        )),
        ReceiptStep::Fix => Ok(Pending::NeedsRepair),
        ReceiptStep::FlagOnly => Ok(Pending::NeedsFlag),
        ReceiptStep::Ready => Ok(Pending::Ready),
    }
}

/// The same inode is safe only when a group member cannot have opened it for
/// writing. The append-only flag is not part of this: a handle opened before
/// the flag was set can still write.
fn keeps_inode(file: &File, host: &dyn Host) -> Result<bool, InstallError> {
    let (uid, _) = host.owner(file)?;
    let mode = file.metadata().map_err(io_err)?.mode();
    if uid != 0 || mode & 0o022 != 0 {
        return Ok(false);
    }
    Ok(!has_acl(file)?)
}

fn has_acl(file: &File) -> Result<bool, InstallError> {
    for name in [
        c"system.posix_acl_access".as_ptr(),
        c"system.posix_acl_default".as_ptr(),
    ] {
        // SAFETY: `name` is a nul-terminated literal. A null buffer and a
        // size of 0 return the attribute's size. A non-negative result means
        // the attribute is present, including an empty one. Any ACL replaces
        // the inode. `ENODATA` and `ENOTSUP` mean there is no ACL.
        let rc = unsafe { libc::fgetxattr(file.as_raw_fd(), name, std::ptr::null_mut(), 0) };
        if rc >= 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        let code = err.raw_os_error();
        let no_acl = code == Some(libc::ENODATA)
            || code == Some(libc::ENOTSUP)
            || code == Some(libc::EOPNOTSUPP);
        if !no_acl {
            return Err(io_err(err));
        }
    }
    Ok(false)
}

fn replace_or_refuse(
    dirfd: i32,
    file: &File,
    meta: &std::fs::Metadata,
    pulse: &str,
    gid: u32,
    adopt: bool,
    host: &dyn Host,
) -> Result<Pending, InstallError> {
    let (uid, _) = host.owner(file)?;
    if uid != 0 && !adopt {
        return Err(InstallError::NotRootOwned {
            file: "receipts.jsonl",
        });
    }
    if meta.len() > MAX_RECEIPT_BYTES {
        return Err(InstallError::Local(
            "receipts.jsonl is larger than 256 MiB".into(),
        ));
    }
    if !adopt {
        // Do not write a genesis row into an inode a group member can still
        // hold open. Another pulse's genesis is still named.
        if meta.len() > 0 {
            let bytes = read_fd(file, meta.len())?;
            if genesis_pulse(&bytes).is_some_and(|name| name != pulse) {
                return Err(InstallError::OtherPulse);
            }
        }
        return Err(InstallError::RefuseAdopt);
    }
    replace_receipt(dirfd, file, meta, pulse, gid, host)?;
    Ok(Pending::Replaced)
}

fn seal_receipt(
    dirfd: i32,
    pending: Pending,
    gid: u32,
    host: &dyn Host,
) -> Result<ReceiptDone, InstallError> {
    let replaced = matches!(pending, Pending::Replaced);
    let flag_only = matches!(pending, Pending::NeedsFlag);
    match pending {
        Pending::NeedsRepair => {
            let file = open_read(dirfd, "receipts.jsonl")?;
            if finish_receipt(&file, gid, host)? {
                Ok(ReceiptDone::Repaired { half: false })
            } else {
                Ok(ReceiptDone::Ready)
            }
        }
        Pending::Ready => Ok(ReceiptDone::Ready),
        Pending::Wrote | Pending::Replaced | Pending::NeedsFlag => {
            let file = open_read(dirfd, "receipts.jsonl")?;
            if !host.append_flag(&file)? {
                host.set_append(&file)?;
                file.sync_all().map_err(io_err)?;
            }
            if replaced {
                Ok(ReceiptDone::Replaced)
            } else if flag_only {
                Ok(ReceiptDone::Repaired { half: true })
            } else {
                Ok(ReceiptDone::Wrote)
            }
        }
    }
}

fn replace_receipt(
    dirfd: i32,
    file: &File,
    meta: &std::fs::Metadata,
    pulse: &str,
    gid: u32,
    host: &dyn Host,
) -> Result<(), InstallError> {
    if meta.len() > MAX_RECEIPT_BYTES {
        return Err(InstallError::Local(
            "receipts.jsonl is larger than 256 MiB".into(),
        ));
    }
    if meta.len() == 0 {
        return Err(InstallError::Local(
            "receipt file is empty and does not verify for this pulse".into(),
        ));
    }
    let bytes = read_fd(file, meta.len())?;
    if genesis_pulse(&bytes).is_some_and(|name| name != pulse) {
        return Err(InstallError::OtherPulse);
    }
    let report = chain::verify_lines(bytes.as_slice());
    if let Some(note) = report.first_break {
        return Err(InstallError::Broken {
            line: note.line,
            reason: note.reason,
        });
    }
    if genesis_pulse(&bytes).as_deref() != Some(pulse) {
        return Err(InstallError::Broken {
            line: 1,
            reason: "genesis",
        });
    }
    let tmp_name = format!(".receipts.jsonl.{}.tmp", std::process::id());
    let tmp_c = CString::new(tmp_name.as_str())
        .map_err(|_| InstallError::Local("path is not usable".into()))?;
    let mut temp = TempFile {
        dirfd,
        name: Some(tmp_c),
    };
    let new_file = create_exclusive(dirfd, &tmp_name, 0o600)?;
    write_all_at(&new_file, &bytes)?;
    new_file.sync_all().map_err(io_err)?;
    host.chown(&new_file, 0, gid)?;
    fchmod(&new_file, RECEIPT_MODE)?;
    sync_dir(dirfd)?;
    let dest = CString::new("receipts.jsonl")
        .map_err(|_| InstallError::Local("path is not usable".into()))?;
    // `renameat` replaces the directory entry. The kernel refuses that when
    // the destination inode has `FS_APPEND_FL`: `may_delete` returns `EPERM`
    // for `IS_APPEND`. Clear the flag on the old inode and try once more.
    // A descriptor opened before the rename still names the old inode.
    let from = temp.name.as_ref().unwrap();
    if let Err(err) = rename_over(dirfd, from, &dest) {
        let append_only = err.raw_os_error() == Some(libc::EPERM) && host.append_flag(file)?;
        if !append_only {
            return Err(io_err(err));
        }
        host.clear_append(file)?;
        if let Err(err) = rename_over(dirfd, from, &dest) {
            return match host.set_append(file) {
                Ok(()) => Err(io_err(err)),
                Err(restore) => Err(InstallError::Local(format!(
                    "the append-only flag was cleared and could not be restored: {restore}; while handling: {err}"
                ))),
            };
        }
    }
    temp.name = None;
    drop(new_file);
    sync_dir(dirfd)?;
    Ok(())
}

fn rename_over(dirfd: i32, from: &CString, to: &CString) -> Result<(), io::Error> {
    // SAFETY: both names are nul-terminated children of this directory fd.
    let rc = unsafe { libc::renameat(dirfd, from.as_ptr(), dirfd, to.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

struct TempFile {
    dirfd: i32,
    name: Option<CString>,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            // SAFETY: `name` is the temp file this install created.
            unsafe { libc::unlinkat(self.dirfd, name.as_ptr(), 0) };
        }
    }
}

enum ReceiptStep {
    Write,
    Fix,
    FlagOnly,
    Ready,
}

fn open_read(dirfd: i32, name: &str) -> Result<File, InstallError> {
    open_name(dirfd, name, OPEN_READ, None)
}

fn open_write(dirfd: i32, name: &str) -> Result<File, InstallError> {
    open_name(dirfd, name, OPEN_WRITE, None)
}

fn create_exclusive(dirfd: i32, name: &str, mode: u32) -> Result<File, InstallError> {
    match open_name(dirfd, name, OPEN_NEW, Some(mode)) {
        Ok(file) => Ok(file),
        Err(InstallError::Missing) => {
            Err(InstallError::Local(format!("{name} could not be created")))
        }
        Err(err) => Err(err),
    }
}

fn require_regular(file: &File, name: &str) -> Result<std::fs::Metadata, InstallError> {
    let meta = file.metadata().map_err(io_err)?;
    if !meta.is_file() {
        return Err(InstallError::Local(format!(
            "{name} must be a regular file"
        )));
    }
    if meta.nlink() != 1 {
        return Err(InstallError::Local(format!(
            "{name} has more than one link"
        )));
    }
    Ok(meta)
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
    let meta = file.metadata().map_err(io_err)?;
    let (uid, file_gid) = host.owner(file)?;
    let shape = meta.mode() & 0o777 == RECEIPT_MODE && uid == 0 && file_gid == gid;
    let flagged = host.append_flag(file)?;
    if shape && flagged {
        Ok(ReceiptStep::Ready)
    } else if shape {
        // The owner and mode already match. Only the append-only flag is
        // missing. That is what a half-finished install looks like, and it
        // is also what an existing file looks like. The summary says that
        // these cannot be told apart and does not say installed.
        Ok(ReceiptStep::FlagOnly)
    } else if adopt {
        Ok(ReceiptStep::Fix)
    } else {
        Err(InstallError::RefuseAdopt)
    }
}

fn finish_receipt(file: &File, gid: u32, host: &dyn Host) -> Result<bool, InstallError> {
    let meta = file.metadata().map_err(io_err)?;
    let (uid, file_gid) = host.owner(file)?;
    let mode_differs = meta.mode() & 0o777 != RECEIPT_MODE;
    let owner_differs = uid != 0 || file_gid != gid;
    let flagged = host.append_flag(file)?;
    if !mode_differs && !owner_differs {
        if flagged {
            return Ok(false);
        }
        host.set_append(file)?;
        return Ok(true);
    }
    // `fchown` and `fchmod` fail while `FS_APPEND_FL` is set. Clear it, then
    // put it back. A failed restore is reported on its own.
    if flagged {
        host.clear_append(file)?;
    }
    if let Err(err) = ensure_mode_owner(file, RECEIPT_MODE, 0, gid, host) {
        return restore_append(file, host, flagged, err);
    }
    if let Err(err) = host.set_append(file) {
        if flagged {
            return Err(InstallError::Local(format!(
                "the append-only flag was cleared and could not be set again: {err}"
            )));
        }
        return Err(err);
    }
    Ok(true)
}

fn restore_append(
    file: &File,
    host: &dyn Host,
    flagged: bool,
    err: InstallError,
) -> Result<bool, InstallError> {
    if !flagged {
        return Err(err);
    }
    match host.set_append(file) {
        Ok(()) => Err(err),
        Err(restore) => Err(InstallError::Local(format!(
            "the append-only flag was cleared and could not be restored: {restore}; while handling: {err}"
        ))),
    }
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

fn inspect_anchors(dirfd: i32, host: &dyn Host) -> Result<(), InstallError> {
    match open_read(dirfd, "anchors.log") {
        Err(InstallError::Missing) => Ok(()),
        Ok(file) => {
            require_regular(&file, "anchors.log")?;
            let (uid, _) = host.owner(&file)?;
            if uid != 0 {
                Err(InstallError::NotRootOwned {
                    file: "anchors.log",
                })
            } else {
                Ok(())
            }
        }
        Err(err) => Err(err),
    }
}

fn ensure_anchors(
    dirfd: i32,
    pulse_dir: &Path,
    host: &dyn Host,
    created_files: &mut Vec<PathBuf>,
) -> Result<bool, InstallError> {
    let file = match open_read(dirfd, "anchors.log") {
        Ok(file) => file,
        Err(InstallError::Missing) => {
            let file = create_exclusive(dirfd, "anchors.log", 0o600)?;
            created_files.push(pulse_dir.join("anchors.log"));
            file
        }
        Err(err) => return Err(err),
    };
    require_regular(&file, "anchors.log")?;
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

fn fchmod(file: &File, mode: u32) -> Result<(), InstallError> {
    // SAFETY: `fchmod` sets the mode of this open descriptor.
    let rc = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io_err(io::Error::last_os_error()))
    }
}

fn sync_dir(dirfd: i32) -> Result<(), InstallError> {
    reopen_dir(dirfd)?.sync_all().map_err(io_err)
}

fn with_cleanup(
    err: InstallError,
    files: &[PathBuf],
    dirs: &[PathBuf],
    host: &dyn Host,
) -> InstallError {
    match remove_created(files, dirs, host) {
        Ok(()) => err,
        Err(cleanup) => InstallError::Local(format!("{err}; cleanup failed: {cleanup}")),
    }
}

fn remove_created(
    files: &[PathBuf],
    dirs: &[PathBuf],
    host: &dyn Host,
) -> Result<(), InstallError> {
    let mut errors = Vec::new();
    for path in files.iter().rev() {
        if let Err(err) = remove_created_file(path, host) {
            errors.push(format!("{}: {err}", path.display()));
        }
    }
    for path in dirs.iter().rev() {
        match std::fs::remove_dir(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => errors.push(format!("{}: {err}", path.display())),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(InstallError::Local(errors.join("; ")))
    }
}

fn remove_created_file(path: &Path, host: &dyn Host) -> Result<(), InstallError> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(io_err(err)),
    };
    // An append-only file cannot be unlinked, including by root. Clear the
    // flag on this descriptor first.
    host.clear_append(&file)?;
    let parent = path
        .parent()
        .ok_or_else(|| InstallError::Local("path is not usable".into()))?;
    let name = path
        .file_name()
        .ok_or_else(|| InstallError::Local("path is not usable".into()))?;
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)
        .map_err(io_err)?;
    let c_name = c_string(name)?;
    // SAFETY: `c_name` is a nul-terminated child of this directory fd.
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), c_name.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(io_err(err))
        }
    }
}

fn prepare_tree(
    dir: &Path,
    pulse: &str,
    host: &dyn Host,
) -> Result<(PathBuf, File, Vec<PathBuf>), InstallError> {
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
    let mut created = Vec::new();
    for name in &names {
        built.push(name);
        match open_child_dir(current.as_raw_fd(), name) {
            Ok(next) => {
                // An existing directory is not chmod'd here. The pulse
                // directory must already be root-owned before any file is
                // trusted. A directory this loop creates is adjusted below.
                current = next;
            }
            Err(InstallError::Missing) => {
                if let Err(err) = mkdir_child(current.as_raw_fd(), name) {
                    return Err(with_cleanup(err, &[], &created, host));
                }
                created.push(built.clone());
                let next = match open_child_dir(current.as_raw_fd(), name) {
                    Ok(next) => next,
                    Err(err) => {
                        return Err(with_cleanup(err, &[], &created, host));
                    }
                };
                let rw = match open_child_rdonly(current.as_raw_fd(), name) {
                    Ok(rw) => rw,
                    Err(err) => {
                        return Err(with_cleanup(err, &[], &created, host));
                    }
                };
                if let Err(err) = ensure_mode_owner(&rw, DIR_MODE, 0, 0, host) {
                    return Err(with_cleanup(err, &[], &created, host));
                }
                current = next;
            }
            Err(err) => return Err(with_cleanup(err, &[], &created, host)),
        }
    }
    Ok((built, current, created))
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
///
/// Stored as `u32` and cast to [`libc::Ioctl`] at the call, so the same
/// bits fit musl's `c_int` request type and glibc's `c_ulong`. The kernel
/// reads and writes an `int`, not a `long`.
const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
const FS_IOC_SETFLAGS: u32 = 0x4008_6602;
const FS_APPEND_FL: libc::c_int = 0x20;

const _: () = assert!(std::mem::size_of::<libc::c_int>() == 4);

fn ioctl_request(value: u32) -> libc::Ioctl {
    value as libc::Ioctl
}

fn flag_io(err: io::Error) -> InstallError {
    InstallError::Local(format!("could not read file flags: {err}"))
}

fn unsupported_flags(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ENOTTY) | Some(libc::EOPNOTSUPP)
    )
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn get_flags(file: &File) -> Result<libc::c_int, io::Error> {
    let mut flags: libc::c_int = 0;
    // SAFETY: the request writes one `int` into `flags`. The fd is open.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), ioctl_request(FS_IOC_GETFLAGS), &mut flags) };
    if rc == 0 {
        Ok(flags)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn set_flags(file: &File, mut flags: libc::c_int) -> Result<(), InstallError> {
    // SAFETY: the request reads one `int` from `flags`. The fd is open.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), ioctl_request(FS_IOC_SETFLAGS), &mut flags) };
    if rc == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        Err(InstallError::Local(format!(
            "could not change the append-only flag: {err}"
        )))
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn get_flags(_file: &File) -> Result<libc::c_int, io::Error> {
    Err(io::Error::other(
        "the append-only flag is only set on 64-bit Linux",
    ))
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn set_flags(_file: &File, _flags: libc::c_int) -> Result<(), InstallError> {
    Err(InstallError::Local(
        "the append-only flag is only set on 64-bit Linux".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::Mutex;

    struct Fake {
        root: bool,
        gid: Option<u32>,
        chowns: Mutex<Vec<(PathBuf, u32, u32)>>,
        appends: Mutex<Vec<PathBuf>>,
        clears: Mutex<Vec<PathBuf>>,
        owners: Mutex<HashMap<(u64, u64), (u32, u32)>>,
        flags: Mutex<HashMap<(u64, u64), bool>>,
    }

    impl Fake {
        fn root_group(gid: u32) -> Self {
            Self {
                root: true,
                gid: Some(gid),
                chowns: Mutex::new(Vec::new()),
                appends: Mutex::new(Vec::new()),
                clears: Mutex::new(Vec::new()),
                owners: Mutex::new(HashMap::new()),
                flags: Mutex::new(HashMap::new()),
            }
        }

        fn claim(&self, path: &Path, uid: u32, gid: u32) {
            let file = File::open(path).unwrap();
            self.owners
                .lock()
                .unwrap()
                .insert(file_key(&file), (uid, gid));
        }

        fn claim_flag(&self, path: &Path) {
            let file = File::open(path).unwrap();
            self.flags.lock().unwrap().insert(file_key(&file), true);
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
            let key = file_key(file);
            if let Some(pair) = self.owners.lock().unwrap().get(&key) {
                return Ok(*pair);
            }
            let meta = file.metadata().map_err(io_err)?;
            Ok((meta.uid(), meta.gid()))
        }

        fn chown(&self, file: &File, uid: u32, gid: u32) -> Result<(), InstallError> {
            let path = fd_path(file);
            self.chowns.lock().unwrap().push((path, uid, gid));
            self.owners
                .lock()
                .unwrap()
                .insert(file_key(file), (uid, gid));
            Ok(())
        }

        fn append_flag(&self, file: &File) -> Result<bool, InstallError> {
            Ok(self
                .flags
                .lock()
                .unwrap()
                .get(&file_key(file))
                .copied()
                .unwrap_or(false))
        }

        fn set_append(&self, file: &File) -> Result<(), InstallError> {
            let path = fd_path(file);
            self.appends.lock().unwrap().push(path);
            self.flags.lock().unwrap().insert(file_key(file), true);
            Ok(())
        }

        fn clear_append(&self, file: &File) -> Result<(), InstallError> {
            let path = fd_path(file);
            self.clears.lock().unwrap().push(path);
            self.flags.lock().unwrap().insert(file_key(file), false);
            Ok(())
        }

        fn append_supported(&self, _dir: &File) -> Result<(), InstallError> {
            Ok(())
        }
    }

    fn file_key(file: &File) -> (u64, u64) {
        let meta = file.metadata().unwrap();
        (meta.dev(), meta.ino())
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
        std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        std::fs::write(&receipt, b"not a receipt\n").unwrap();
        let host = Fake::root_group(42);
        host.claim(&pulse, 0, 0);
        host.claim(&receipt, 0, 0);
        let err = run(dir.path(), "echo", "pulse", false, &host).unwrap_err();
        assert!(matches!(err, InstallError::RefuseAdopt), "{err}");
        assert_eq!(std::fs::read(&receipt).unwrap(), b"not a receipt\n");
        assert!(!pulse.join("anchors.log").exists());
        assert!(host.appends.lock().unwrap().is_empty());
    }

    #[test]
    fn receipt_install_refuses_a_file_not_owned_by_root() {
        let dir = tempfile::tempdir().unwrap();
        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        std::fs::write(&receipt, b"").unwrap();
        let host = Fake::root_group(42);
        host.claim(&pulse, 0, 0);
        let err = run(dir.path(), "echo", "pulse", false, &host).unwrap_err();
        assert!(
            matches!(
                err,
                InstallError::NotRootOwned {
                    file: "receipts.jsonl"
                }
            ),
            "{err}"
        );
        assert!(err.to_string().contains("stop the pulse"));
        assert!(err.to_string().contains("--adopt"));
        assert_eq!(std::fs::read(&receipt).unwrap(), b"");
        assert!(!pulse.join("anchors.log").exists());

        let anchors = pulse.join("anchors.log");
        std::fs::write(&anchors, b"keep").unwrap();
        std::fs::remove_file(&receipt).unwrap();
        let err = run(dir.path(), "echo", "pulse", true, &host).unwrap_err();
        assert!(
            matches!(
                err,
                InstallError::NotRootOwned {
                    file: "anchors.log"
                }
            ),
            "{err}"
        );
        assert!(err.to_string().contains("move or chown"), "{err}");
        assert!(!err.to_string().contains("--adopt"), "{err}");
        assert_eq!(std::fs::read(&anchors).unwrap(), b"keep");
        assert!(!receipt.exists());
    }

    #[test]
    fn receipt_install_refuses_an_unsafe_directory() {
        let dir = tempfile::tempdir().unwrap();
        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o777)).unwrap();
        let host = Fake::root_group(42);
        host.claim(&pulse, 0, 0);
        let err = run(dir.path(), "echo", "pulse", false, &host).unwrap_err();
        assert!(matches!(err, InstallError::DirUnsafe), "{err}");
        assert!(!pulse.join("receipts.jsonl").exists());
    }

    #[test]
    fn receipt_install_adopt_replaces_a_non_root_inode() {
        let dir = tempfile::tempdir().unwrap();
        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        let bytes = genesis_bytes("echo").unwrap();
        std::fs::write(&receipt, &bytes).unwrap();
        let before = std::fs::metadata(&receipt).unwrap().ino();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&receipt)
            .unwrap();
        let host = Fake::root_group(42);
        host.claim(&pulse, 0, 0);
        let outcome = run(dir.path(), "echo", "pulse", true, &host).unwrap();
        assert_eq!(outcome, Outcome::Replaced(pulse.clone()));
        assert!(summary(&outcome).contains("inode replaced"));
        let after = std::fs::metadata(&receipt).unwrap().ino();
        assert_ne!(before, after);
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
        std::os::unix::fs::FileExt::write_at(&held, b"X", 0).unwrap();
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
        assert_eq!(mode_of(&receipt), RECEIPT_MODE);
    }

    #[test]
    fn receipt_install_clears_the_append_flag_before_a_mode_change() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake::root_group(42);
        run(dir.path(), "echo", "pulse", false, &host).unwrap();
        let receipt = dir.path().join("echo/receipts.jsonl");
        std::fs::set_permissions(&receipt, std::fs::Permissions::from_mode(0o600)).unwrap();
        host.claim_flag(&receipt);
        let before = std::fs::metadata(&receipt).unwrap().ino();
        let clears = host.clears.lock().unwrap().len();
        let outcome = run(dir.path(), "echo", "pulse", true, &host).unwrap();
        assert_eq!(
            outcome,
            Outcome::Repaired {
                path: dir.path().join("echo"),
                half: false,
                layout: false,
            }
        );
        assert!(summary(&outcome).contains("mode or owner repaired"));
        assert_eq!(std::fs::metadata(&receipt).unwrap().ino(), before);
        assert!(host.clears.lock().unwrap().len() > clears);
        assert_eq!(mode_of(&receipt), RECEIPT_MODE);
        assert!(host.append_flag(&File::open(&receipt).unwrap()).unwrap());
    }

    #[test]
    fn receipt_install_refuses_a_genesis_that_names_another_pulse() {
        let dir = tempfile::tempdir().unwrap();
        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        let bytes = genesis_bytes("other").unwrap();
        std::fs::write(&receipt, &bytes).unwrap();
        let host = Fake::root_group(42);
        host.claim(&pulse, 0, 0);
        host.claim(&receipt, 0, 0);
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
        std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        let mut bytes = genesis_bytes("echo").unwrap();
        bytes.extend_from_slice(b"not-a-row\n");
        std::fs::write(&receipt, &bytes).unwrap();
        let host = Fake::root_group(42);
        host.claim(&pulse, 0, 0);
        let err = run(dir.path(), "echo", "pulse", true, &host).unwrap_err();
        match err {
            InstallError::Broken { line, .. } => assert_eq!(line, 2),
            other => panic!("{other}"),
        }
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
        assert!(host.appends.lock().unwrap().is_empty());
    }

    #[test]
    fn receipt_install_adopt_replaces_a_group_writable_root_inode() {
        let dir = tempfile::tempdir().unwrap();
        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        let bytes = genesis_bytes("echo").unwrap();
        std::fs::write(&receipt, &bytes).unwrap();
        std::fs::set_permissions(&receipt, std::fs::Permissions::from_mode(0o660)).unwrap();
        let before = std::fs::metadata(&receipt).unwrap().ino();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&receipt)
            .unwrap();
        let host = Fake::root_group(42);
        host.claim(&pulse, 0, 0);
        host.claim(&receipt, 0, 42);
        let refused = run(dir.path(), "echo", "pulse", false, &host).unwrap_err();
        assert!(matches!(refused, InstallError::RefuseAdopt), "{refused}");
        assert_eq!(mode_of(&receipt), 0o660);
        assert_eq!(std::fs::metadata(&receipt).unwrap().ino(), before);
        let outcome = run(dir.path(), "echo", "pulse", true, &host).unwrap();
        assert_eq!(outcome, Outcome::Replaced(pulse.clone()));
        assert!(summary(&outcome).contains("inode replaced"));
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
        assert_eq!(mode_of(&receipt), RECEIPT_MODE);
        let after = std::fs::metadata(&receipt).unwrap().ino();
        assert_ne!(before, after);
        assert_eq!(held.metadata().unwrap().ino(), before);
        std::os::unix::fs::FileExt::write_at(&held, b"X", 0).unwrap();
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
        assert_eq!(host.appends.lock().unwrap().len(), 1);
        assert!(pulse.join("anchors.log").exists());
    }

    #[test]
    fn receipt_install_finishes_a_flagless_receipt_without_calling_it_installed() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake::root_group(42);
        run(dir.path(), "echo", "pulse", false, &host).unwrap();
        let pulse = dir.path().join("echo");
        let receipt = pulse.join("receipts.jsonl");
        std::fs::remove_file(pulse.join("anchors.log")).unwrap();
        host.flags
            .lock()
            .unwrap()
            .remove(&file_key(&File::open(&receipt).unwrap()));
        let before = std::fs::metadata(&receipt).unwrap().ino();
        let outcome = run(dir.path(), "echo", "pulse", false, &host).unwrap();
        assert_eq!(
            outcome,
            Outcome::Repaired {
                path: pulse.clone(),
                half: true,
                layout: true,
            }
        );
        let text = summary(&outcome);
        assert!(text.starts_with("adopted "), "{text}");
        assert!(!text.starts_with("installed"), "{text}");
        assert!(text.contains(HALF_NOTE), "{text}");
        assert!(text.contains("append-only flag set"), "{text}");
        assert!(text.contains("anchors.log"), "{text}");
        assert_eq!(std::fs::metadata(&receipt).unwrap().ino(), before);
        assert!(host.append_flag(&File::open(&receipt).unwrap()).unwrap());
        assert!(pulse.join("anchors.log").exists());
    }

    #[test]
    fn receipt_install_adopt_replaces_a_root_file_with_an_acl() {
        let dir = tempfile::tempdir().unwrap();
        let pulse = dir.path().join("echo");
        std::fs::create_dir(&pulse).unwrap();
        std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
        let receipt = pulse.join("receipts.jsonl");
        let bytes = genesis_bytes("echo").unwrap();
        std::fs::write(&receipt, &bytes).unwrap();
        std::fs::set_permissions(&receipt, std::fs::Permissions::from_mode(0o640)).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&receipt)
            .unwrap();
        if !attach_user_acl(&file) {
            eprintln!("skipping ACL replacement: filesystem refused a POSIX ACL");
            return;
        }
        drop(file);
        assert_eq!(
            std::fs::metadata(&receipt).unwrap().mode() & 0o022,
            0,
            "ACL test must not rely on a group or world write bit"
        );
        let before = std::fs::metadata(&receipt).unwrap().ino();
        let host = Fake::root_group(42);
        host.claim(&pulse, 0, 0);
        host.claim(&receipt, 0, 42);
        host.claim_flag(&receipt);
        let outcome = run(dir.path(), "echo", "pulse", true, &host).unwrap();
        assert_eq!(outcome, Outcome::Replaced(pulse));
        assert_ne!(std::fs::metadata(&receipt).unwrap().ino(), before);
        assert_eq!(std::fs::read(&receipt).unwrap(), bytes);
    }

    fn attach_user_acl(file: &File) -> bool {
        // POSIX ACL xattr version 2: user obj, one named user, group obj,
        // mask, other. A named user makes `fgetxattr` report an ACL even
        // when the mode has no group or world write bit.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        for (tag, perm, id) in [
            (1u16, 4u16, 0u32),
            (2, 4, 65534),
            (4, 4, 0),
            (0x10, 4, 0),
            (0x20, 4, 0),
        ] {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&perm.to_le_bytes());
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        // SAFETY: the name is a nul-terminated literal and `bytes` is the
        // value. The descriptor is open for writing.
        let rc = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                c"system.posix_acl_access".as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
            )
        };
        rc == 0
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
        std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
        host.claim(&pulse, 0, 0);
        let outside = dir.path().join("outside");
        std::fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, pulse.join("receipts.jsonl")).unwrap();
        let err = run(dir.path(), "echo", "pulse", true, &host).unwrap_err();
        assert!(matches!(err, InstallError::Symlink), "{err}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        assert!(!pulse.join("anchors.log").exists());

        let again = dir.path().join("again");
        std::fs::create_dir(&again).unwrap();
        std::fs::set_permissions(&again, std::fs::Permissions::from_mode(0o755)).unwrap();
        host.claim(&again, 0, 0);
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
        assert!(service.contains(
            "ExecStart=/usr/local/bin/pulse-null receipts anchor --pulse %i --file /var/lib/pulse-null/%i/receipts.jsonl"
        ));
        assert!(!service.contains("WorkingDirectory="));
        assert!(!service.contains("/home/pulse"));
        assert!(service.contains("LoadCredential=anchor-token:/etc/pulse-null/anchor/token"));
        assert!(service.contains("User=root"));
        assert!(service.contains("CapabilityBoundingSet="));
        assert!(service.contains("/etc/pulse-null/anchor.toml"));
        assert!(timer.contains("OnCalendar=hourly"));
        assert!(timer.contains("pulse-null-anchor@%i.service"));
        if std::path::Path::new("/run/systemd/system").is_dir() && which_systemd_analyze() {
            for unit in ["pulse-null-anchor@.service", "pulse-null-anchor@.timer"] {
                let output = crate::process_state::during_fork(|| {
                    std::process::Command::new("systemd-analyze")
                        .arg("verify")
                        .arg(root.join(unit))
                        .output()
                        .unwrap()
                });
                let stderr = String::from_utf8_lossy(&output.stderr);
                let lower = stderr.to_ascii_lowercase();
                assert!(!lower.contains("unknown key"), "{unit}\n{stderr}");
                assert!(!lower.contains("ignoring"), "{unit}\n{stderr}");
                assert!(
                    output.status.success() || stderr.contains("is not executable"),
                    "{unit}\n{stderr}"
                );
            }
        }
    }

    #[test]
    fn receipt_install_append_flag_ioctl_argument_is_an_int() {
        assert_eq!(std::mem::size_of::<libc::c_int>(), 4);
        assert_eq!(std::mem::size_of_val(&FS_APPEND_FL), 4);
    }

    fn which_systemd_analyze() -> bool {
        crate::process_state::during_fork(|| {
            std::process::Command::new("systemd-analyze")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        })
    }
}
