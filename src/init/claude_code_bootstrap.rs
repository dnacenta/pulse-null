//! Claude Code integration, scoped to one entity.
//!
//! Everything Claude Code needs to run *as* an entity lives inside that
//! entity's directory: `.claude/settings.json` (recall-echo hooks that carry
//! the entity root explicitly), `.claude/rules/recall-echo.md` (the memory
//! protocol, entity-relative), and `memory/.recall-echo.toml` (where
//! recall-echo reads its config). The provider runs `claude` with the entity
//! as cwd, so Claude Code picks these up as project-scope configuration.
//!
//! Nothing here writes to `$HOME/.claude`. The previous design symlinked the
//! user's `~/.claude/{ARCHIVE.md,EPHEMERAL.md,memories}` into one entity,
//! which made a second entity under the same unix user impossible (PN-104).
//! [`verify`] still *looks* at `$HOME/.claude`, but only to report leftovers
//! from that design so `pulse-null repair` can retire them.

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
        write!(f, "  {} {}{}", icon, self.path.display(), detail)
    }
}

/// The three recall-echo hook subcommands, in the event order Claude Code
/// fires them. `consume` takes the root positionally; the other two take
/// `--entity-root` — matching what `recall-echo init` itself writes.
const RECALL_HOOKS: [(&str, &str); 3] = [
    ("SessionStart", "consume"),
    ("PreCompact", "checkpoint"),
    ("SessionEnd", "archive-session"),
];

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
        if c.exists() {
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
fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

/// The canonical hook command for one subcommand.
fn hook_command(recall_bin: &str, sub: &str, root: &Path) -> String {
    let root = shell_quote(root);
    match sub {
        "consume" => format!("{recall_bin} consume {root}"),
        "checkpoint" => {
            format!("{recall_bin} checkpoint --trigger precompact --entity-root {root}")
        }
        _ => format!("{recall_bin} {sub} --entity-root {root}"),
    }
}

/// Our hooks, keyed by event, in Claude Code's settings.json shape.
fn render_hooks(recall_bin: &str, root: &Path) -> serde_json::Value {
    let mut hooks = serde_json::Map::new();
    for (event, sub) in RECALL_HOOKS {
        let mut entry = serde_json::Map::new();
        if event == "SessionStart" {
            entry.insert("matcher".into(), "startup|resume".into());
        }
        entry.insert(
            "hooks".into(),
            serde_json::json!([{
                "type": "command",
                "command": hook_command(recall_bin, sub, root)
            }]),
        );
        hooks.insert(event.into(), serde_json::Value::Array(vec![entry.into()]));
    }
    serde_json::Value::Object(hooks)
}

/// Does this hook entry (one element of an event's array) run recall-echo's
/// `sub` command? Matched loosely on purpose: any binary path, any root, with
/// or without a wrapper — it is *ours* to replace either way.
fn is_recall_entry(entry: &serde_json::Value, sub: &str) -> bool {
    entry["hooks"]
        .as_array()
        .map(|hooks| {
            hooks.iter().any(|h| {
                h["command"]
                    .as_str()
                    .is_some_and(|c| is_recall_command(c, sub))
            })
        })
        .unwrap_or(false)
}

fn is_recall_command(command: &str, sub: &str) -> bool {
    command
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| {
            Path::new(w[0])
                .file_name()
                .is_some_and(|f| f == "recall-echo")
                && w[1] == sub
        })
}

/// Merge our hooks into an existing settings document: every other key and
/// every foreign hook survives; each recall-echo hook appears exactly once,
/// in its canonical form.
fn merge_hooks(mut existing: serde_json::Value, ours: &serde_json::Value) -> serde_json::Value {
    if !existing.is_object() {
        existing = serde_json::json!({});
    }
    let root = existing.as_object_mut().expect("object");
    let hooks_value = root.entry("hooks").or_insert_with(|| serde_json::json!({}));
    if !hooks_value.is_object() {
        *hooks_value = serde_json::json!({});
    }
    let hooks = hooks_value.as_object_mut().expect("object");

    for (event, sub) in RECALL_HOOKS {
        let mut entries: Vec<serde_json::Value> = hooks
            .get(event)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| !is_recall_entry(entry, sub))
            .collect();
        if let Some(canonical) = ours[event].as_array().and_then(|a| a.first()) {
            entries.push(canonical.clone());
        }
        hooks.insert(event.to_string(), serde_json::Value::Array(entries));
    }
    existing
}

/// Bring `.claude/settings.json` up to date without clobbering anything else
/// in it.
fn ensure_settings(path: &Path, ours: &serde_json::Value) -> BootstrapItem {
    let item = |status| BootstrapItem {
        path: path.to_path_buf(),
        kind: ItemKind::ConfigFile,
        status,
    };
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => Some(v),
            Err(e) => return item(ItemStatus::Skipped(format!("not valid JSON: {e}"))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return item(ItemStatus::Skipped(e.to_string())),
    };
    let was_present = existing.is_some();
    let merged = merge_hooks(existing.clone().unwrap_or(serde_json::json!({})), ours);
    if existing.as_ref() == Some(&merged) {
        return item(ItemStatus::Exists);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let rendered = serde_json::to_string_pretty(&merged).unwrap_or_default() + "\n";
    match std::fs::write(path, rendered) {
        Ok(()) if was_present => item(ItemStatus::Updated),
        Ok(()) => item(ItemStatus::Created),
        Err(e) => item(ItemStatus::Skipped(e.to_string())),
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

/// Write a config file if it doesn't exist. Never overwrites: an existing
/// file may carry hand-tuned settings (Echo's graph config lives in its
/// `.recall-echo.toml`).
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

/// Generate the recall-echo.md rules file. Every path is relative to the
/// entity root, which is the cwd Claude Code runs in for this entity.
fn render_rules_md() -> String {
    r#"# recall-echo — Memory Protocol

You have a persistent four-layer memory system. Use it to maintain continuity across sessions.
All paths below are relative to your entity directory, which is the working directory you run in.

## Memory Layers

### Layer 0 — Knowledge Graph (structured, semantic)
- Embedded SurrealDB graph database with FastEmbed local embeddings.
- Stores entities, relationships, and conversation episodes.
- Bayesian confidence scoring on relationships — corroborated knowledge gains confidence over time.
- Semantic search finds memories by meaning, not just keywords.
- Queried via `recall-echo graph search`, `graph query`, or `graph traverse`.

### Layer 1 — Curated Memory (memory/MEMORY.md)
- Your source of truth. Distilled facts, preferences, patterns, key decisions.
- Loaded into your system prompt at startup.
- Keep under 200 lines. Only write confirmed, stable information.
- Before adding, check if an existing entry should be updated. No duplicates.

### Layer 2 — Recent Sessions (memory/EPHEMERAL.md)
- Rolling window of your last 5 session summaries, loaded into your system prompt.
- Read at session start to orient on recent work.
- Each entry has a pointer to the full archive.
- Managed automatically by recall-echo hooks. Do not edit manually.

### Layer 3 — Full Archive (archives/conversations/)
- Index: `memory/ARCHIVE.md`
- Full conversations: `archives/conversations/conversation-NNN.md`
- NOT loaded into context. Search on demand using Grep.
- To search: `Grep pattern="search term" path="archives/conversations/"`

## Session Lifecycle

### On session start:
1. MEMORY.md and EPHEMERAL.md are in your context.
2. Orient from recent sessions. Use archive pointers if you need full context.

### During the session:
- Update memory/MEMORY.md when you learn stable facts.
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

## Commands

- `recall-echo init` — Initialize or upgrade the memory system
- `recall-echo consume` — Output EPHEMERAL.md at session start (SessionStart hook)
- `recall-echo checkpoint --trigger precompact` — Save checkpoint before context compression
- `recall-echo archive-session` — Archive conversation from JSONL transcript (SessionEnd hook)
- `recall-echo search <query>` — Search conversation archives
- `recall-echo search <query> --ranked` — Ranked search with relevance scoring
- `recall-echo graph search <query>` — Semantic search across graph entities
- `recall-echo graph query <query>` — Hybrid search (semantic + graph expansion + episodes)
- `recall-echo graph traverse <entity>` — Graph traversal with confidence display

## Rules

- Never write duplicates to MEMORY.md. Check first, update if exists.
- When MEMORY.md approaches 200 lines, distill it.
- Archive conversations are immutable. Never modify them.
- When the user says "we discussed this before" — search archives before saying you don't remember.
"#
    .to_string()
}

/// Create all Claude Code integration files for one entity.
/// Safe to run multiple times — skips anything already correct.
pub fn ensure(entity_root: &Path) -> Vec<BootstrapItem> {
    let entity_root = entity_root
        .canonicalize()
        .unwrap_or_else(|_| entity_root.to_path_buf());
    let claude_dir = entity_root.join(".claude");
    let recall_bin = find_recall_echo_bin();

    vec![
        ensure_dir(&claude_dir),
        ensure_dir(&claude_dir.join("rules")),
        ensure_dir(&entity_root.join("memory")),
        ensure_settings(
            &claude_dir.join("settings.json"),
            &render_hooks(&recall_bin, &entity_root),
        ),
        ensure_config(&claude_dir.join("rules/recall-echo.md"), &render_rules_md()),
        ensure_config(
            &entity_root.join("memory/.recall-echo.toml"),
            &render_recall_echo_toml(&entity_root),
        ),
        ensure_conversations_link(&entity_root),
    ]
}

/// Verify Claude Code integration without creating anything. Also reports
/// leftovers of the pre-PN-104 user-level layout that point at this entity.
pub fn verify(entity_root: &Path) -> Vec<BootstrapItem> {
    let entity_root = entity_root
        .canonicalize()
        .unwrap_or_else(|_| entity_root.to_path_buf());
    let claude_dir = entity_root.join(".claude");
    let recall_bin = find_recall_echo_bin();
    let mut items = Vec::new();

    // settings.json: present and carrying all three canonical hooks.
    let settings = claude_dir.join("settings.json");
    let status = match std::fs::read_to_string(&settings) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(existing) => {
                let ours = render_hooks(&recall_bin, &entity_root);
                if merge_hooks(existing.clone(), &ours) == existing {
                    ItemStatus::Exists
                } else {
                    ItemStatus::Wrong("recall-echo hooks missing or stale".into())
                }
            }
            Err(e) => ItemStatus::Wrong(format!("not valid JSON: {e}")),
        },
        Err(_) => ItemStatus::Missing,
    };
    items.push(BootstrapItem {
        path: settings,
        kind: ItemKind::ConfigFile,
        status,
    });

    for path in [
        claude_dir.join("rules/recall-echo.md"),
        entity_root.join("memory/.recall-echo.toml"),
    ] {
        let status = if path.exists() {
            ItemStatus::Exists
        } else {
            ItemStatus::Missing
        };
        items.push(BootstrapItem {
            path,
            kind: ItemKind::ConfigFile,
            status,
        });
    }

    let link = entity_root.join("memory/conversations");
    let status = if link.is_symlink() {
        ItemStatus::Exists
    } else {
        ItemStatus::Missing
    };
    items.push(BootstrapItem {
        path: link,
        kind: ItemKind::Symlink,
        status,
    });

    if recall_bin == "recall-echo" {
        items.push(BootstrapItem {
            path: PathBuf::from("recall-echo"),
            kind: ItemKind::ConfigFile,
            status: ItemStatus::Missing,
        });
    }

    if let Some(home) = home_dir() {
        for link in legacy_home_links(&entity_root, &home) {
            items.push(BootstrapItem {
                path: link,
                kind: ItemKind::Symlink,
                status: ItemStatus::Wrong(
                    "legacy user-level link into this entity — run 'pulse-null repair'".into(),
                ),
            });
        }
    }

    items
}

/// Symlinks in `$HOME/.claude` left by the pre-PN-104 bootstrap that resolve
/// into `entity_root`. Links into *other* entities are not ours to touch.
pub fn legacy_home_links(entity_root: &Path, home: &Path) -> Vec<PathBuf> {
    let root = entity_root
        .canonicalize()
        .unwrap_or_else(|_| entity_root.to_path_buf());
    ["ARCHIVE.md", "EPHEMERAL.md", "memories"]
        .iter()
        .map(|name| home.join(".claude").join(name))
        .filter(|link| link.is_symlink())
        .filter(|link| {
            let Ok(target) = std::fs::read_link(link) else {
                return false;
            };
            let target = if target.is_absolute() {
                target
            } else {
                link.parent().unwrap_or(Path::new("/")).join(target)
            };
            let target = target.canonicalize().unwrap_or(target);
            target.starts_with(&root)
        })
        .collect()
}

/// recall-echo hook commands in the *user-level* `$HOME/.claude/settings.json`
/// that carry no entity root. Under one entity per user they worked by
/// accident of cwd; with several they fire for every entity and resolve to
/// the wrong one. Reported for the operator to remove — never edited here.
pub fn user_hooks_missing_root(home: &Path) -> Vec<String> {
    let path = home.join(".claude/settings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    if let Some(events) = doc["hooks"].as_object() {
        for entries in events.values() {
            for entry in entries.as_array().into_iter().flatten() {
                for hook in entry["hooks"].as_array().into_iter().flatten() {
                    let Some(command) = hook["command"].as_str() else {
                        continue;
                    };
                    let ours = RECALL_HOOKS
                        .iter()
                        .any(|(_, sub)| is_recall_command(command, sub));
                    if !ours {
                        continue;
                    }
                    let carries_root = command.contains("--entity-root")
                        || (is_recall_command(command, "consume")
                            && command
                                .split_whitespace()
                                .skip_while(|w| *w != "consume")
                                .nth(1)
                                .is_some());
                    if !carries_root {
                        found.push(command.to_string());
                    }
                }
            }
        }
    }
    found
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

    fn read_settings(root: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(root.join(".claude/settings.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn commands(doc: &serde_json::Value, event: &str) -> Vec<String> {
        doc["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|e| e["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|h| h["command"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn ensure_fresh_entity_writes_all_items() {
        let dir = entity();
        let root = dir.path().canonicalize().unwrap();
        let items = ensure(&root);
        assert!(
            items
                .iter()
                .all(|i| matches!(i.status, ItemStatus::Created | ItemStatus::Exists)),
            "{items:?}"
        );

        let doc = read_settings(&root);
        let quoted = shell_quote(&root);
        assert_eq!(commands(&doc, "SessionStart").len(), 1);
        assert!(commands(&doc, "SessionStart")[0].ends_with(&format!("consume {quoted}")));
        assert!(commands(&doc, "PreCompact")[0].ends_with(&format!(
            "checkpoint --trigger precompact --entity-root {quoted}"
        )));
        assert!(commands(&doc, "SessionEnd")[0]
            .ends_with(&format!("archive-session --entity-root {quoted}")));
        assert_eq!(doc["hooks"]["SessionStart"][0]["matcher"], "startup|resume");

        assert!(root.join(".claude/rules/recall-echo.md").exists());
        assert!(root.join("memory/.recall-echo.toml").exists());
        assert_eq!(
            std::fs::read_link(root.join("memory/conversations")).unwrap(),
            PathBuf::from("../archives/conversations")
        );
    }

    #[test]
    fn ensure_is_idempotent() {
        let dir = entity();
        ensure(dir.path());
        let before = std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        let again = ensure(dir.path());
        assert!(
            again.iter().all(|i| i.status == ItemStatus::Exists),
            "{again:?}"
        );
        let after = std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn merge_keeps_foreign_hooks_and_dedupes_ours() {
        let dir = entity();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            root.join(".claude/settings.json"),
            serde_json::json!({
                "permissions": {"allow": ["Bash(ls:*)"]},
                "hooks": {
                    "SessionEnd": [
                        {"hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session || true"}]},
                        {"hooks": [{"type": "command", "command": "echo bye"}]}
                    ],
                    "PreCompact": [
                        {"hooks": [{"type": "command", "command": "recall-echo checkpoint --trigger precompact"}]}
                    ],
                    "Stop": [{"hooks": [{"type": "command", "command": "echo stop"}]}]
                }
            })
            .to_string(),
        )
        .unwrap();

        let items = ensure(&root);
        let settings = items
            .iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert_eq!(settings.status, ItemStatus::Updated);

        let doc = read_settings(&root);
        assert_eq!(doc["permissions"]["allow"][0], "Bash(ls:*)");
        assert_eq!(commands(&doc, "Stop"), vec!["echo stop"]);
        let end = commands(&doc, "SessionEnd");
        assert_eq!(end.len(), 2, "{end:?}");
        assert_eq!(end[0], "echo bye");
        assert!(end[1].contains("--entity-root"));
        assert!(!end[1].contains("|| true"));
        let pre = commands(&doc, "PreCompact");
        assert_eq!(pre.len(), 1);
        assert!(pre[0].contains("--entity-root"));
        assert_eq!(commands(&doc, "SessionStart").len(), 1);
    }

    #[test]
    fn rules_md_has_no_home_paths() {
        let rules = render_rules_md();
        assert!(!rules.contains("~/.claude"));
        assert!(!rules.contains("@~"));
        assert!(rules.contains("memory/MEMORY.md"));
        assert!(rules.contains("archives/conversations/"));
    }

    #[test]
    fn existing_recall_toml_untouched() {
        let dir = entity();
        let toml = dir.path().join("memory/.recall-echo.toml");
        std::fs::write(&toml, "[graph]\nmode = \"server\"\n").unwrap();
        ensure(dir.path());
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
        let items = ensure(&root);
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
        let items = ensure(&root);
        let link = items
            .iter()
            .find(|i| i.path.ends_with("memory/conversations"))
            .unwrap();
        assert!(matches!(link.status, ItemStatus::Skipped(_)));
    }

    #[test]
    fn legacy_links_only_match_this_entity() {
        let dir = entity();
        let root = dir.path().canonicalize().unwrap();
        let other = entity();
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::os::unix::fs::symlink(root.join("memory/ARCHIVE.md"), claude.join("ARCHIVE.md"))
            .unwrap();
        std::os::unix::fs::symlink(root.join("memory"), claude.join("memories")).unwrap();
        std::os::unix::fs::symlink(
            other.path().join("memory/EPHEMERAL.md"),
            claude.join("EPHEMERAL.md"),
        )
        .unwrap();

        let mut found = legacy_home_links(&root, home.path());
        found.sort();
        assert_eq!(
            found,
            vec![claude.join("ARCHIVE.md"), claude.join("memories")]
        );
        assert!(legacy_home_links(other.path(), home.path())
            .iter()
            .all(|p| p.ends_with("EPHEMERAL.md")));
    }

    #[test]
    fn user_hooks_without_root_are_reported_not_edited() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let original = serde_json::json!({
            "hooks": {
                "PreCompact": [{"hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo checkpoint --trigger precompact"}]}],
                "SessionEnd": [{"hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session || true"}]}],
                "SessionStart": [{"hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo consume '/srv/e'"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "recall-echo checkpoint --trigger precompact --entity-root '/srv/e'"}]}]
            }
        })
        .to_string();
        std::fs::write(claude.join("settings.json"), &original).unwrap();

        let mut found = user_hooks_missing_root(home.path());
        found.sort();
        assert_eq!(
            found,
            vec![
                "/usr/local/bin/recall-echo archive-session || true".to_string(),
                "/usr/local/bin/recall-echo checkpoint --trigger precompact".to_string(),
            ]
        );
        assert_eq!(
            std::fs::read_to_string(claude.join("settings.json")).unwrap(),
            original
        );
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote(Path::new("/a/b c")), "'/a/b c'");
        assert_eq!(shell_quote(Path::new("/a/it's")), "'/a/it'\\''s'");
    }
}
