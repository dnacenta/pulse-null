//! One module per agent CLI. This is the only place in the crate where a
//! vendor's name, flags and file conventions are allowed to appear.

pub mod claude;
mod claude_hooks;
pub mod codex;
pub mod grok;

use std::path::{Path, PathBuf};

use super::adapter::{CliAdapter, Observed, ToolCall};

/// The pulse's `.claude/rules/` directory, which Claude Code loads by itself
/// and other CLIs get inlined by the prompt builder.
fn shared_rules_dir(root: &Path) -> PathBuf {
    root.join(".claude/rules")
}

/// Files in [`shared_rules_dir`] written for Claude Code alone; another CLI
/// inlining the directory skips them.
const CLAUDE_ONLY_RULES: &[&str] = &[claude_hooks::RECALL_ECHO_RULES_FILE];

/// The `message` of the first `{"type":"error"}` record in an NDJSON
/// stdout (empty when the record has none), for CLIs that report failures
/// that way.
fn tagged_error_message(stdout: &str) -> Option<String> {
    fn tagged_error(text: &str) -> Option<String> {
        let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
        (value.get("type").and_then(serde_json::Value::as_str) == Some("error")).then(|| {
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
    }
    tagged_error(stdout.trim())
        .or_else(|| stdout.lines().find_map(|line| tagged_error(line.trim())))
}

/// Every adapter the crate ships: config name and constructor. One list, so
/// validation, the wizards and construction cannot drift apart.
type Build = fn() -> Box<dyn CliAdapter>;

const REGISTRY: [(&str, Build); 3] = [
    ("claude", || Box::new(claude::Claude)),
    ("grok", || Box::new(grok::Grok)),
    ("codex", || Box::new(codex::Codex)),
];

/// Every adapter the crate ships, by config name.
pub const NAMES: [&str; 3] = [REGISTRY[0].0, REGISTRY[1].0, REGISTRY[2].0];

/// The adapter for a wizard-shaped (provider, adapter) pair, folding the
/// pre-PN-106 `claude-code` spelling.
pub fn for_config(provider: &str, adapter: Option<&str>) -> Option<Box<dyn CliAdapter>> {
    match provider {
        "cli" => adapter.and_then(by_name),
        "claude-code" => by_name("claude"), // vendor-ok: pre-PN-106 alias
        _ => None,
    }
}

/// The adapter whose CLI recall-echo names `provider` in its own `[llm]`
/// section — the reverse of `AgentIntegration::recall_echo_provider`.
pub fn by_recall_echo_provider(provider: &str) -> Option<Box<dyn CliAdapter>> {
    REGISTRY
        .iter()
        .map(|(_, build)| build())
        .find(|adapter| adapter.integration().recall_echo_provider() == provider)
}

/// Look an adapter up by its `[llm] adapter` value.
pub fn by_name(name: &str) -> Option<Box<dyn CliAdapter>> {
    REGISTRY
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, build)| build())
}

/// What one line of an agent-stream (`stream-json --verbose`-shaped) output
/// says beyond the reply: the model named by the init record and by each
/// assistant message, and every `tool_use` block in an assistant message.
/// Shared by the CLIs that speak this wire format.
fn observe_agent_stream_line(line: &str) -> Vec<Observed> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return Vec::new();
    };
    let model = |v: &serde_json::Value| {
        v.as_str()
            .filter(|m| !m.trim().is_empty())
            .map(|m| Observed::ServedModel(m.to_string()))
    };
    match value["type"].as_str() {
        Some("system") if value["subtype"].as_str() == Some("init") => {
            model(&value["model"]).into_iter().collect()
        }
        Some("assistant") => {
            let message = &value["message"];
            let mut seen: Vec<Observed> = model(&message["model"]).into_iter().collect();
            seen.extend(
                message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|block| block["type"].as_str() == Some("tool_use"))
                    .filter_map(|block| {
                        Some(Observed::ToolCall(ToolCall {
                            id: block["id"].as_str().map(str::to_string),
                            ..ToolCall::new(block["name"].as_str()?, block["input"].clone())
                        }))
                    }),
            );
            seen
        }
        Some("user") => value["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"].as_str() == Some("tool_result"))
            .filter_map(|block| {
                Some(Observed::ToolResult {
                    tool_use_id: block["tool_use_id"].as_str()?.to_string(),
                    text: tool_result_text(&block["content"]),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Longest tool result text kept per call.
const TOOL_RESULT_MAX_BYTES: usize = 8 * 1024;

/// A tool result's text: a string, or the text blocks of a list (bounded).
fn tool_result_text(content: &serde_json::Value) -> String {
    let text = match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    crate::utils::safe_truncate(&text, TOOL_RESULT_MAX_BYTES).to_string()
}

/// Copy one file into a probe scratch, private to the owner (0600). Never
/// follows a symlink at the source: a link is skipped, not resolved.
fn copy_private_file(src: &Path, dst: &Path) -> std::io::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::symlink_metadata(src) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    }
    let Some((mut file, _)) = crate::fs_safe::open_regular(src)? else {
        return Ok(false);
    };
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)?;
    std::io::copy(&mut file, &mut out)?;
    out.flush()?;
    Ok(true)
}

/// Copy a directory tree of regular files into a probe scratch, private to
/// the owner. Symlinks and special files are skipped. A missing source is an
/// empty copy.
fn copy_private_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !is_real_dir(src) {
        return Ok(());
    }
    create_private_dir(dst)?;
    for entry in std::fs::read_dir(src)? {
        copy_private_entry(&entry?, dst)?;
    }
    Ok(())
}

/// Copy one entry of a tree into `dst`: a directory through, a regular file
/// as itself; anything else is skipped.
fn copy_private_entry(entry: &std::fs::DirEntry, dst: &Path) -> std::io::Result<()> {
    let kind = entry.file_type()?;
    let to = dst.join(entry.file_name());
    if kind.is_dir() {
        copy_private_tree(&entry.path(), &to)
    } else if kind.is_file() {
        copy_private_file(&entry.path(), &to).map(|_| ())
    } else {
        Ok(())
    }
}

/// A directory, and not a symlink to one.
fn is_real_dir(path: &Path) -> bool {
    path.is_dir() && !path.is_symlink()
}

/// Create `dir` (and missing parents) with mode 0700.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(test)]
mod probe_support_tests {
    use super::*;

    #[test]
    fn agent_stream_lines_yield_model_and_tool_calls() {
        let fixture =
            include_str!("../../../tests/fixtures/grok-build/probe-streaming-messages.ndjson");
        let seen: Vec<Observed> = fixture
            .lines()
            .flat_map(observe_agent_stream_line)
            .collect();
        assert_eq!(seen[0], Observed::ServedModel("grok-4.6".into()));
        let calls: Vec<&ToolCall> = seen
            .iter()
            .filter_map(|o| match o {
                Observed::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_terminal_command");
        assert_eq!(calls[0].input["command"], "ls");
        assert!(observe_agent_stream_line("not json").is_empty());
        let result = observe_agent_stream_line(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"lychee"}]}]}}"#,
        );
        assert_eq!(
            result,
            [Observed::ToolResult {
                tool_use_id: "t1".into(),
                text: "lychee".into()
            }]
        );
        assert!(observe_agent_stream_line(r#"{"type":"result","result":"x"}"#).is_empty());
    }

    #[test]
    fn private_copies_skip_links_and_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/a.md"), "a").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", src.path().join("link")).unwrap();
        copy_private_tree(src.path(), &dst.path().join("copy")).unwrap();
        let copied = dst.path().join("copy/sub/a.md");
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), "a");
        assert_eq!(
            std::fs::metadata(&copied).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!dst.path().join("copy/link").exists());
        assert_eq!(
            std::fs::metadata(dst.path().join("copy"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
