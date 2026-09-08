//! Entity-local wiring that every agent CLI shares, and the file mechanics
//! the per-CLI integrations build on.
//!
//! Everything an agent CLI needs to run *as* an entity lives inside that
//! entity's directory. This module owns the CLI-independent part — the
//! memory directory, `memory/.recall-echo.toml` carrying the entity's own
//! provider, the conversations link — plus the symlink-safe, contained
//! read/write helpers. Which instruction file, hooks or rules a given CLI
//! reads is that CLI's adapter's business (`cli_provider::adapters`).
//!
//! Nothing here writes to the user's home. The pre-PN-104 design symlinked
//! `~/.claude/{ARCHIVE.md,EPHEMERAL.md,memories}` into one entity, which made
//! a second entity under the same unix user impossible.

use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct BootstrapItem {
    pub path: PathBuf,
    #[allow(dead_code)]
    pub kind: ItemKind,
    pub status: ItemStatus,
}

#[derive(Debug)]
pub enum ItemKind {
    Symlink,
    Directory,
    ConfigFile,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ItemStatus {
    Created,
    /// Existed with other content and was brought up to date (settings.json
    /// merge, absolute → relative symlink).
    Updated,
    Exists,
    Missing,
    Wrong(String),
    Skipped(String),
}

impl fmt::Display for BootstrapItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let icon = match &self.status {
            ItemStatus::Created | ItemStatus::Updated => "\x1b[32m✓\x1b[0m",
            ItemStatus::Exists => "\x1b[2m·\x1b[0m",
            ItemStatus::Missing => "\x1b[33m✗\x1b[0m",
            ItemStatus::Wrong(_) => "\x1b[33m✗\x1b[0m",
            ItemStatus::Skipped(_) => "\x1b[33m⚠\x1b[0m",
        };
        let detail = match &self.status {
            ItemStatus::Created => " created".to_string(),
            ItemStatus::Updated => " updated".to_string(),
            ItemStatus::Exists => " ok".to_string(),
            ItemStatus::Missing => " missing".to_string(),
            ItemStatus::Wrong(reason) => format!(" wrong: {reason}"),
            ItemStatus::Skipped(reason) => format!(" skipped: {reason}"),
        };
        write!(
            f,
            "  {} {}{}",
            icon,
            printable(&self.path.display().to_string()),
            printable(&detail)
        )
    }
}

/// Paths and reasons can come from files we did not write. Keep terminal
/// control sequences and bidi overrides out of what we print about them.
pub fn printable(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control()
                || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
            {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// Find the recall-echo binary path.
pub fn find_recall_echo_bin() -> String {
    find_recall_echo_bin_in(home_dir().as_deref())
}

fn find_recall_echo_bin_in(home: Option<&Path>) -> String {
    let mut candidates = Vec::new();
    if let Some(home) = home {
        candidates.push(home.join(".cargo/bin/recall-echo"));
    }
    candidates.push(PathBuf::from("/usr/local/bin/recall-echo"));
    for c in &candidates {
        if c.is_file() {
            return c.to_string_lossy().to_string();
        }
    }
    "recall-echo".to_string()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Single-quote a path for a `sh -c` hook command. The only character that
/// needs care inside single quotes is the single quote itself.
pub(crate) fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

/// Largest config file this module will read. These are hand-sized JSON
/// documents; anything bigger is not one of ours.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Read a small regular file. `Ok(None)` when absent; an error names why a
/// present path was refused (symlink, not a regular file, too large). The
/// file is opened once with `O_NOFOLLOW` and inspected through the handle,
/// so nothing can be swapped in between the check and the read.
pub(crate) fn read_small_file(path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err("is a symlink — refusing to follow it".into())
        }
        Err(e) => return Err(e.to_string()),
    };
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(format!("larger than {MAX_CONFIG_BYTES} bytes"));
    }
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    Ok(Some(text))
}

/// Write `content` to `path` atomically, never through a symlink at any
/// component: the parent must already exist and canonicalise inside
/// `within`; the temp file is created `O_EXCL` with a random name and
/// mode 0600 (or the mode of the file it replaces), fsynced, then renamed.
pub(crate) fn write_regular_file(path: &Path, content: &str, within: &Path) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let existing = match path.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() => {
            return Err("is a symlink — refusing to write through it".into());
        }
        Ok(m) => Some(m),
        Err(_) => None,
    };
    let parent = path.parent().ok_or("no parent directory")?;
    let parent_real = parent
        .canonicalize()
        .map_err(|e| format!("parent directory: {e}"))?;
    if !parent_real.starts_with(within) {
        return Err(format!(
            "parent {} resolves outside the entity ({})",
            parent.display(),
            parent_real.display()
        ));
    }
    let name = path.file_name().ok_or("no file name")?.to_string_lossy();
    let tmp = parent_real.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple()));
    let mode = existing
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    let written = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string());
    drop(file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, parent_real.join(&*name)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Create a directory if it doesn't exist. A symlink where the directory
/// should be is refused, not followed.
pub(crate) fn ensure_dir(path: &Path) -> BootstrapItem {
    if let Ok(meta) = path.symlink_metadata() {
        let status = if meta.file_type().is_symlink() {
            ItemStatus::Skipped("is a symlink — refusing to follow it".into())
        } else if meta.is_dir() {
            ItemStatus::Exists
        } else {
            ItemStatus::Skipped("regular file exists at path".into())
        };
        return BootstrapItem {
            path: path.to_path_buf(),
            kind: ItemKind::Directory,
            status,
        };
    }
    match std::fs::create_dir_all(path) {
        Ok(()) => BootstrapItem {
            path: path.to_path_buf(),
            kind: ItemKind::Directory,
            status: ItemStatus::Created,
        },
        Err(e) => BootstrapItem {
            path: path.to_path_buf(),
            kind: ItemKind::Directory,
            status: ItemStatus::Skipped(e.to_string()),
        },
    }
}

/// Write a config file if it doesn't exist. Never overwrites: an existing
/// file may carry hand-tuned settings (Echo's graph config lives in its
/// `.recall-echo.toml`).
pub(crate) fn ensure_config(path: &Path, content: &str, within: &Path) -> BootstrapItem {
    let item = |status| BootstrapItem {
        path: path.to_path_buf(),
        kind: ItemKind::ConfigFile,
        status,
    };
    if let Ok(meta) = path.symlink_metadata() {
        return item(if meta.file_type().is_symlink() {
            ItemStatus::Skipped("is a symlink — refusing to follow it".into())
        } else {
            ItemStatus::Exists
        });
    }
    match write_regular_file(path, content, within) {
        Ok(()) => item(ItemStatus::Created),
        Err(e) => item(ItemStatus::Skipped(e)),
    }
}

/// `memory/conversations -> ../archives/conversations`, relative so the
/// entity tree survives being moved. An absolute link to the same place is
/// rewritten; anything else is left alone and reported.
fn ensure_conversations_link(entity_root: &Path) -> BootstrapItem {
    let link = entity_root.join("memory/conversations");
    let target = entity_root.join("archives/conversations");
    let relative = PathBuf::from("../archives/conversations");
    let item = |status| BootstrapItem {
        path: link.clone(),
        kind: ItemKind::Symlink,
        status,
    };
    if !target.exists() {
        return item(ItemStatus::Skipped(
            "archives/conversations does not exist yet".into(),
        ));
    }
    let mut replacing = false;
    if link.symlink_metadata().is_ok() {
        if !link.is_symlink() {
            return item(ItemStatus::Skipped(
                "regular file/dir exists at path".into(),
            ));
        }
        match std::fs::read_link(&link) {
            Ok(existing) if existing == relative => return item(ItemStatus::Exists),
            Ok(_) => {
                let same_place = link
                    .canonicalize()
                    .ok()
                    .zip(target.canonicalize().ok())
                    .is_some_and(|(a, b)| a == b);
                if !same_place {
                    return item(ItemStatus::Skipped(
                        "points somewhere other than archives/conversations".into(),
                    ));
                }
                if let Err(e) = std::fs::remove_file(&link) {
                    return item(ItemStatus::Skipped(e.to_string()));
                }
                replacing = true;
            }
            Err(e) => return item(ItemStatus::Skipped(e.to_string())),
        }
    }
    if let Some(parent) = link.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::os::unix::fs::symlink(&relative, &link) {
        Ok(()) if replacing => item(ItemStatus::Updated),
        Ok(()) => item(ItemStatus::Created),
        Err(e) => item(ItemStatus::Skipped(e.to_string())),
    }
}

/// Generate the recall-echo.toml config content. `recall_provider` is the
/// value recall-echo's `[llm] provider` takes for the entity's agent CLI —
/// the same CLI does the entity's thinking and recall-echo's extraction, and
/// its sessions are what recall-echo's capture sweep should look for.
pub(crate) fn render_recall_echo_toml(entity_root: &Path, recall_provider: &str) -> String {
    // TOML string literals, escaped by the toml crate — a quote or newline
    // in the path must not be able to open a new table.
    let docs_dir = toml::Value::String(format!("{}/journal", entity_root.display())).to_string();
    let provider = toml::Value::String(recall_provider.to_string()).to_string();
    format!(
        r#"[ephemeral]
max_entries = 5

[llm]
provider = {provider}
model = ""
api_base = ""

[capture]
sources = [{provider}]

[pipeline]
docs_dir = {docs_dir}
auto_sync = true
"#
    )
}

/// The agent-CLI-independent part of an entity's wiring: the memory
/// directory, recall-echo's config with the entity's provider, and the
/// conversations link. Safe to run repeatedly. A root that is not valid
/// UTF-8 is refused: a lossy path would be persisted into config.
pub fn ensure_common(entity_root: &Path, recall_provider: &str) -> Vec<BootstrapItem> {
    let entity_root = entity_root
        .canonicalize()
        .unwrap_or_else(|_| entity_root.to_path_buf());
    if entity_root.to_str().is_none() {
        return vec![BootstrapItem {
            path: entity_root,
            kind: ItemKind::Directory,
            status: ItemStatus::Skipped("entity root is not valid UTF-8".into()),
        }];
    }
    let memory_dir = entity_root.join("memory");
    let mut items = Vec::new();
    items.extend(migrate_legacy_instructions(&entity_root));
    let memory = ensure_dir(&memory_dir);
    let memory_ok = matches!(memory.status, ItemStatus::Created | ItemStatus::Exists);
    items.push(memory);
    if memory_ok {
        items.push(ensure_config(
            &memory_dir.join(".recall-echo.toml"),
            &render_recall_echo_toml(&entity_root, recall_provider),
            &entity_root,
        ));
        items.push(ensure_conversations_link(&entity_root));
    }
    items
}

/// Entities created before PN-106 keep their instructions in the file one
/// vendor's CLI reads. Copy it to the generic `INSTRUCTIONS.md` once, so the
/// prompt builder and every adapter's pointer file have something to point
/// at. Nothing is copied when INSTRUCTIONS.md exists or the legacy file is
/// itself only a pointer.
fn migrate_legacy_instructions(entity_root: &Path) -> Vec<BootstrapItem> {
    let generic = entity_root.join("INSTRUCTIONS.md");
    if generic.exists() {
        return Vec::new();
    }
    let legacy = entity_root.join("CLAUDE.md"); // vendor-ok: pre-PN-106 instruction file
    let Ok(Some(text)) = read_small_file(&legacy) else {
        return Vec::new();
    };
    if text.trim().is_empty() || text.contains("@INSTRUCTIONS.md") {
        return Vec::new();
    }
    vec![ensure_config(&generic, &text, entity_root)]
}

/// Report the state of the agent-CLI-independent wiring without changing
/// anything.
pub fn verify_common(entity_root: &Path, recall_provider: &str) -> Vec<BootstrapItem> {
    let entity_root = entity_root
        .canonicalize()
        .unwrap_or_else(|_| entity_root.to_path_buf());
    let mut items = Vec::new();
    let toml_path = entity_root.join("memory/.recall-echo.toml");
    // Present, and naming the entity's own provider: `ensure_config` never
    // rewrites the file, so an entity whose adapter changed would otherwise
    // keep recall-echo extracting with the previous CLI.
    let status = match read_small_file(&toml_path) {
        Ok(Some(text)) => match toml::from_str::<toml::Value>(&text) {
            Ok(doc) => {
                let configured = doc
                    .get("llm")
                    .and_then(|l| l.get("provider"))
                    .and_then(|p| p.as_str());
                if configured == Some(recall_provider) {
                    ItemStatus::Exists
                } else {
                    ItemStatus::Wrong(format!(
                        "[llm] provider is {} but this entity's agent is {recall_provider}",
                        configured.unwrap_or("unset")
                    ))
                }
            }
            Err(e) => ItemStatus::Wrong(format!("not valid TOML: {e}")),
        },
        Ok(None) => ItemStatus::Missing,
        Err(e) => ItemStatus::Wrong(e),
    };
    items.push(BootstrapItem {
        status,
        path: toml_path,
        kind: ItemKind::ConfigFile,
    });
    let link = entity_root.join("memory/conversations");
    items.push(BootstrapItem {
        status: if link.is_symlink() {
            ItemStatus::Exists
        } else {
            ItemStatus::Missing
        },
        path: link,
        kind: ItemKind::Symlink,
    });
    if find_recall_echo_bin() == "recall-echo" {
        items.push(BootstrapItem {
            path: PathBuf::from("recall-echo"),
            kind: ItemKind::ConfigFile,
            status: ItemStatus::Missing,
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("archives/conversations")).unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        dir
    }

    #[test]
    fn existing_recall_toml_untouched() {
        let dir = entity();
        let toml = dir.path().join("memory/.recall-echo.toml");
        std::fs::write(&toml, "[graph]\nmode = \"server\"\n").unwrap();
        ensure_common(dir.path(), "claude-code");
        assert_eq!(
            std::fs::read_to_string(&toml).unwrap(),
            "[graph]\nmode = \"server\"\n"
        );
    }

    #[test]
    fn conversations_link_is_relative() {
        let dir = entity();
        let root = dir.path().canonicalize().unwrap();
        // An absolute link to the right place gets rewritten as relative.
        std::os::unix::fs::symlink(
            root.join("archives/conversations"),
            root.join("memory/conversations"),
        )
        .unwrap();
        let items = ensure_common(&root, "claude-code");
        let link = items
            .iter()
            .find(|i| i.path.ends_with("memory/conversations"))
            .unwrap();
        assert_eq!(link.status, ItemStatus::Updated);
        assert_eq!(
            std::fs::read_link(root.join("memory/conversations")).unwrap(),
            PathBuf::from("../archives/conversations")
        );

        // A link somewhere else is left alone.
        let other = tempfile::tempdir().unwrap();
        std::fs::remove_file(root.join("memory/conversations")).unwrap();
        std::os::unix::fs::symlink(other.path(), root.join("memory/conversations")).unwrap();
        let items = ensure_common(&root, "claude-code");
        let link = items
            .iter()
            .find(|i| i.path.ends_with("memory/conversations"))
            .unwrap();
        assert!(matches!(link.status, ItemStatus::Skipped(_)));
    }

    #[test]
    fn recall_toml_escapes_the_path() {
        let toml_text = render_recall_echo_toml(
            Path::new("/srv/evil\"\n[llm]\napi_base = \"http://x"),
            "grok",
        );
        let parsed: toml::Value = toml::from_str(&toml_text).expect("still one document");
        assert!(parsed
            .get("llm")
            .and_then(|l| l.get("api_base"))
            .is_some_and(|v| v.as_str() == Some("")));
        assert!(parsed["pipeline"]["docs_dir"]
            .as_str()
            .unwrap()
            .starts_with("/srv/evil\""));
    }

    #[test]
    fn printable_scrubs_control_and_bidi_but_keeps_unicode() {
        assert_eq!(printable("a\u{1b}[2Kb\u{202E}c"), "a\u{FFFD}[2Kb\u{FFFD}c");
        assert_eq!(printable("café 日本 🦀"), "café 日本 🦀");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote(Path::new("/a/b c")), "'/a/b c'");
        assert_eq!(shell_quote(Path::new("/a/it's")), "'/a/it'\\''s'");
    }
}
