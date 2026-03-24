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

#[derive(Debug)]
pub enum ItemStatus {
    Created,
    Exists,
    Missing,
    Wrong(String),
    Skipped(String),
}

impl fmt::Display for BootstrapItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let icon = match &self.status {
            ItemStatus::Created => "\x1b[32m✓\x1b[0m",
            ItemStatus::Exists => "\x1b[2m·\x1b[0m",
            ItemStatus::Missing => "\x1b[33m✗\x1b[0m",
            ItemStatus::Wrong(_) => "\x1b[33m✗\x1b[0m",
            ItemStatus::Skipped(_) => "\x1b[33m⚠\x1b[0m",
        };
        let detail = match &self.status {
            ItemStatus::Created => " created".to_string(),
            ItemStatus::Exists => " ok".to_string(),
            ItemStatus::Missing => " missing".to_string(),
            ItemStatus::Wrong(reason) => format!(" wrong: {reason}"),
            ItemStatus::Skipped(reason) => format!(" skipped: {reason}"),
        };
        write!(f, "  {} {}{}", icon, self.path.display(), detail)
    }
}

/// Find the recall-echo binary path.
fn find_recall_echo_bin(home: &Path) -> String {
    let candidates = [
        home.join(".cargo/bin/recall-echo"),
        PathBuf::from("/usr/local/bin/recall-echo"),
    ];
    for c in &candidates {
        if c.exists() {
            return c.to_string_lossy().to_string();
        }
    }
    "recall-echo".to_string()
}

/// Create a symlink if it doesn't exist or already points to the right target.
fn ensure_symlink(link: &Path, target: &Path) -> BootstrapItem {
    if link.symlink_metadata().is_ok() {
        if link.is_symlink() {
            if let Ok(existing_target) = std::fs::read_link(link) {
                if existing_target == target {
                    return BootstrapItem {
                        path: link.to_path_buf(),
                        kind: ItemKind::Symlink,
                        status: ItemStatus::Exists,
                    };
                }
                return BootstrapItem {
                    path: link.to_path_buf(),
                    kind: ItemKind::Symlink,
                    status: ItemStatus::Skipped(format!(
                        "points to {} instead of {}",
                        existing_target.display(),
                        target.display()
                    )),
                };
            }
        }
        return BootstrapItem {
            path: link.to_path_buf(),
            kind: ItemKind::Symlink,
            status: ItemStatus::Skipped("regular file/dir exists at path".to_string()),
        };
    }

    // Create parent dirs if needed
    if let Some(parent) = link.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match std::os::unix::fs::symlink(target, link) {
        Ok(()) => BootstrapItem {
            path: link.to_path_buf(),
            kind: ItemKind::Symlink,
            status: ItemStatus::Created,
        },
        Err(e) => BootstrapItem {
            path: link.to_path_buf(),
            kind: ItemKind::Symlink,
            status: ItemStatus::Skipped(e.to_string()),
        },
    }
}

/// Create a directory if it doesn't exist.
fn ensure_dir(path: &Path) -> BootstrapItem {
    if path.exists() {
        return BootstrapItem {
            path: path.to_path_buf(),
            kind: ItemKind::Directory,
            status: ItemStatus::Exists,
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

/// Write a config file if it doesn't exist.
fn ensure_config(path: &Path, content: &str) -> BootstrapItem {
    if path.exists() {
        return BootstrapItem {
            path: path.to_path_buf(),
            kind: ItemKind::ConfigFile,
            status: ItemStatus::Exists,
        };
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(path, content) {
        Ok(()) => BootstrapItem {
            path: path.to_path_buf(),
            kind: ItemKind::ConfigFile,
            status: ItemStatus::Created,
        },
        Err(e) => BootstrapItem {
            path: path.to_path_buf(),
            kind: ItemKind::ConfigFile,
            status: ItemStatus::Skipped(e.to_string()),
        },
    }
}

/// Generate the recall-echo.toml config content.
fn render_recall_echo_toml(entity_root: &Path) -> String {
    format!(
        r#"[ephemeral]
max_entries = 5

[llm]
provider = "claude-code"
model = ""
api_base = ""

[pipeline]
docs_dir = "{}/journal"
auto_sync = true
"#,
        entity_root.display()
    )
}

/// Generate the settings.json hooks content.
fn render_settings_json(recall_bin: &str) -> String {
    serde_json::json!({
        "hooks": {
            "PreCompact": [{
                "hooks": [{
                    "type": "command",
                    "command": format!("{} checkpoint --trigger precompact", recall_bin)
                }]
            }],
            "SessionEnd": [{
                "hooks": [{
                    "type": "command",
                    "command": format!("{} archive-session", recall_bin)
                }]
            }]
        }
    })
    .to_string()
}

/// Generate the recall-echo.md rules file with entity-specific paths.
fn render_rules_md(entity_root: &Path) -> String {
    format!(
        r#"# recall-echo — Memory Protocol

You have a persistent three-layer memory system. Use it to maintain continuity across sessions.

## Memory Layers

### Layer 1 — Curated Memory (MEMORY.md)
- Location: `{entity_root}/memory/MEMORY.md`
- Your source of truth. Distilled facts, preferences, patterns, key decisions.
- Auto-loaded at session start (first 200 lines).
- Keep under 200 lines. Only write confirmed, stable information.
- Before adding, check if an existing entry should be updated. No duplicates.

### Layer 2 — Recent Sessions (EPHEMERAL.md)
@~/.claude/EPHEMERAL.md
- Rolling window of your last 5 session summaries.
- Read at session start to orient on recent work.
- Each entry has a pointer to the full archive.
- Managed automatically by recall-echo hooks. Do not edit manually.

### Layer 3 — Full Archive (conversations/)
- Index: `~/.claude/ARCHIVE.md`
- Full conversations: `~/.claude/conversations/conversation-NNN.md`
- NOT loaded into context. Search on demand using Grep.
- To search: `Grep pattern="search term" path="~/.claude/conversations/"`

## Session Lifecycle

### On session start:
1. MEMORY.md is in your context (auto-loaded).
2. EPHEMERAL.md is in your context (via @ import above).
3. Orient from recent sessions. Use archive pointers if you need full context.

### During the session:
- Update MEMORY.md when you learn stable facts.
- When the user references past work, search the archive first.
- Do NOT update MEMORY.md with speculative or session-specific info.

### On PreCompact (context about to be compressed):
The PreCompact hook automatically runs `recall-echo checkpoint --trigger precompact`.
The output tells you the file path and log number. Open that file and fill in the
Summary, Key Details, Action Items, and Unresolved sections with context from the
current conversation.

### On session end:
- The SessionEnd hook archives this conversation automatically.
- No manual action required.

## Rules

- Never write duplicates to MEMORY.md. Check first, update if exists.
- When MEMORY.md approaches 200 lines, distill it.
- Archive conversations are immutable. Never modify them.
- When the user says "we discussed this before" — search archives before saying you don't remember.
"#,
        entity_root = entity_root.display()
    )
}

/// Create all Claude Code integration files and symlinks.
/// Safe to run multiple times — skips anything already correct.
pub fn ensure(entity_root: &Path, home_dir: &Path) -> Vec<BootstrapItem> {
    let claude_dir = home_dir.join(".claude");
    let recall_bin = find_recall_echo_bin(home_dir);
    let mut items = vec![
        // Directories
        ensure_dir(&claude_dir),
        ensure_dir(&claude_dir.join("memory")),
        ensure_dir(&claude_dir.join("hooks")),
        ensure_dir(&claude_dir.join("rules")),
        // Symlinks: ~/.claude/ → entity/memory/
        ensure_symlink(
            &claude_dir.join("ARCHIVE.md"),
            &entity_root.join("memory/ARCHIVE.md"),
        ),
    ];

    items.push(ensure_symlink(
        &claude_dir.join("EPHEMERAL.md"),
        &entity_root.join("memory/EPHEMERAL.md"),
    ));
    items.push(ensure_symlink(
        &claude_dir.join("memories"),
        &entity_root.join("memory"),
    ));

    // Symlink: entity/memory/conversations → entity/archives/conversations
    let conversations_link = entity_root.join("memory/conversations");
    let conversations_target = entity_root.join("archives/conversations");
    if conversations_target.exists() {
        items.push(ensure_symlink(&conversations_link, &conversations_target));
    }

    // Config files
    items.push(ensure_config(
        &claude_dir.join("memory/.recall-echo.toml"),
        &render_recall_echo_toml(entity_root),
    ));
    items.push(ensure_config(
        &claude_dir.join("settings.json"),
        &render_settings_json(&recall_bin),
    ));
    items.push(ensure_config(
        &claude_dir.join("rules/recall-echo.md"),
        &render_rules_md(entity_root),
    ));

    items
}

/// Verify Claude Code integration without creating anything.
pub fn verify(entity_root: &Path, home_dir: &Path) -> Vec<BootstrapItem> {
    let claude_dir = home_dir.join(".claude");
    let mut items = Vec::new();

    // Check symlinks
    let symlinks: Vec<(PathBuf, PathBuf)> = vec![
        (
            claude_dir.join("ARCHIVE.md"),
            entity_root.join("memory/ARCHIVE.md"),
        ),
        (
            claude_dir.join("EPHEMERAL.md"),
            entity_root.join("memory/EPHEMERAL.md"),
        ),
        (claude_dir.join("memories"), entity_root.join("memory")),
    ];

    for (link, target) in &symlinks {
        if link.symlink_metadata().is_err() {
            items.push(BootstrapItem {
                path: link.clone(),
                kind: ItemKind::Symlink,
                status: ItemStatus::Missing,
            });
        } else if link.is_symlink() {
            if let Ok(existing) = std::fs::read_link(link) {
                if existing == *target {
                    items.push(BootstrapItem {
                        path: link.clone(),
                        kind: ItemKind::Symlink,
                        status: ItemStatus::Exists,
                    });
                } else {
                    items.push(BootstrapItem {
                        path: link.clone(),
                        kind: ItemKind::Symlink,
                        status: ItemStatus::Wrong(format!(
                            "points to {} instead of {}",
                            existing.display(),
                            target.display()
                        )),
                    });
                }
            }
        }
    }

    // Check config files
    let configs: Vec<PathBuf> = vec![
        claude_dir.join("memory/.recall-echo.toml"),
        claude_dir.join("settings.json"),
        claude_dir.join("rules/recall-echo.md"),
    ];

    for path in &configs {
        if path.exists() {
            items.push(BootstrapItem {
                path: path.clone(),
                kind: ItemKind::ConfigFile,
                status: ItemStatus::Exists,
            });
        } else {
            items.push(BootstrapItem {
                path: path.clone(),
                kind: ItemKind::ConfigFile,
                status: ItemStatus::Missing,
            });
        }
    }

    // Check recall-echo binary
    let recall_bin = find_recall_echo_bin(home_dir);
    if recall_bin == "recall-echo" {
        items.push(BootstrapItem {
            path: PathBuf::from("recall-echo"),
            kind: ItemKind::ConfigFile,
            status: ItemStatus::Missing,
        });
    }

    items
}
