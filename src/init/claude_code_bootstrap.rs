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
fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

/// The canonical hook command for one subcommand.
fn hook_command(recall_bin: &str, sub: &str, root: &Path) -> String {
    let bin = shell_quote(Path::new(recall_bin));
    let root = shell_quote(root);
    match sub {
        "consume" => format!("{bin} consume {root}"),
        "checkpoint" => format!("{bin} checkpoint --trigger precompact --entity-root {root}"),
        _ => format!("{bin} {sub} --entity-root {root}"),
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

/// The first two shell words of a hook command, honouring single quotes
/// (the only quoting `hook_command` emits). Enough to recognise our own
/// output whatever the binary path contains; not a general shell parser.
fn first_two_words(command: &str) -> Option<(String, String)> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => in_quote = !in_quote,
            // Outside quotes a backslash escapes the next character — this is
            // how `shell_quote` spells a literal quote (`'\''`).
            '\\' if !in_quote => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    if words.len() == 2 {
                        break;
                    }
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() && words.len() < 2 {
        words.push(cur);
    }
    let mut it = words.into_iter();
    Some((it.next()?, it.next()?))
}

/// Is this command a recall-echo `sub` invocation? Anchored: the *first*
/// word must be the recall-echo binary (quoted or not, any path) and the
/// second the subcommand. A command that merely mentions recall-echo
/// somewhere is not ours to touch.
fn is_recall_command(command: &str, sub: &str) -> bool {
    first_two_words(command).is_some_and(|(bin, first_arg)| {
        Path::new(&bin)
            .file_name()
            .is_some_and(|f| f == "recall-echo")
            && first_arg == sub
    })
}

/// Drop recall-echo `sub` hooks from one event entry, keeping every other
/// hook in it. `None` when nothing is left worth keeping.
fn strip_recall_hooks(mut entry: serde_json::Value, sub: &str) -> Option<serde_json::Value> {
    let Some(hooks) = entry["hooks"].as_array() else {
        return Some(entry);
    };
    let kept: Vec<serde_json::Value> = hooks
        .iter()
        .filter(|h| {
            !h["command"]
                .as_str()
                .is_some_and(|c| is_recall_command(c, sub))
        })
        .cloned()
        .collect();
    if kept.is_empty() {
        return None;
    }
    entry["hooks"] = serde_json::Value::Array(kept);
    Some(entry)
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
            .filter_map(|entry| strip_recall_hooks(entry, sub))
            .collect();
        if let Some(canonical) = ours[event].as_array().and_then(|a| a.first()) {
            entries.push(canonical.clone());
        }
        hooks.insert(event.to_string(), serde_json::Value::Array(entries));
    }
    existing
}

/// Largest config file this module will read. These are hand-sized JSON
/// documents; anything bigger is not one of ours.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Read a small regular file. `Ok(None)` when absent; an error names why a
/// present path was refused (symlink, not a regular file, too large). The
/// file is opened once with `O_NOFOLLOW` and inspected through the handle,
/// so nothing can be swapped in between the check and the read.
fn read_small_file(path: &Path) -> Result<Option<String>, String> {
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
fn write_regular_file(path: &Path, content: &str, within: &Path) -> Result<(), String> {
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

/// Does `existing` already carry our hooks — each event with exactly one
/// recall-echo hook for its subcommand, in canonical form? Order among an
/// event's entries does not matter; only `merge_hooks` normalises it.
fn has_canonical_hooks(existing: &serde_json::Value, ours: &serde_json::Value) -> bool {
    RECALL_HOOKS.iter().all(|(event, sub)| {
        let Some(canonical) = ours[event][0]["hooks"][0]["command"].as_str() else {
            return false;
        };
        let commands: Vec<&str> = existing["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|entry| entry["hooks"].as_array().into_iter().flatten())
            .filter_map(|h| h["command"].as_str())
            .filter(|c| is_recall_command(c, sub))
            .collect();
        commands == [canonical]
    })
}

/// Bring `.claude/settings.json` up to date without clobbering anything else
/// in it.
fn ensure_settings(path: &Path, ours: &serde_json::Value, within: &Path) -> BootstrapItem {
    let item = |status| BootstrapItem {
        path: path.to_path_buf(),
        kind: ItemKind::ConfigFile,
        status,
    };
    let existing = match read_small_file(path) {
        Ok(Some(text)) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => Some(v),
            Err(e) => return item(ItemStatus::Skipped(format!("not valid JSON: {e}"))),
        },
        Ok(None) => None,
        Err(e) => return item(ItemStatus::Skipped(e)),
    };
    let was_present = existing.is_some();
    let merged = merge_hooks(existing.clone().unwrap_or(serde_json::json!({})), ours);
    if existing.as_ref() == Some(&merged) {
        return item(ItemStatus::Exists);
    }
    let rendered = match serde_json::to_string_pretty(&merged) {
        Ok(text) => text + "\n",
        Err(e) => return item(ItemStatus::Skipped(format!("could not serialise: {e}"))),
    };
    match write_regular_file(path, &rendered, within) {
        Ok(()) if was_present => item(ItemStatus::Updated),
        Ok(()) => item(ItemStatus::Created),
        Err(e) => item(ItemStatus::Skipped(e)),
    }
}

/// Create a directory if it doesn't exist. A symlink where the directory
/// should be is refused, not followed.
fn ensure_dir(path: &Path) -> BootstrapItem {
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
fn ensure_config(path: &Path, content: &str, within: &Path) -> BootstrapItem {
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

/// Generate the recall-echo.toml config content.
fn render_recall_echo_toml(entity_root: &Path) -> String {
    // A TOML string literal, escaped by the toml crate — a quote or newline
    // in the path must not be able to open a new table.
    let docs_dir = toml::Value::String(format!("{}/journal", entity_root.display())).to_string();
    format!(
        r#"[ephemeral]
max_entries = 5

[llm]
provider = "claude-code"
model = ""
api_base = ""

[pipeline]
docs_dir = {docs_dir}
auto_sync = true
"#
    )
}

/// Generate the recall-echo.md rules file. Every path is relative to the
/// entity root, which is the cwd Claude Code runs in for this entity.
fn render_rules_md() -> &'static str {
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
}

/// Create all Claude Code integration files for one entity.
/// Safe to run multiple times — skips anything already correct. A parent
/// that is not a real directory (a symlink, say) stops everything under it:
/// nothing is created through a component we did not verify.
pub fn ensure(entity_root: &Path) -> Vec<BootstrapItem> {
    let entity_root = entity_root
        .canonicalize()
        .unwrap_or_else(|_| entity_root.to_path_buf());
    if entity_root.to_str().is_none() {
        // A lossy conversion would persist a hook pointing at a path that
        // does not exist. Say so instead.
        return vec![BootstrapItem {
            path: entity_root,
            kind: ItemKind::Directory,
            status: ItemStatus::Skipped("entity root is not valid UTF-8".into()),
        }];
    }
    let claude_dir = entity_root.join(".claude");
    let memory_dir = entity_root.join("memory");
    let recall_bin = find_recall_echo_bin();
    let ok = |item: &BootstrapItem| matches!(item.status, ItemStatus::Created | ItemStatus::Exists);

    let mut items = Vec::new();
    let claude = ensure_dir(&claude_dir);
    let claude_ok = ok(&claude);
    items.push(claude);
    if claude_ok {
        let rules = ensure_dir(&claude_dir.join("rules"));
        let rules_ok = ok(&rules);
        items.push(rules);
        items.push(ensure_settings(
            &claude_dir.join("settings.json"),
            &render_hooks(&recall_bin, &entity_root),
            &entity_root,
        ));
        if rules_ok {
            items.push(ensure_config(
                &claude_dir.join("rules/recall-echo.md"),
                render_rules_md(),
                &entity_root,
            ));
        }
    }
    let memory = ensure_dir(&memory_dir);
    let memory_ok = ok(&memory);
    items.push(memory);
    if memory_ok {
        items.push(ensure_config(
            &memory_dir.join(".recall-echo.toml"),
            &render_recall_echo_toml(&entity_root),
            &entity_root,
        ));
        items.push(ensure_conversations_link(&entity_root));
    }
    items
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
    let status = match read_small_file(&settings) {
        Ok(Some(text)) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(existing) => {
                let ours = render_hooks(&recall_bin, &entity_root);
                if has_canonical_hooks(&existing, &ours) {
                    ItemStatus::Exists
                } else {
                    ItemStatus::Wrong("recall-echo hooks missing or stale".into())
                }
            }
            Err(e) => ItemStatus::Wrong(format!("not valid JSON: {e}")),
        },
        Ok(None) => ItemStatus::Missing,
        Err(e) => ItemStatus::Wrong(e),
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
            // A dangling link cannot be shown to resolve inside the entity
            // (`..` is not normalised lexically), so it is not ours to remove.
            let Ok(target) = target.canonicalize() else {
                return false;
            };
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
    let Ok(Some(text)) = read_small_file(&path) else {
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
                                .is_some_and(|arg| !arg.starts_with(['|', '&', ';', '>', '<'])));
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
        std::fs::write(root.join("memory/ARCHIVE.md"), "").unwrap();
        std::fs::write(other.path().join("memory/EPHEMERAL.md"), "").unwrap();
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
        let for_other = legacy_home_links(other.path(), home.path());
        assert_eq!(for_other.len(), 1);
        assert!(for_other[0].ends_with("EPHEMERAL.md"));

        // A dangling link cannot be shown to point inside the entity — even
        // one whose lexical target escapes through `..` — so it is left alone.
        std::fs::remove_file(claude.join("ARCHIVE.md")).unwrap();
        std::os::unix::fs::symlink(root.join("../../etc/passwd"), claude.join("ARCHIVE.md"))
            .unwrap();
        std::fs::remove_file(root.join("memory/ARCHIVE.md")).unwrap();
        assert_eq!(
            legacy_home_links(&root, home.path()),
            vec![claude.join("memories")]
        );
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
    fn recall_match_is_anchored_and_hook_level() {
        assert!(is_recall_command(
            "'/usr/local/bin/recall-echo' consume '/e'",
            "consume"
        ));
        assert!(is_recall_command(
            "recall-echo archive-session || true",
            "archive-session"
        ));
        assert!(!is_recall_command("echo recall-echo consume", "consume"));
        assert!(!is_recall_command("recall-echo consume", "checkpoint"));

        let entry = serde_json::json!({"hooks": [
            {"type": "command", "command": "recall-echo consume '/e'"},
            {"type": "command", "command": "echo sibling"}
        ]});
        let kept = strip_recall_hooks(entry, "consume").expect("sibling survives");
        assert_eq!(kept["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(kept["hooks"][0]["command"], "echo sibling");
        let only_ours =
            serde_json::json!({"hooks": [{"type": "command", "command": "recall-echo consume"}]});
        assert!(strip_recall_hooks(only_ours, "consume").is_none());
    }

    #[test]
    fn bootstrap_refuses_to_write_through_symlinks() {
        let dir = entity();
        let root = dir.path().canonicalize().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.join(".claude/rules")).unwrap();
        // Dangling links where the config files should be.
        std::os::unix::fs::symlink(
            elsewhere.path().join("pwned.json"),
            root.join(".claude/settings.json"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("pwned.md"),
            root.join(".claude/rules/recall-echo.md"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("pwned.toml"),
            root.join("memory/.recall-echo.toml"),
        )
        .unwrap();

        let items = ensure(&root);
        for name in ["settings.json", "recall-echo.md", ".recall-echo.toml"] {
            let item = items.iter().find(|i| i.path.ends_with(name)).unwrap();
            assert!(
                matches!(item.status, ItemStatus::Skipped(_)),
                "{name}: {:?}",
                item.status
            );
        }
        assert!(
            std::fs::read_dir(elsewhere.path())
                .unwrap()
                .next()
                .is_none(),
            "nothing written through the links"
        );
    }

    #[test]
    fn recall_toml_escapes_the_path() {
        let toml_text =
            render_recall_echo_toml(Path::new("/srv/evil\"\n[llm]\napi_base = \"http://x"));
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
    fn hook_commands_quote_the_binary() {
        let cmd = hook_command("/opt/my tools/recall-echo", "consume", Path::new("/e"));
        assert_eq!(cmd, "'/opt/my tools/recall-echo' consume '/e'");
        // …and we still recognise our own output.
        assert!(is_recall_command(&cmd, "consume"));
        let odd = hook_command(
            "/opt/it's here/recall-echo",
            "archive-session",
            Path::new("/e"),
        );
        assert!(is_recall_command(&odd, "archive-session"));
        assert!(!is_recall_command(
            "sh -c 'recall-echo consume /e'",
            "consume"
        ));
    }

    #[test]
    fn writes_never_follow_a_symlinked_parent_or_temp() {
        let dir = entity();
        let root = dir.path().canonicalize().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // `.claude` itself is a link out of the tree.
        std::os::unix::fs::symlink(elsewhere.path(), root.join(".claude")).unwrap();
        let items = ensure(&root);
        let claude = items.iter().find(|i| i.path.ends_with(".claude")).unwrap();
        assert!(matches!(claude.status, ItemStatus::Skipped(_)));
        assert!(
            !items.iter().any(|i| i.path.ends_with("settings.json")),
            "nothing under a refused parent is attempted: {items:?}"
        );
        assert!(std::fs::read_dir(elsewhere.path())
            .unwrap()
            .next()
            .is_none());

        // A direct write whose parent resolves outside the entity is refused.
        let err = write_regular_file(&elsewhere.path().join("x.json"), "{}", &root).unwrap_err();
        assert!(err.contains("outside the entity"), "{err}");

        // Temp files are O_EXCL: a planted name cannot be written through.
        std::fs::remove_file(root.join(".claude")).unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        write_regular_file(&root.join(".claude/settings.json"), "{}\n", &root).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(root.join(".claude"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left behind");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.join(".claude/settings.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn verify_accepts_our_hook_in_any_position() {
        let dir = entity();
        let root = dir.path().canonicalize().unwrap();
        ensure(&root);
        // Move a foreign hook *after* ours in SessionEnd.
        let path = root.join(".claude/settings.json");
        let mut doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        doc["hooks"]["SessionEnd"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"hooks": [{"type": "command", "command": "echo bye"}]}));
        std::fs::write(&path, doc.to_string()).unwrap();
        let settings = verify(&root)
            .into_iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert_eq!(settings.status, ItemStatus::Exists, "{settings:?}");
    }

    #[test]
    fn consume_followed_by_an_operator_carries_no_root() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            serde_json::json!({"hooks": {"SessionStart": [{"hooks": [
                {"type": "command", "command": "recall-echo consume || true"}
            ]}]}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            user_hooks_missing_root(home.path()),
            vec!["recall-echo consume || true".to_string()]
        );
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
