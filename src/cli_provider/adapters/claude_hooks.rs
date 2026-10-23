//! Claude Code's pulse-local files: `.claude/settings.json` (recall-echo
//! hooks that carry the pulse root explicitly), `.claude/rules/recall-echo.md`
//! (the memory protocol, pulse-relative), and the leftovers of the
//! pre-PN-104 user-level layout that `repair` retires. Claude Code reads
//! these as project-scope configuration from its working directory —
//! verified 2026-09-08 with a planted rule and a bare-directory control.

use std::path::{Path, PathBuf};

use crate::cli_provider::adapter::ReportedUserHook;
use crate::init::agent_bootstrap::{
    ensure_config, ensure_dir, find_recall_echo_bin, read_small_file, shell_quote,
    write_regular_file, BootstrapItem, ItemKind, ItemStatus,
};

/// The rules file this module writes into `.claude/rules/`. It describes
/// Claude Code's hooks, so CLIs that inline the rules directory skip it.
pub(super) const RECALL_ECHO_RULES_FILE: &str = "recall-echo.md";

/// First line of the rules file this module generates. A file that still
/// starts with it is ours to refresh; anything else was edited by hand.
const RULES_HEADER: &str = "# recall-echo — Memory Protocol";

#[cfg(test)]
thread_local! {
    static HOME_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Some(path) = HOME_OVERRIDE.with(|slot| slot.borrow().clone()) {
            return Some(path);
        }
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
fn set_home_override(path: Option<PathBuf>) {
    HOME_OVERRIDE.with(|slot| *slot.borrow_mut() = path);
}

/// The recall-echo hooks a pulse installs, in the event order Claude Code
/// fires them. Both take `--pulse-root` — matching what `recall-echo init`
/// itself writes. Hooks written before PN-115 spell it `--entity-root`
/// (recall-echo ≥ 4.6.0 reads both); `ensure` rewrites them to the canonical
/// form like any other stale recall-echo hook.
///
/// SessionStart `consume` is not one of them. The runtime loads
/// EPHEMERAL.md into the chat prompt, so a SessionStart hook delivered it a
/// second time, and into task and intent runs that leave it out on purpose.
const RECALL_HOOKS: [(&str, &str); 2] = [
    ("PreCompact", "checkpoint"),
    ("SessionEnd", "archive-session"),
];

/// Hooks earlier pulses installed and that repair now removes. `merge_hooks`
/// only rewrites the subcommands in [`RECALL_HOOKS`], so without this list a
/// retired `consume` hook would stay in settings.json.
const RETIRED_RECALL_HOOKS: [(&str, &str); 1] = [("SessionStart", "consume")];

/// The canonical hook command for one subcommand.
fn hook_command(recall_bin: &str, sub: &str, root: &Path) -> String {
    let bin = shell_quote(Path::new(recall_bin));
    let root = shell_quote(root);
    match sub {
        "consume" => format!("{bin} consume {root}"),
        "checkpoint" => format!("{bin} checkpoint --trigger precompact --pulse-root {root}"),
        _ => format!("{bin} {sub} --pulse-root {root}"),
    }
}

/// Our hooks, keyed by event, in Claude Code's settings.json shape.
fn render_hooks(recall_bin: &str, root: &Path) -> serde_json::Value {
    let mut hooks = serde_json::Map::new();
    for (event, sub) in RECALL_HOOKS {
        let entry = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": hook_command(recall_bin, sub, root)
            }]
        });
        hooks.insert(event.into(), serde_json::Value::Array(vec![entry]));
    }
    serde_json::Value::Object(hooks)
}

struct ShellWord {
    text: String,
    /// The word was single-quoted, so it is a literal even when its text
    /// looks like a redirect or an operator.
    quoted: bool,
}

/// Shell words of a hook command, honouring single quotes (the only quoting
/// `hook_command` emits). An unquoted `#` starts a comment and ends the
/// scan. Outside quotes a backslash escapes the next character — this is
/// how `shell_quote` spells a literal quote (`'\''`). Not a general shell
/// parser.
fn shell_words(command: &str) -> Vec<ShellWord> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut quoted = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_quote = !in_quote;
                quoted = true;
            }
            '\\' if !in_quote => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() || quoted {
                    words.push(ShellWord {
                        text: std::mem::take(&mut cur),
                        quoted,
                    });
                    quoted = false;
                }
            }
            '#' if !in_quote && cur.is_empty() && !quoted => break,
            c => cur.push(c),
        }
    }
    if !cur.is_empty() || quoted {
        words.push(ShellWord { text: cur, quoted });
    }
    words
}

/// The first two shell words of a hook command. Enough to recognise our own
/// output whatever the binary path contains.
fn first_two_words(command: &str) -> Option<(String, String)> {
    let words = shell_words(command);
    Some((words.first()?.text.clone(), words.get(1)?.text.clone()))
}

/// An unquoted word that is a shell operator or a redirect, not a path.
fn is_operator_or_redirect(word: &str) -> bool {
    matches!(
        word,
        "|" | "||" | "&" | "&&" | ";" | ";;" | "|&" | "(" | ")"
    ) || is_redirect(word)
}

/// `>`, `2>`, `2>&1`, `&>`, and the same forms with a target glued on.
fn is_redirect(word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    if word.starts_with("&>") {
        return true;
    }
    let rest = word.trim_start_matches(|c: char| c.is_ascii_digit());
    rest.starts_with('>') || rest.starts_with('<')
}

fn is_root_flag(word: &str) -> bool {
    word == "--pulse-root"
        || word == "--entity-root"
        || word.starts_with("--pulse-root=")
        || word.starts_with("--entity-root=")
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
/// in its canonical form. Retired hooks are stripped. An event left with no
/// hooks is removed, and a `hooks` object this function created that ends
/// up empty is removed too — a `hooks` key the user already had is kept.
fn merge_hooks(mut existing: serde_json::Value, ours: &serde_json::Value) -> serde_json::Value {
    if !existing.is_object() {
        existing = serde_json::json!({});
    }
    let root = existing.as_object_mut().expect("object");
    let created_hooks = !root.contains_key("hooks");
    let hooks_empty = {
        let hooks_value = root.entry("hooks").or_insert_with(|| serde_json::json!({}));
        if !hooks_value.is_object() {
            *hooks_value = serde_json::json!({});
        }
        let hooks = hooks_value.as_object_mut().expect("object");

        for (event, sub) in RETIRED_RECALL_HOOKS {
            let Some(raw) = hooks.get(event).and_then(|v| v.as_array()).cloned() else {
                continue;
            };
            let kept: Vec<serde_json::Value> = raw
                .into_iter()
                .filter_map(|entry| strip_recall_hooks(entry, sub))
                .collect();
            if kept.is_empty() {
                hooks.remove(event);
            } else {
                hooks.insert(event.to_string(), serde_json::Value::Array(kept));
            }
        }

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
        hooks.is_empty()
    };
    if created_hooks && hooks_empty {
        root.remove("hooks");
    }
    existing
}

/// Does `existing` already carry our hooks — each event with exactly one
/// recall-echo hook for its subcommand, in canonical form — and no retired
/// recall hook? Order among an event's entries does not matter; only
/// `merge_hooks` normalises it.
fn has_canonical_hooks(existing: &serde_json::Value, ours: &serde_json::Value) -> bool {
    let installed = RECALL_HOOKS.iter().all(|(event, sub)| {
        let Some(canonical) = ours[event][0]["hooks"][0]["command"].as_str() else {
            return false;
        };
        recall_commands(existing, event, sub) == [canonical]
    });
    let retired_absent = RETIRED_RECALL_HOOKS
        .iter()
        .all(|(event, sub)| recall_commands(existing, event, sub).is_empty());
    installed && retired_absent
}

/// recall-echo `sub` commands under one event.
fn recall_commands<'a>(existing: &'a serde_json::Value, event: &str, sub: &str) -> Vec<&'a str> {
    existing["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| entry["hooks"].as_array().into_iter().flatten())
        .filter_map(|h| h["command"].as_str())
        .filter(|c| is_recall_command(c, sub))
        .collect()
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
/// pulse root, which is the cwd Claude Code runs in for this pulse.
fn render_rules_md() -> &'static str {
    r#"# recall-echo — Memory Protocol

You have a persistent four-layer memory system. Use it to maintain continuity across sessions.
All paths below are relative to your pulse directory, which is the working directory you run in.

## Memory Layers

### Layer 0 — Knowledge Graph (structured, semantic)
- Embedded SurrealDB graph database with FastEmbed local embeddings.
- Stores pulses, relationships, and conversation episodes.
- Bayesian confidence scoring on relationships — corroborated knowledge gains confidence over time.
- Semantic search finds memories by meaning, not just keywords.
- Queried via `recall-echo graph search`, `graph query`, or `graph traverse`.

### Layer 1 — Curated Memory (memory/MEMORY.md)
- Your source of truth. Distilled facts, preferences, patterns, key decisions.
- Loaded into your system prompt at startup.
- Keep under 200 lines. Only write confirmed, stable information.
- Before adding, check if an existing entry should be updated. No duplicates.

### Layer 2 — Recent Sessions (memory/EPHEMERAL.md)
- Rolling window of your last 5 session summaries.
- Loaded into your chat system prompt by pulse-null; not in scheduled task runs; written by the runtime at session end; do not edit.
- Each entry has a pointer to the full archive.

### Layer 3 — Full Archive (archives/conversations/)
- Index: `memory/ARCHIVE.md`
- Full conversations: `archives/conversations/conversation-NNN.md`
- NOT loaded into context. Search on demand using Grep.
- To search: `Grep pattern="search term" path="archives/conversations/"`

## Session Lifecycle

### During the session:
- Update memory/MEMORY.md when you learn stable facts.
- When the user references past work, search the archive first.
- Do NOT update MEMORY.md with speculative or session-specific info.

### On PreCompact (context about to be compressed):
The PreCompact hook runs `recall-echo checkpoint --trigger precompact`, which writes the full conversation automatically. There is nothing to write by hand.

### On session end:
The SessionEnd hook runs `recall-echo archive-session` and archives this conversation automatically.

## Commands

- `recall-echo init` — Initialize or upgrade the memory system
- `recall-echo checkpoint --trigger precompact` — Write the conversation before context compression (PreCompact hook)
- `recall-echo archive-session` — Archive the conversation from its transcript (SessionEnd hook)
- `recall-echo search <query>` — Search conversation archives
- `recall-echo search <query> --ranked` — Ranked search with relevance scoring
- `recall-echo graph search <query>` — Semantic search across graph entities
- `recall-echo graph query <query>` — Hybrid search (semantic + graph expansion + episodes)
- `recall-echo graph traverse <pulse>` — Graph traversal with confidence display

## Rules

- Never write duplicates to MEMORY.md. Check first, update if exists.
- When MEMORY.md approaches 200 lines, distill it.
- Archive conversations are immutable. Never modify them.
- When the user says "we discussed this before" — search archives before saying you don't remember.
"#
}

/// The rules file this module wrote before the 4.x lifecycle rewrite.
/// Repair refreshes a file only when it is still this text, or the
/// current template. Anything else was edited by hand.
const OLD_GENERATED_RULES: &str = r#"# recall-echo — Memory Protocol

You have a persistent four-layer memory system. Use it to maintain continuity across sessions.
All paths below are relative to your pulse directory, which is the working directory you run in.

## Memory Layers

### Layer 0 — Knowledge Graph (structured, semantic)
- Embedded SurrealDB graph database with FastEmbed local embeddings.
- Stores pulses, relationships, and conversation episodes.
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
- `recall-echo graph search <query>` — Semantic search across graph entitys
- `recall-echo graph query <query>` — Hybrid search (semantic + graph expansion + episodes)
- `recall-echo graph traverse <pulse>` — Graph traversal with confidence display

## Rules

- Never write duplicates to MEMORY.md. Check first, update if exists.
- When MEMORY.md approaches 200 lines, distill it.
- Archive conversations are immutable. Never modify them.
- When the user says "we discussed this before" — search archives before saying you don't remember.
"#;

/// The first line equals `header` exactly. A `\n` or `\r\n` may follow it.
/// One trailing `\r` on that line is ignored; anything else on the line is
/// not the header.
fn first_line_is_header(text: &str, header: &str) -> bool {
    let mut line = text.split('\n').next().unwrap_or("");
    if let Some(stripped) = line.strip_suffix('\r') {
        line = stripped;
    }
    line == header
}

/// A generated file we may replace: one leading BOM stripped, the first
/// line exactly `header`, and the body (CRLF normalised) equal to the
/// current template or [`OLD_GENERATED_RULES`].
fn is_known_generated(text: &str, content: &str, header: &str) -> bool {
    let stripped = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    if !first_line_is_header(stripped, header) {
        return false;
    }
    let normalized = stripped.replace("\r\n", "\n");
    normalized == content || normalized == OLD_GENERATED_RULES
}

/// Write a generated file. Refresh it only when it is still the current
/// template or the known previous one, allowing one leading BOM and CRLF
/// line endings. Every other file is hand-edited: leave it byte-for-byte
/// and report why.
fn ensure_generated(path: &Path, content: &str, header: &str, within: &Path) -> BootstrapItem {
    let item = |status| BootstrapItem {
        path: path.to_path_buf(),
        kind: ItemKind::ConfigFile,
        status,
    };
    let existing = match read_small_file(path) {
        Ok(text) => text,
        Err(e) => return item(ItemStatus::Skipped(e)),
    };
    match existing {
        None => match write_regular_file(path, content, within) {
            Ok(()) => item(ItemStatus::Created),
            Err(e) => item(ItemStatus::Skipped(e)),
        },
        Some(text) if text == content => item(ItemStatus::Exists),
        Some(text) if is_known_generated(&text, content, header) => {
            match write_regular_file(path, content, within) {
                Ok(()) => item(ItemStatus::Updated),
                Err(e) => item(ItemStatus::Skipped(e)),
            }
        }
        Some(_) => item(ItemStatus::Skipped("hand-edited; left as is".into())),
    }
}

/// Create all Claude Code integration files for one pulse.
/// Safe to run multiple times — skips anything already correct. A parent
/// that is not a real directory (a symlink, say) stops everything under it:
/// nothing is created through a component we did not verify.
pub(super) fn ensure_files(pulse_root: &Path, recall_bin: &str) -> Vec<BootstrapItem> {
    let pulse_root = pulse_root
        .canonicalize()
        .unwrap_or_else(|_| pulse_root.to_path_buf());
    if pulse_root.to_str().is_none() {
        // The hook commands embed the root; a lossy conversion would persist
        // a hook pointing at a path that does not exist. Say so instead.
        return vec![BootstrapItem {
            path: pulse_root,
            kind: ItemKind::Directory,
            status: ItemStatus::Skipped("pulse root is not valid UTF-8".into()),
        }];
    }
    let claude_dir = pulse_root.join(".claude");
    let ok = |item: &BootstrapItem| matches!(item.status, ItemStatus::Created | ItemStatus::Exists);

    let mut items = Vec::new();
    // Claude Code reads CLAUDE.md; the pulse's instructions live in the
    // generic INSTRUCTIONS.md, so CLAUDE.md is a one-line import of it.
    // Pulses created before PN-106 already have a full CLAUDE.md, which
    // `ensure_config` leaves alone.
    items.push(ensure_config(
        &pulse_root.join("CLAUDE.md"),
        "@INSTRUCTIONS.md\n",
        &pulse_root,
    ));
    let claude = ensure_dir(&claude_dir);
    let claude_ok = ok(&claude);
    items.push(claude);
    if claude_ok {
        let rules = ensure_dir(&claude_dir.join("rules"));
        let rules_ok = ok(&rules);
        items.push(rules);
        items.push(ensure_settings(
            &claude_dir.join("settings.json"),
            &render_hooks(recall_bin, &pulse_root),
            &pulse_root,
        ));
        if rules_ok {
            items.push(ensure_generated(
                &claude_dir.join("rules").join(RECALL_ECHO_RULES_FILE),
                render_rules_md(),
                RULES_HEADER,
                &pulse_root,
            ));
        }
    }
    items
}

/// Verify Claude Code integration without creating anything. Also reports
/// leftovers of the pre-PN-104 user-level layout that point at this pulse.
pub(super) fn verify_files(pulse_root: &Path) -> Vec<BootstrapItem> {
    let pulse_root = pulse_root
        .canonicalize()
        .unwrap_or_else(|_| pulse_root.to_path_buf());
    let claude_dir = pulse_root.join(".claude");
    let recall_bin = find_recall_echo_bin();
    let mut items = Vec::new();

    // settings.json: present, carrying the canonical hooks, and no retired one.
    let settings = claude_dir.join("settings.json");
    let status = match read_small_file(&settings) {
        Ok(Some(text)) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(existing) => {
                let ours = render_hooks(&recall_bin, &pulse_root);
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

    let rules = claude_dir.join("rules").join(RECALL_ECHO_RULES_FILE);
    let rules_status = match read_small_file(&rules) {
        Ok(Some(text)) if text == render_rules_md() => ItemStatus::Exists,
        Ok(Some(_)) => ItemStatus::Wrong("differs from the current template".into()),
        Ok(None) => ItemStatus::Missing,
        Err(e) => ItemStatus::Wrong(e),
    };
    items.push(BootstrapItem {
        path: rules,
        kind: ItemKind::ConfigFile,
        status: rules_status,
    });

    if let Some(home) = home_dir() {
        for link in legacy_home_links(&pulse_root, &home) {
            items.push(BootstrapItem {
                path: link,
                kind: ItemKind::Symlink,
                status: ItemStatus::Wrong(
                    "legacy user-level link into this pulse — run 'pulse-null repair'".into(),
                ),
            });
        }
        // User-level consume prints one pulse's EPHEMERAL.md into every
        // Claude Code session. The file stays untouched; verify is not
        // clean while the hook is there.
        if user_hooks_missing_root(&home)
            .iter()
            .any(|hook| hook.consume)
        {
            items.push(BootstrapItem {
                path: home.join(".claude/settings.json"),
                kind: ItemKind::ConfigFile,
                status: ItemStatus::Wrong(
                    "user-level consume still runs in every session; the path picks which file is printed but does not limit which sessions the hook runs in. recall-echo init writes `<recall-bin> consume '<pulse-root>'` — remove that hook by hand".into(),
                ),
            });
        }
    }

    items
}

/// Symlinks in `$HOME/.claude` left by the pre-PN-104 bootstrap that resolve
/// into `pulse_root`. Links into *other* pulses are not ours to touch.
pub(super) fn legacy_home_links(pulse_root: &Path, home: &Path) -> Vec<PathBuf> {
    let root = pulse_root
        .canonicalize()
        .unwrap_or_else(|_| pulse_root.to_path_buf());
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
            // A dangling link cannot be shown to resolve inside the pulse
            // (`..` is not normalised lexically), so it is not ours to remove.
            let Ok(target) = target.canonicalize() else {
                return false;
            };
            target.starts_with(&root)
        })
        .collect()
}

/// Whether `command` names a pulse root. The repair message uses this to
/// choose its wording; a rooted `consume` is still reported.
///
/// `consume` takes the root as the next positional word. A comment (`#`),
/// a redirect (`>`, `2>`, `2>&1`, `&>`), or a shell operator in that
/// position is not a root. Other subcommands carry a root only when
/// `--pulse-root` or `--entity-root` is its own word, not a substring.
fn command_carries_root(command: &str) -> bool {
    let words = shell_words(command);
    let Some(bin) = words.first() else {
        return false;
    };
    let Some(sub) = words.get(1) else {
        return false;
    };
    let recall = Path::new(&bin.text)
        .file_name()
        .is_some_and(|name| name == "recall-echo");
    if !recall {
        return false;
    }
    if sub.text == "consume" {
        if let Some(next) = words.get(2) {
            if next.quoted || !is_operator_or_redirect(&next.text) {
                return true;
            }
        }
    }
    words
        .iter()
        .any(|word| !word.quoted && is_root_flag(&word.text))
}

/// recall-echo hook commands in the user-level `$HOME/.claude/settings.json`
/// that repair reports and does not edit.
///
/// Every `consume` hook is reported, with or without a positional root.
/// `recall-echo init` writes `<recall-bin> consume '<pulse-root>'` there.
/// The path chooses which `memory/EPHEMERAL.md` is printed; Claude Code
/// runs user-level hooks in every session, so the path does not limit
/// which sessions the hook runs in. Other recall-echo hooks are reported
/// only when they name no pulse root.
pub(super) fn user_hooks_missing_root(home: &Path) -> Vec<ReportedUserHook> {
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
                    let consume = is_recall_command(command, "consume");
                    let ours = consume
                        || RECALL_HOOKS
                            .iter()
                            .any(|(_, sub)| is_recall_command(command, sub));
                    if !ours {
                        continue;
                    }
                    let carries_root = command_carries_root(command);
                    if consume || !carries_root {
                        found.push(ReportedUserHook {
                            command: command.to_string(),
                            carries_root,
                            consume,
                        });
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
        let mut items = verify_common(root, "claude-code");
        items.extend(verify_files(root));
        items
    }

    fn pulse() -> tempfile::TempDir {
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
    fn ensure_fresh_pulse_writes_all_items() {
        let dir = pulse();
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
        assert!(doc["hooks"].get("SessionStart").is_none(), "{doc}");
        assert!(!doc.to_string().contains("consume"), "{doc}");
        assert!(commands(&doc, "PreCompact")[0].ends_with(&format!(
            "checkpoint --trigger precompact --pulse-root {quoted}"
        )));
        assert!(commands(&doc, "SessionEnd")[0]
            .ends_with(&format!("archive-session --pulse-root {quoted}")));

        assert!(root.join(".claude/rules/recall-echo.md").exists());
        assert!(root.join("memory/.recall-echo.toml").exists());
        assert_eq!(
            std::fs::read_link(root.join("memory/conversations")).unwrap(),
            PathBuf::from("../archives/conversations")
        );
    }

    #[test]
    fn ensure_is_idempotent() {
        let dir = pulse();
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
        let dir = pulse();
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
        assert!(end[1].contains("--pulse-root"));
        assert!(!end[1].contains("|| true"));
        let pre = commands(&doc, "PreCompact");
        assert_eq!(pre.len(), 1);
        assert!(pre[0].contains("--pulse-root"));
        assert!(doc["hooks"].get("SessionStart").is_none(), "{doc}");
    }

    /// PN-115: hooks written by a pre-rename binary carry `--entity-root`.
    /// `verify` reports them stale, `ensure` (what `repair` runs) rewrites
    /// them to `--pulse-root` in place, and a second run changes nothing.
    #[test]
    fn repair_rewrites_legacy_entity_root_hooks() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let bin = find_recall_echo_bin();
        let legacy = |cmd: &str| cmd.replace("--pulse-root", "--entity-root");
        let ours = render_hooks(&bin, &root);
        let mut old = ours.clone();
        for (event, _) in RECALL_HOOKS {
            let command = old[event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .to_string();
            old[event][0]["hooks"][0]["command"] = legacy(&command).into();
        }
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            root.join(".claude/settings.json"),
            serde_json::json!({ "hooks": old, "permissions": {"allow": []} }).to_string(),
        )
        .unwrap();
        let before = read_settings(&root);
        assert!(commands(&before, "SessionEnd")[0].contains("--entity-root"));

        let stale = verify_all(&root);
        let settings = stale
            .iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert!(
            matches!(settings.status, ItemStatus::Wrong(_)),
            "{settings:?}"
        );

        let items = ensure_all(&root);
        let settings = items
            .iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert_eq!(settings.status, ItemStatus::Updated);
        let doc = read_settings(&root);
        let quoted = shell_quote(&root);
        for (event, _) in RECALL_HOOKS {
            let cmds = commands(&doc, event);
            assert_eq!(cmds.len(), 1, "{event}: {cmds:?}");
            assert!(!cmds[0].contains("--entity-root"), "{event}: {cmds:?}");
        }
        assert!(commands(&doc, "PreCompact")[0].ends_with(&format!(
            "checkpoint --trigger precompact --pulse-root {quoted}"
        )));
        assert!(commands(&doc, "SessionEnd")[0]
            .ends_with(&format!("archive-session --pulse-root {quoted}")));
        assert!(doc["permissions"].is_object(), "foreign keys survive");

        let again = ensure_all(&root);
        assert!(
            again.iter().all(|i| i.status == ItemStatus::Exists),
            "{again:?}"
        );
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
    fn legacy_links_only_match_this_pulse() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let other = pulse();
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

        // A dangling link cannot be shown to point inside the pulse — even
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

    fn reported_commands(home: &Path) -> Vec<String> {
        let mut found: Vec<String> = user_hooks_missing_root(home)
            .into_iter()
            .map(|hook| hook.command)
            .collect();
        found.sort();
        found
    }

    struct HomeOverride;
    impl Drop for HomeOverride {
        fn drop(&mut self) {
            set_home_override(None);
        }
    }
    fn override_home(path: &Path) -> HomeOverride {
        set_home_override(Some(path.to_path_buf()));
        HomeOverride
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
                "Stop": [{"hooks": [{"type": "command", "command": "recall-echo checkpoint --trigger precompact --pulse-root '/srv/e'"}]}],
                "SubagentStop": [{"hooks": [{"type": "command", "command": "recall-echo checkpoint --trigger precompact --pulse-root '/srv/e'"}]}]
            }
        })
        .to_string();
        std::fs::write(claude.join("settings.json"), &original).unwrap();

        let found = user_hooks_missing_root(home.path());
        assert_eq!(
            reported_commands(home.path()),
            vec![
                "/usr/local/bin/recall-echo archive-session || true".to_string(),
                "/usr/local/bin/recall-echo checkpoint --trigger precompact".to_string(),
                "/usr/local/bin/recall-echo consume '/srv/e'".to_string(),
            ]
        );
        let consume = found
            .iter()
            .find(|hook| hook.command.contains("consume"))
            .unwrap();
        assert!(consume.consume && consume.carries_root, "{consume:?}");
        assert!(found
            .iter()
            .filter(|hook| !hook.consume)
            .all(|hook| !hook.carries_root));
        assert_eq!(
            std::fs::read_to_string(claude.join("settings.json")).unwrap(),
            original
        );
    }

    /// `recall-echo init` writes this SessionStart command into the user-level
    /// settings file: an unquoted binary, `consume`, and the pulse root in
    /// single quotes. The root picks the file that is printed. It does not
    /// limit which sessions the hook runs in, and the file is not edited.
    #[test]
    fn user_level_recall_echo_init_consume_is_reported_and_verify_is_not_clean() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let command = "/usr/local/bin/recall-echo consume '/srv/pulse-a'";
        let original = serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "matcher": "startup|resume",
                    "hooks": [{"type": "command", "command": command}]
                }],
                "SessionEnd": [{"hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session --pulse-root '/srv/pulse-a'"}]}],
                "PreCompact": [{"hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo checkpoint --trigger precompact --pulse-root '/srv/pulse-a'"}]}]
            }
        })
        .to_string();
        std::fs::write(claude.join("settings.json"), &original).unwrap();

        let found = user_hooks_missing_root(home.path());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].command, command);
        assert!(found[0].consume && found[0].carries_root, "{found:?}");
        assert_eq!(
            std::fs::read_to_string(claude.join("settings.json")).unwrap(),
            original
        );

        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        ensure_files(&root, "recall-echo");
        let _home = override_home(home.path());
        let items = verify_files(&root);
        let user = items
            .iter()
            .find(|item| item.path == home.path().join(".claude/settings.json"))
            .expect("user-level settings are part of verify");
        match &user.status {
            ItemStatus::Wrong(reason) => {
                assert!(
                    reason.contains(
                        "the path picks which file is printed but does not limit which sessions the hook runs in"
                    ),
                    "{reason}"
                );
                assert!(
                    reason
                        .contains("recall-echo init writes `<recall-bin> consume '<pulse-root>'`"),
                    "{reason}"
                );
            }
            other => panic!("verify reported clean for a user-level consume hook: {other:?}"),
        }
        assert!(
            items
                .iter()
                .any(|item| !matches!(item.status, ItemStatus::Exists)),
            "verify reported clean: {items:?}"
        );
        assert_eq!(
            std::fs::read_to_string(claude.join("settings.json")).unwrap(),
            original,
            "verify must not edit the user-level file"
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
        let dir = pulse();
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
        let dir = pulse();
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

        // A direct write whose parent resolves outside the pulse is refused.
        let err = write_regular_file(&elsewhere.path().join("x.json"), "{}", &root).unwrap_err();
        assert!(err.contains("outside the pulse"), "{err}");

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
        let dir = pulse();
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
            reported_commands(home.path()),
            vec!["recall-echo consume || true".to_string()]
        );
        assert!(!user_hooks_missing_root(home.path())[0].carries_root);
    }

    #[test]
    fn redirects_comments_and_operators_are_not_a_root() {
        let rootless = [
            "recall-echo consume 2>/dev/null",
            "recall-echo consume 2>&1",
            "recall-echo consume &>/dev/null",
            "recall-echo consume >/dev/null",
            "recall-echo consume > /dev/null",
            "recall-echo consume # comment",
            "recall-echo consume # --pulse-root",
            "recall-echo consume || true",
            "recall-echo consume && true",
            "recall-echo consume | cat",
            "recall-echo consume &",
            "recall-echo consume ; echo hi",
            "recall-echo archive-session # --pulse-root",
            "recall-echo checkpoint --trigger precompact # --pulse-root /e",
            "recall-echo archive-session --not-pulse-root",
        ];
        for command in rootless {
            assert!(!command_carries_root(command), "{command}");
        }
        let rooted = [
            "recall-echo consume '/srv/e'",
            "/usr/local/bin/recall-echo consume '/srv/pulse-a'",
            "recall-echo consume /srv/e",
            "recall-echo consume '/srv/e' || true",
            "recall-echo consume '2>/dev/null'",
            "recall-echo archive-session --pulse-root '/srv/e'",
            "recall-echo archive-session --entity-root '/srv/e'",
            "recall-echo checkpoint --trigger precompact --pulse-root=/srv/e",
            "'/opt/my tools/recall-echo' consume '/e'",
        ];
        for command in rooted {
            assert!(command_carries_root(command), "{command}");
        }
    }

    #[test]
    fn user_level_redirect_and_comment_consume_are_rootless() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let original = serde_json::json!({
            "hooks": {
                "SessionStart": [{"hooks": [
                    {"type": "command", "command": "recall-echo consume 2>/dev/null"},
                    {"type": "command", "command": "recall-echo consume 2>&1"},
                    {"type": "command", "command": "recall-echo consume &>/dev/null"},
                    {"type": "command", "command": "recall-echo consume # --pulse-root"}
                ]}]
            }
        })
        .to_string();
        std::fs::write(claude.join("settings.json"), &original).unwrap();

        let found = user_hooks_missing_root(home.path());
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(
            found.iter().all(|hook| hook.consume && !hook.carries_root),
            "{found:?}"
        );
        assert_eq!(
            std::fs::read_to_string(claude.join("settings.json")).unwrap(),
            original
        );
    }

    #[test]
    fn fresh_pulse_has_no_session_start_hook() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let items = ensure_files(&root, "recall-echo");
        let settings = items
            .iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert_eq!(settings.status, ItemStatus::Created);

        let doc = read_settings(&root);
        assert!(doc["hooks"].get("SessionStart").is_none(), "{doc}");
        assert!(!doc.to_string().contains("consume"), "{doc}");
        let quoted = shell_quote(&root);
        assert!(commands(&doc, "PreCompact")[0].ends_with(&format!(
            "checkpoint --trigger precompact --pulse-root {quoted}"
        )));
        assert!(commands(&doc, "SessionEnd")[0]
            .ends_with(&format!("archive-session --pulse-root {quoted}")));
    }

    #[test]
    fn repair_strips_consume_keeps_foreign_hook() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            root.join(".claude/settings.json"),
            serde_json::json!({
                "hooks": {
                    "SessionStart": [{
                        "matcher": "startup|resume",
                        "hooks": [
                            {"type": "command", "command": format!("recall-echo consume {}", shell_quote(&root))},
                            {"type": "command", "command": "echo foreign"}
                        ]
                    }],
                    "PreCompact": [{"hooks": [{"type": "command", "command": "recall-echo checkpoint --trigger precompact"}]}],
                    "SessionEnd": [{"hooks": [{"type": "command", "command": "recall-echo archive-session || true"}]}]
                }
            })
            .to_string(),
        )
        .unwrap();

        let before = verify_files(&root)
            .into_iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert!(
            matches!(before.status, ItemStatus::Wrong(ref reason) if reason.contains("stale")),
            "{before:?}"
        );

        let items = ensure_files(&root, "recall-echo");
        let settings = items
            .iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert_eq!(settings.status, ItemStatus::Updated);

        let doc = read_settings(&root);
        assert_eq!(
            commands(&doc, "SessionStart"),
            vec!["echo foreign".to_string()]
        );
        assert!(!doc.to_string().contains("consume"), "{doc}");
        let quoted = shell_quote(&root);
        assert!(commands(&doc, "PreCompact")[0].ends_with(&format!(
            "checkpoint --trigger precompact --pulse-root {quoted}"
        )));
        assert!(commands(&doc, "SessionEnd")[0]
            .ends_with(&format!("archive-session --pulse-root {quoted}")));

        let after = verify_files(&root)
            .into_iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert_eq!(after.status, ItemStatus::Exists, "{after:?}");
    }

    #[test]
    fn verify_flags_leftover_consume() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let bin = find_recall_echo_bin();
        let mut hooks = render_hooks(&bin, &root);
        hooks["SessionStart"] = serde_json::json!([{
            "matcher": "startup|resume",
            "hooks": [{"type": "command", "command": hook_command(&bin, "consume", &root)}]
        }]);
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            root.join(".claude/settings.json"),
            serde_json::to_string_pretty(&serde_json::json!({ "hooks": hooks })).unwrap() + "\n",
        )
        .unwrap();

        let before = verify_files(&root)
            .into_iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert!(
            matches!(before.status, ItemStatus::Wrong(ref reason) if reason.contains("stale")),
            "{before:?}"
        );

        let items = ensure_files(&root, &bin);
        let settings = items
            .iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert_eq!(settings.status, ItemStatus::Updated);
        let doc = read_settings(&root);
        assert!(doc["hooks"].get("SessionStart").is_none(), "{doc}");
        assert!(!doc.to_string().contains("consume"), "{doc}");

        let after = verify_files(&root)
            .into_iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert_eq!(after.status, ItemStatus::Exists, "{after:?}");
    }

    #[test]
    fn empty_event_removed_after_strip() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            root.join(".claude/settings.json"),
            serde_json::json!({
                "permissions": {"allow": ["Bash(ls:*)"]},
                "hooks": {
                    "SessionStart": [{"hooks": [
                        {"type": "command", "command": format!("recall-echo consume {}", shell_quote(&root))}
                    ]}]
                }
            })
            .to_string(),
        )
        .unwrap();

        let items = ensure_files(&root, "recall-echo");
        let settings = items
            .iter()
            .find(|i| i.path.ends_with("settings.json"))
            .unwrap();
        assert_eq!(settings.status, ItemStatus::Updated);

        let doc = read_settings(&root);
        assert!(
            doc["hooks"].get("SessionStart").is_none(),
            "empty event removed: {doc}"
        );
        assert!(doc["hooks"]["PreCompact"].is_array(), "{doc}");
        assert!(doc["hooks"]["SessionEnd"].is_array(), "{doc}");
        assert_eq!(doc["permissions"]["allow"][0], "Bash(ls:*)");
    }

    #[test]
    fn user_level_rootless_consume_still_reported() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let original = serde_json::json!({
            "hooks": {
                "SessionStart": [{"hooks": [
                    {"type": "command", "command": "recall-echo consume || true"}
                ]}]
            }
        })
        .to_string();
        std::fs::write(claude.join("settings.json"), &original).unwrap();

        assert_eq!(
            reported_commands(home.path()),
            vec!["recall-echo consume || true".to_string()]
        );
        assert_eq!(
            std::fs::read_to_string(claude.join("settings.json")).unwrap(),
            original
        );
    }

    #[test]
    fn ensure_twice_is_idempotent() {
        let dir = pulse();
        let root = dir.path();
        let first = ensure_files(root, "recall-echo");
        assert!(
            first
                .iter()
                .any(|i| { i.path.ends_with("settings.json") && i.status == ItemStatus::Created }),
            "{first:?}"
        );
        let second = ensure_files(root, "recall-echo");
        assert!(
            second.iter().all(|i| i.status == ItemStatus::Exists),
            "{second:?}"
        );
        let doc = read_settings(root);
        assert!(doc["hooks"].get("SessionStart").is_none(), "{doc}");
        assert!(!doc.to_string().contains("consume"), "{doc}");
    }

    /// The rules file pulses carried before the 4.x lifecycle rewrite.
    fn rules_item(items: &[BootstrapItem]) -> &BootstrapItem {
        items
            .iter()
            .find(|i| i.path.ends_with("recall-echo.md"))
            .expect("rules item")
    }

    #[test]
    fn rules_template_has_no_manual_checkpoint_or_consume() {
        let rules = render_rules_md();
        assert!(rules.starts_with(RULES_HEADER));
        assert!(!rules.contains("fill in"), "{rules}");
        assert!(!rules.contains("consume"), "{rules}");
        assert!(!rules.contains("entitys"));
        assert!(rules.contains("graph entities"));
        assert!(rules.contains(
            "Loaded into your chat system prompt by pulse-null; not in scheduled task runs; written by the runtime at session end; do not edit."
        ));
        assert!(rules.contains("writes the full conversation automatically"));
        assert!(rules.contains("archive-session"));
        assert!(rules.contains("memory/MEMORY.md"));
        assert!(rules.contains("archives/conversations/"));
    }

    #[test]
    fn stale_generated_rules_rewritten() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join(".claude/rules/recall-echo.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, OLD_GENERATED_RULES).unwrap();

        let items = ensure_files(&root, "recall-echo");
        assert_eq!(rules_item(&items).status, ItemStatus::Updated);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, render_rules_md());
        assert!(!text.contains("fill in"), "{text}");
        assert!(!text.contains("consume"), "{text}");

        let again = verify_files(&root);
        assert_eq!(rules_item(&again).status, ItemStatus::Exists);
    }

    #[test]
    fn hand_edited_rules_left_alone() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join(".claude/rules/recall-echo.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "# My own memory notes\nDo not replace this.\n";
        std::fs::write(&path, original).unwrap();

        let items = ensure_files(&root, "recall-echo");
        assert!(
            matches!(
                &rules_item(&items).status,
                ItemStatus::Skipped(reason) if reason == "hand-edited; left as is"
            ),
            "{items:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        let verified = verify_files(&root);
        assert!(
            matches!(
                &rules_item(&verified).status,
                ItemStatus::Wrong(reason) if reason == "differs from the current template"
            ),
            "{verified:?}"
        );
    }

    #[test]
    fn longer_rules_title_is_kept() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join(".claude/rules/recall-echo.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "# recall-echo — Memory Protocol (local notes)\nKeep this.\n";
        std::fs::write(&path, original).unwrap();

        let items = ensure_files(&root, "recall-echo");
        assert!(
            matches!(
                &rules_item(&items).status,
                ItemStatus::Skipped(reason) if reason == "hand-edited; left as is"
            ),
            "{items:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
    }

    #[test]
    fn exact_rules_title_with_extra_notes_is_kept() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join(".claude/rules/recall-echo.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "# recall-echo — Memory Protocol\n\nLocal notes that must stay.\n";
        std::fs::write(&path, original).unwrap();

        let items = ensure_files(&root, "recall-echo");
        assert!(
            matches!(
                &rules_item(&items).status,
                ItemStatus::Skipped(reason) if reason == "hand-edited; left as is"
            ),
            "{items:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
    }

    #[test]
    fn bom_stock_rules_are_refreshed() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join(".claude/rules/recall-echo.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = format!("\u{FEFF}{OLD_GENERATED_RULES}");
        std::fs::write(&path, &original).unwrap();

        let items = ensure_files(&root, "recall-echo");
        assert_eq!(rules_item(&items).status, ItemStatus::Updated);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), render_rules_md());
    }

    #[test]
    fn crlf_stock_rules_are_refreshed() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join(".claude/rules/recall-echo.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = render_rules_md().replace('\n', "\r\n");
        std::fs::write(&path, &original).unwrap();

        let items = ensure_files(&root, "recall-echo");
        assert_eq!(rules_item(&items).status, ItemStatus::Updated);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), render_rules_md());
    }

    #[test]
    fn verify_reports_stale_rules() {
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join(".claude/rules/recall-echo.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, OLD_GENERATED_RULES).unwrap();

        let items = verify_files(&root);
        assert!(
            matches!(
                &rules_item(&items).status,
                ItemStatus::Wrong(reason) if reason == "differs from the current template"
            ),
            "{items:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), OLD_GENERATED_RULES);
    }

    #[test]
    fn no_hook_injects_ephemeral() {
        assert!(
            RECALL_HOOKS.iter().all(|(_, sub)| *sub != "consume"),
            "RECALL_HOOKS installs consume: {RECALL_HOOKS:?}"
        );
        let dir = pulse();
        let root = dir.path().canonicalize().unwrap();
        ensure_files(&root, "recall-echo");
        let doc = read_settings(&root);
        assert!(doc["hooks"].get("SessionStart").is_none(), "{doc}");
        let hooks = doc["hooks"].as_object().expect("hooks object");
        for event in hooks.keys() {
            for command in commands(&doc, event) {
                assert!(
                    !is_recall_command(&command, "consume"),
                    "{event} injects EPHEMERAL via {command}"
                );
            }
        }
    }
}
