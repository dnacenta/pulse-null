//! Claude Code's entity-local files: `.claude/settings.json` (recall-echo
//! hooks that carry the entity root explicitly), `.claude/rules/recall-echo.md`
//! (the memory protocol, entity-relative), and the leftovers of the
//! pre-PN-104 user-level layout that `repair` retires. Claude Code reads
//! these as project-scope configuration from its working directory —
//! verified 2026-09-08 with a planted rule and a bare-directory control.

use std::path::{Path, PathBuf};

use crate::init::agent_bootstrap::{
    ensure_config, ensure_dir, find_recall_echo_bin, read_small_file, shell_quote,
    write_regular_file, BootstrapItem, ItemKind, ItemStatus,
};

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The three recall-echo hook subcommands, in the event order Claude Code
/// fires them. `consume` takes the root positionally; the other two take
/// `--entity-root` — matching what `recall-echo init` itself writes.
const RECALL_HOOKS: [(&str, &str); 3] = [
    ("SessionStart", "consume"),
    ("PreCompact", "checkpoint"),
    ("SessionEnd", "archive-session"),
];

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
pub(super) fn ensure_files(entity_root: &Path, recall_bin: &str) -> Vec<BootstrapItem> {
    let entity_root = entity_root
        .canonicalize()
        .unwrap_or_else(|_| entity_root.to_path_buf());
    let claude_dir = entity_root.join(".claude");
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
            &render_hooks(recall_bin, &entity_root),
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
    items
}

/// Verify Claude Code integration without creating anything. Also reports
/// leftovers of the pre-PN-104 user-level layout that point at this entity.
pub(super) fn verify_files(entity_root: &Path) -> Vec<BootstrapItem> {
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

    let rules = claude_dir.join("rules/recall-echo.md");
    items.push(BootstrapItem {
        status: if rules.exists() {
            ItemStatus::Exists
        } else {
            ItemStatus::Missing
        },
        path: rules,
        kind: ItemKind::ConfigFile,
    });

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
pub(super) fn legacy_home_links(entity_root: &Path, home: &Path) -> Vec<PathBuf> {
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
pub(super) fn user_hooks_missing_root(home: &Path) -> Vec<String> {
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
    use crate::init::agent_bootstrap::{ensure_common, verify_common, write_regular_file};

    fn ensure_all(root: &Path) -> Vec<BootstrapItem> {
        let mut items = ensure_common(root, "claude-code");
        items.extend(ensure_files(root, &find_recall_echo_bin()));
        items
    }

    fn verify_all(root: &Path) -> Vec<BootstrapItem> {
        let mut items = verify_common(root);
        items.extend(verify_files(root));
        items
    }

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
        let items = ensure_all(&root);
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
        ensure_all(dir.path());
        let before = std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        let again = ensure_all(dir.path());
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

        let items = ensure_all(&root);
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

        let items = ensure_all(&root);
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
        let items = ensure_all(&root);
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
        ensure_all(&root);
        // Move a foreign hook *after* ours in SessionEnd.
        let path = root.join(".claude/settings.json");
        let mut doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        doc["hooks"]["SessionEnd"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"hooks": [{"type": "command", "command": "echo bye"}]}));
        std::fs::write(&path, doc.to_string()).unwrap();
        let settings = verify_all(&root)
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
}
