//! Daily task digest — consolidates scheduled task and intent output into
//! a single EPHEMERAL.md entry instead of one per execution.
//!
//! Called as a post-step after the last scheduled task of the day (typically
//! night-reflection), or on-demand via the write_task_digest() function.

use std::fs;
use std::path::Path;

use chrono::Utc;

/// Scan today's conversation archives for task/intent executions and write
/// a consolidated EPHEMERAL.md summary.
///
/// Reads the archive INDEX.md to find entries with task/research triggers
/// from today, then reads each archive to extract summaries. Writes a single
/// "Task Digest" entry to EPHEMERAL.md.
pub fn write_task_digest(root_dir: &Path, entity_name: &str) {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    write_task_digest_for_date(root_dir, entity_name, &today);
}

/// Write a task digest for a specific date (for testing and backfill).
pub fn write_task_digest_for_date(root_dir: &Path, entity_name: &str, date: &str) {
    let index_path = root_dir
        .join("archives")
        .join("conversations")
        .join("INDEX.md");

    if !index_path.exists() {
        tracing::debug!("No INDEX.md found, skipping task digest");
        return;
    }

    let index_content = match fs::read_to_string(&index_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Cannot read INDEX.md for digest: {}", e);
            return;
        }
    };

    // Parse INDEX.md table rows to find today's task/research archives
    // Format: | NNN | YYYY-MM-DD | trigger | channel | N |
    let task_entries: Vec<TaskEntry> = index_content
        .lines()
        .filter(|line| line.starts_with('|') && line.contains(date))
        .filter_map(parse_index_line)
        .filter(|entry| is_task_trigger(&entry.trigger))
        .collect();

    if task_entries.is_empty() {
        tracing::debug!("No task archives found for {} — skipping digest", date);
        return;
    }

    // Read each archive and extract the assistant's response summary
    let conv_dir = root_dir.join("archives").join("conversations");
    let mut summaries: Vec<DigestEntry> = Vec::new();

    for entry in &task_entries {
        let archive_path = conv_dir.join(format!("conversation-{:03}.md", entry.log_num));
        if let Ok(content) = fs::read_to_string(&archive_path) {
            let summary = extract_assistant_summary(&content);
            if !summary.is_empty() {
                summaries.push(DigestEntry {
                    task_name: entry.channel.clone(),
                    summary,
                });
            }
        }
    }

    if summaries.is_empty() {
        tracing::debug!("No substantive task output for {} — skipping digest", date);
        return;
    }

    // Build the digest content
    let now = Utc::now().format("%Y-%m-%d %H:%M UTC");
    let mut content = format!("## Task Digest — {}\n\n", now);
    content.push_str(&format!(
        "{} completed {} task(s)\n\n",
        entity_name,
        summaries.len()
    ));
    content.push_str("### Key outputs\n\n");

    for entry in &summaries {
        let display_name = entry
            .task_name
            .strip_prefix("task:")
            .or_else(|| entry.task_name.strip_prefix("research:"))
            .unwrap_or(&entry.task_name);
        content.push_str(&format!("- **{}**: {}\n", display_name, entry.summary));
    }

    // Write to EPHEMERAL.md
    let ephemeral_path = root_dir.join("memory").join("EPHEMERAL.md");
    if let Err(e) = fs::write(&ephemeral_path, content) {
        tracing::warn!("Could not save task digest to EPHEMERAL.md: {}", e);
    } else {
        tracing::info!(
            "Task digest written to EPHEMERAL.md ({} entries for {})",
            summaries.len(),
            date
        );
    }
}

#[derive(Debug)]
struct TaskEntry {
    log_num: u32,
    trigger: String,
    channel: String,
}

struct DigestEntry {
    task_name: String,
    summary: String,
}

/// Check if a trigger string indicates a task or research execution.
fn is_task_trigger(trigger: &str) -> bool {
    trigger.starts_with("task-end-")
        || trigger.starts_with("research-end-")
        || trigger == "task-execution"
}

/// Parse a single INDEX.md table row into a TaskEntry.
/// Format: | NNN | YYYY-MM-DD | trigger | channel | N |
fn parse_index_line(line: &str) -> Option<TaskEntry> {
    let parts: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
    if parts.len() < 6 {
        return None;
    }

    let log_num: u32 = parts[1].parse().ok()?;
    let trigger = parts[3].to_string();
    let channel = parts[4].to_string();

    Some(TaskEntry {
        log_num,
        trigger,
        channel,
    })
}

/// Extract a brief summary from an archive file's assistant response.
/// Takes the last assistant message and truncates to ~200 chars.
fn extract_assistant_summary(archive_content: &str) -> String {
    // Find the last "### Assistant" or similar block
    let mut last_assistant = String::new();

    let mut in_assistant = false;
    for line in archive_content.lines() {
        if line.starts_with("### Assistant") || line.starts_with("**Assistant**") {
            in_assistant = true;
            last_assistant.clear();
            continue;
        }
        if line.starts_with("### User") || line.starts_with("**User**") || line.starts_with("---") {
            in_assistant = false;
        }
        if in_assistant && !line.trim().is_empty() {
            if !last_assistant.is_empty() {
                last_assistant.push(' ');
            }
            last_assistant.push_str(line.trim());
        }
    }

    // Truncate to ~200 chars
    if last_assistant.len() > 200 {
        let truncated = crate::utils::safe_truncate(&last_assistant, 200);
        format!("{}...", truncated)
    } else {
        last_assistant
    }
}

/// Check if it's time to write the daily digest.
///
/// Returns true if:
/// 1. There are task archives for today that haven't been digested yet
/// 2. The current EPHEMERAL doesn't already contain today's digest
pub fn needs_digest(root_dir: &Path) -> bool {
    let today = Utc::now().format("%Y-%m-%d").to_string();

    // Check if EPHEMERAL already has today's digest
    let ephemeral_path = root_dir.join("memory").join("EPHEMERAL.md");
    if let Ok(content) = fs::read_to_string(&ephemeral_path) {
        if content.contains(&format!("Task Digest — {}", today)) {
            return false;
        }
    }

    // Check if there are task archives for today
    let index_path = root_dir
        .join("archives")
        .join("conversations")
        .join("INDEX.md");
    if let Ok(content) = fs::read_to_string(&index_path) {
        return content
            .lines()
            .filter(|line| line.starts_with('|') && line.contains(&today))
            .filter_map(parse_index_line)
            .any(|entry| is_task_trigger(&entry.trigger));
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        let conv_dir = dir.path().join("archives").join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        dir
    }

    #[test]
    fn parse_index_line_extracts_fields() {
        let line =
            "| 042 | 2026-04-01 | task-end-morning_orientation | task:morning_orientation | 3 |";
        let entry = parse_index_line(line).unwrap();
        assert_eq!(entry.log_num, 42);
        assert_eq!(entry.trigger, "task-end-morning_orientation");
        assert_eq!(entry.channel, "task:morning_orientation");
    }

    #[test]
    fn parse_index_line_rejects_short_lines() {
        assert!(parse_index_line("| 042 | 2026-04-01 |").is_none());
    }

    #[test]
    fn is_task_trigger_identifies_task_triggers() {
        assert!(is_task_trigger("task-end-morning_orientation"));
        assert!(is_task_trigger("research-end-consciousness"));
        assert!(is_task_trigger("task-execution"));
        assert!(!is_task_trigger("session-end"));
        assert!(!is_task_trigger("comms-end"));
        assert!(!is_task_trigger("checkpoint"));
    }

    #[test]
    fn extract_assistant_summary_finds_last_response() {
        let content = "---\nlog: 1\n---\n\n### User\n\nHello\n\n### Assistant\n\nFirst response\n\n### User\n\nFollow up\n\n### Assistant\n\nSecond response with more detail\n";
        let summary = extract_assistant_summary(content);
        assert!(summary.contains("Second response"));
        assert!(!summary.contains("First response"));
    }

    #[test]
    fn extract_assistant_summary_truncates_long_responses() {
        let long_text = "x".repeat(500);
        let content = format!("### Assistant\n\n{}\n", long_text);
        let summary = extract_assistant_summary(&content);
        assert!(summary.len() < 210);
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn extract_assistant_summary_returns_empty_for_no_assistant() {
        let content = "### User\n\nHello\n";
        let summary = extract_assistant_summary(content);
        assert!(summary.is_empty());
    }

    #[test]
    fn write_digest_skips_when_no_index() {
        let dir = setup_test_dir();
        // No INDEX.md — should not create EPHEMERAL
        write_task_digest(dir.path(), "Echo");
        let ephemeral = dir.path().join("memory").join("EPHEMERAL.md");
        assert!(!ephemeral.exists());
    }

    #[test]
    fn write_digest_skips_when_no_task_entries() {
        let dir = setup_test_dir();
        let today = Utc::now().format("%Y-%m-%d").to_string();

        // INDEX with only session-end entries
        let index = format!(
            "| Log | Date | Trigger | Channel | Messages |\n|-----|------|---------|---------|----------|\n| 001 | {} | session-end | discord | 10 |\n",
            today
        );
        fs::write(
            dir.path()
                .join("archives")
                .join("conversations")
                .join("INDEX.md"),
            index,
        )
        .unwrap();

        write_task_digest(dir.path(), "Echo");
        let ephemeral = dir.path().join("memory").join("EPHEMERAL.md");
        assert!(!ephemeral.exists());
    }

    #[test]
    fn write_digest_produces_ephemeral() {
        let dir = setup_test_dir();
        let today = Utc::now().format("%Y-%m-%d").to_string();

        // INDEX with a task entry
        let index = format!(
            "| Log | Date | Trigger | Channel | Messages |\n|-----|------|---------|---------|----------|\n| 001 | {} | task-end-morning_orientation | task:morning_orientation | 3 |\n",
            today
        );
        fs::write(
            dir.path()
                .join("archives")
                .join("conversations")
                .join("INDEX.md"),
            index,
        )
        .unwrap();

        // Create the matching archive file
        let archive = "---\nlog: 1\ndate: \"2026-04-01T10:00:00Z\"\ntrigger: task-end-morning_orientation\nchannel: task:morning_orientation\n---\n\n# Conversation 001\n\n### User\n\nTask prompt\n\n### Assistant\n\nCompleted morning orientation. Pipeline is healthy. No stale thoughts.\n";
        fs::write(
            dir.path()
                .join("archives")
                .join("conversations")
                .join("conversation-001.md"),
            archive,
        )
        .unwrap();

        write_task_digest(dir.path(), "Echo");

        let ephemeral = fs::read_to_string(dir.path().join("memory").join("EPHEMERAL.md")).unwrap();
        assert!(ephemeral.contains("Task Digest"));
        assert!(ephemeral.contains("morning_orientation"));
        assert!(ephemeral.contains("Completed morning orientation"));
    }

    #[test]
    fn needs_digest_returns_false_when_already_written() {
        let dir = setup_test_dir();
        let today = Utc::now().format("%Y-%m-%d").to_string();

        // Write an existing digest for today
        let ephemeral = format!("## Task Digest — {} 10:00 UTC\n\nSome content\n", today);
        fs::write(dir.path().join("memory").join("EPHEMERAL.md"), ephemeral).unwrap();

        // Create an INDEX with task entries
        let index = format!(
            "| Log | Date | Trigger | Channel | Messages |\n|-----|------|---------|---------|----------|\n| 001 | {} | task-end-morning | task:morning | 3 |\n",
            today
        );
        fs::write(
            dir.path()
                .join("archives")
                .join("conversations")
                .join("INDEX.md"),
            index,
        )
        .unwrap();

        assert!(!needs_digest(dir.path()));
    }

    #[test]
    fn needs_digest_returns_true_when_tasks_exist_and_no_digest() {
        let dir = setup_test_dir();
        let today = Utc::now().format("%Y-%m-%d").to_string();

        let index = format!(
            "| Log | Date | Trigger | Channel | Messages |\n|-----|------|---------|---------|----------|\n| 001 | {} | task-end-morning | task:morning | 3 |\n",
            today
        );
        fs::write(
            dir.path()
                .join("archives")
                .join("conversations")
                .join("INDEX.md"),
            index,
        )
        .unwrap();

        assert!(needs_digest(dir.path()));
    }
}
