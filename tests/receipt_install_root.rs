//! Real-filesystem install. Skips unless the process is root and the
//! filesystem accepts `FS_APPEND_FL`.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

#[test]
fn receipt_install_root_real_filesystem() {
    if euid() != 0 {
        eprintln!("skipping receipt_install_root_real_filesystem: not root");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let _clear = ClearAppend {
        root: dir.path().to_path_buf(),
    };
    if !flags_supported(dir.path()) {
        eprintln!(
            "skipping receipt_install_root_real_filesystem: filesystem does not support FS_APPEND_FL"
        );
        return;
    }
    let bin = env!("CARGO_BIN_EXE_pulse-null");
    let root = dir.path();
    let root_s = root.to_str().unwrap();

    let first = run(
        bin,
        &[
            "receipts", "install", "--pulse", "echo", "--group", "root", "--dir", root_s,
        ],
    );
    assert_eq!(first.0, 0, "install\n{}{}", first.1, first.2);
    assert!(first.1.contains("installed"), "{}", first.1);
    let receipt = root.join("echo/receipts.jsonl");
    let anchors = root.join("echo/anchors.log");
    assert_installed(&receipt);
    assert_eq!(std::fs::metadata(&anchors).unwrap().len(), 0);
    assert_eq!(mode_of(&anchors), 0o644);
    assert_eq!(owner_of(&anchors), (0, 0));
    let receipt_s = receipt.to_str().unwrap();
    let verified = run(bin, &["receipts", "verify", "--file", receipt_s]);
    assert_eq!(verified.0, 0, "verify\n{}{}", verified.1, verified.2);

    let second = run(
        bin,
        &[
            "receipts", "install", "--pulse", "echo", "--group", "root", "--dir", root_s,
        ],
    );
    assert_eq!(second.0, 0, "rerun\n{}{}", second.1, second.2);
    assert!(
        second.1.contains("already installed; nothing changed"),
        "{}",
        second.1
    );

    let adopt = run(
        bin,
        &[
            "receipts", "install", "--pulse", "echo", "--group", "root", "--adopt", "--dir", root_s,
        ],
    );
    assert_eq!(adopt.0, 0, "adopt\n{}{}", adopt.1, adopt.2);
    assert!(
        adopt.1.contains("already installed; nothing changed"),
        "{}",
        adopt.1
    );
    let again = run(
        bin,
        &[
            "receipts", "install", "--pulse", "echo", "--group", "root", "--adopt", "--dir", root_s,
        ],
    );
    assert_eq!(again.0, 0, "adopt again\n{}{}", again.1, again.2);
    assert!(
        again.1.contains("already installed; nothing changed"),
        "{}",
        again.1
    );
    assert_installed(&receipt);

    let migrated = root.join("moved");
    std::fs::create_dir(&migrated).unwrap();
    let pulse = migrated.join("echo");
    std::fs::create_dir(&pulse).unwrap();
    std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
    let old = pulse.join("receipts.jsonl");
    std::fs::write(&old, std::fs::read(&receipt).unwrap()).unwrap();
    std::os::unix::fs::chown(&old, Some(65534), Some(65534)).unwrap();
    let before = std::fs::metadata(&old).unwrap().ino();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&old)
        .unwrap();
    let migrated_s = migrated.to_str().unwrap();
    let copied = run(
        bin,
        &[
            "receipts", "install", "--pulse", "echo", "--group", "root", "--adopt", "--dir",
            migrated_s,
        ],
    );
    assert_eq!(copied.0, 0, "adopt copy\n{}{}", copied.1, copied.2);
    assert!(copied.1.contains("inode replaced"), "{}", copied.1);
    let after = std::fs::metadata(&old).unwrap().ino();
    assert_ne!(before, after);
    std::os::unix::fs::FileExt::write_at(&held, b"X", 0).unwrap();
    let now = std::fs::read(&old).unwrap();
    assert!(
        !now.starts_with(b"X"),
        "old descriptor changed the new file"
    );
    assert_installed(&old);
    let second_copy = run(
        bin,
        &[
            "receipts", "install", "--pulse", "echo", "--group", "root", "--adopt", "--dir",
            migrated_s,
        ],
    );
    assert_eq!(
        second_copy.0, 0,
        "adopt copy again\n{}{}",
        second_copy.1, second_copy.2
    );
    assert!(
        second_copy.1.contains("already installed; nothing changed"),
        "{}",
        second_copy.1
    );
    assert_eq!(std::fs::metadata(&old).unwrap().ino(), after);
    let old_s = old.to_str().unwrap();
    let verified = run(bin, &["receipts", "verify", "--file", old_s]);
    assert_eq!(verified.0, 0, "verify copy\n{}{}", verified.1, verified.2);

    let (group, gid) = plain_group();
    let writable = root.join("writable");
    std::fs::create_dir(&writable).unwrap();
    let pulse = writable.join("echo");
    std::fs::create_dir(&pulse).unwrap();
    std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
    let wide = pulse.join("receipts.jsonl");
    std::fs::write(&wide, std::fs::read(&receipt).unwrap()).unwrap();
    std::os::unix::fs::chown(&wide, Some(0), Some(gid)).unwrap();
    std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o660)).unwrap();
    let wide_before = std::fs::metadata(&wide).unwrap().ino();
    let wide_held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&wide)
        .unwrap();
    let writable_s = writable.to_str().unwrap();
    let group_s = group.as_str();
    let adopted = run(
        bin,
        &[
            "receipts", "install", "--pulse", "echo", "--group", group_s, "--adopt", "--dir",
            writable_s,
        ],
    );
    assert_eq!(adopted.0, 0, "adopt 0660\n{}{}", adopted.1, adopted.2);
    assert!(adopted.1.contains("inode replaced"), "{}", adopted.1);
    let wide_after = std::fs::metadata(&wide).unwrap().ino();
    assert_ne!(wide_before, wide_after);
    std::os::unix::fs::FileExt::write_at(&wide_held, b"X", 0).unwrap();
    assert!(
        !std::fs::read(&wide).unwrap().starts_with(b"X"),
        "old descriptor changed the new file"
    );
    assert_installed_as(&wide, gid);
    let wide_s = wide.to_str().unwrap();
    let verified = run(bin, &["receipts", "verify", "--file", wide_s]);
    assert_eq!(verified.0, 0, "verify 0660\n{}{}", verified.1, verified.2);

    // The destination already has the append-only flag. `renameat` over that
    // inode is refused, so install clears the flag and renames again.
    let flagged = root.join("flagged");
    std::fs::create_dir(&flagged).unwrap();
    let pulse = flagged.join("echo");
    std::fs::create_dir(&pulse).unwrap();
    std::fs::set_permissions(&pulse, std::fs::Permissions::from_mode(0o755)).unwrap();
    let marked = pulse.join("receipts.jsonl");
    std::fs::write(&marked, std::fs::read(&receipt).unwrap()).unwrap();
    std::os::unix::fs::chown(&marked, Some(0), Some(gid)).unwrap();
    std::fs::set_permissions(&marked, std::fs::Permissions::from_mode(0o660)).unwrap();
    let marked_held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&marked)
        .unwrap();
    let chattr = Command::new("chattr")
        .arg("+a")
        .arg(&marked)
        .status()
        .unwrap();
    assert!(chattr.success(), "chattr +a");
    let marked_before = std::fs::metadata(&marked).unwrap().ino();
    let flagged_s = flagged.to_str().unwrap();
    let over = run(
        bin,
        &[
            "receipts", "install", "--pulse", "echo", "--group", group_s, "--adopt", "--dir",
            flagged_s,
        ],
    );
    assert_eq!(over.0, 0, "adopt append-only\n{}{}", over.1, over.2);
    assert!(over.1.contains("inode replaced"), "{}", over.1);
    assert_ne!(std::fs::metadata(&marked).unwrap().ino(), marked_before);
    let _ = std::os::unix::fs::FileExt::write_at(&marked_held, b"X", 0);
    assert!(!std::fs::read(&marked).unwrap().starts_with(b"X"));
    assert_installed_as(&marked, gid);
    let marked_s = marked.to_str().unwrap();
    let verified = run(bin, &["receipts", "verify", "--file", marked_s]);
    assert_eq!(
        verified.0, 0,
        "verify append-only\n{}{}",
        verified.1, verified.2
    );
}

fn euid() -> u32 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            return rest.split_whitespace().next().unwrap().parse().unwrap();
        }
    }
    panic!("no Uid in /proc/self/status");
}

fn flags_supported(dir: &Path) -> bool {
    let probe = dir.join(".flag-probe");
    std::fs::write(&probe, b"x").unwrap();
    let status = Command::new("chattr").arg("+a").arg(&probe).status();
    let _ = Command::new("chattr").arg("-a").arg(&probe).status();
    let _ = std::fs::remove_file(&probe);
    status.map(|status| status.success()).unwrap_or(false)
}

fn run(bin: &str, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(bin).args(args).output().expect("pulse-null");
    (
        output.status.code().unwrap_or(1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn assert_installed(path: &Path) {
    assert_installed_as(path, 0);
}

fn assert_installed_as(path: &Path, gid: u32) {
    let meta = std::fs::metadata(path).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o640, "mode");
    assert_eq!((meta.uid(), meta.gid()), (0, gid), "owner");
    let listed = Command::new("lsattr").arg(path).output().unwrap();
    let text = String::from_utf8_lossy(&listed.stdout);
    let field = text.split_whitespace().next().unwrap_or("");
    assert!(
        field.contains('a'),
        "append-only flag missing\n{text}{}",
        String::from_utf8_lossy(&listed.stderr)
    );
}

fn plain_group() -> (String, u32) {
    let text = std::fs::read_to_string("/etc/group").unwrap();
    for line in text.lines() {
        let mut parts = line.split(':');
        let name = parts.next().unwrap_or("");
        let _ = parts.next();
        let gid = parts.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
        if gid == 0 || name.is_empty() || name.starts_with('.') {
            continue;
        }
        let plain = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if plain {
            return (name.to_string(), gid);
        }
    }
    ("root".to_string(), 0)
}

struct ClearAppend {
    root: std::path::PathBuf,
}

impl Drop for ClearAppend {
    fn drop(&mut self) {
        clear_tree(&self.root);
    }
}

fn clear_tree(path: &Path) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => clear_tree(&path),
            Ok(kind) if kind.is_file() => {
                let _ = Command::new("chattr").arg("-a").arg(&path).status();
            }
            _ => {}
        }
    }
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().mode() & 0o777
}

fn owner_of(path: &Path) -> (u32, u32) {
    let meta = std::fs::metadata(path).unwrap();
    (meta.uid(), meta.gid())
}
