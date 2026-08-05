//! LOGBOOK rotation — prevents unbounded growth that causes death spirals.

use std::path::Path;

use chrono::Utc;

/// Maximum lines before rotation triggers.
const MAX_LINES: usize = 200;
/// Maximum bytes before rotation triggers. A line cap bounds entry *count*,
/// not size — one 200KB entry is still one line.
const MAX_BYTES: usize = 64 * 1024;
/// Lines to keep from the end after rotation.
const KEEP_TAIL: usize = 50;
/// Lines to keep from the start (title/header).
const KEEP_HEADER: usize = 3;
/// Byte share of MAX_BYTES the header may claim.
const HEADER_MAX_BYTES: usize = MAX_BYTES / 4;
/// Byte allowance reserved for the "rotated" note.
const ROTATION_NOTE_MAX_BYTES: usize = 128;

/// Take lines while their total size (including newlines) stays within budget.
fn take_within_budget<'a>(lines: impl Iterator<Item = &'a str>, budget: usize) -> Vec<&'a str> {
    let mut kept = Vec::new();
    let mut used = 0usize;
    for line in lines {
        let cost = line.len() + 1;
        if used + cost > budget {
            break;
        }
        used += cost;
        kept.push(line);
    }
    kept
}

/// Rotate LOGBOOK.md when it exceeds MAX_LINES or MAX_BYTES.
/// Archives the full file and keeps header + most recent KEEP_TAIL lines.
pub fn rotate_if_needed(root_dir: &Path, logbook_path: &Path) {
    let content = match std::fs::read_to_string(logbook_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= MAX_LINES && content.len() <= MAX_BYTES {
        return;
    }

    // Archive the full logbook
    let archive_dir = root_dir.join("archives");
    if let Err(e) = std::fs::create_dir_all(&archive_dir) {
        tracing::warn!("Failed to create archive directory: {}", e);
        return;
    }
    let now = Utc::now().format("%Y-%m-%d-%H%M");
    let archive_name = format!("LOGBOOK-rotated-{}.md", now);
    let archive_path = archive_dir.join(&archive_name);
    if let Err(e) = std::fs::write(&archive_path, &content) {
        tracing::error!("Failed to archive LOGBOOK for rotation: {}", e);
        return;
    }

    // Keep header + most recent entries, bounded by lines and by bytes.
    // The full content is archived above, so anything dropped here is
    // recoverable from the archive.
    let header = take_within_budget(lines.iter().take(KEEP_HEADER).copied(), HEADER_MAX_BYTES);
    let header_bytes: usize = header.iter().map(|l| l.len() + 1).sum();
    let tail_budget = MAX_BYTES.saturating_sub(header_bytes + ROTATION_NOTE_MAX_BYTES);
    let mut tail = take_within_budget(lines.iter().rev().take(KEEP_TAIL).copied(), tail_budget);
    tail.reverse();

    let mut rotated = header.join("\n");
    rotated.push_str(&format!(
        "\n\n<!-- Rotated {} — older entries in {} -->\n\n",
        Utc::now().format("%Y-%m-%d %H:%M UTC"),
        archive_name,
    ));
    rotated.push_str(&tail.join("\n"));
    rotated.push('\n');

    if let Err(e) = std::fs::write(logbook_path, rotated) {
        tracing::error!("Failed to write rotated LOGBOOK: {}", e);
    } else {
        tracing::info!(
            "LOGBOOK rotated: {} lines / {} bytes → {} lines (archived to {})",
            lines.len(),
            content.len(),
            header.len() + tail.len() + 2,
            archive_path.display()
        );
    }
}

/// Maximum task output files to retain.
const MAX_TASK_OUTPUT_FILES: usize = 50;

/// Write full task/intent output to task-output/ for queryable visibility.
/// Returns the path written, or None on failure.
pub fn write_task_output(
    root_dir: &Path,
    task_id: &str,
    task_name: &str,
    output: &str,
    tokens_in: u32,
    tokens_out: u32,
    tool_rounds: u32,
) -> Option<std::path::PathBuf> {
    if output.trim().is_empty() {
        return None;
    }

    let dir = root_dir.join("task-output");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::error!("Failed to create task-output dir: {}", e);
        return None;
    }

    let now = Utc::now();
    let filename = format!(
        "{}-{}.md",
        now.format("%Y%m%d-%H%M%S"),
        task_id
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            })
            .collect::<String>()
    );
    let path = dir.join(&filename);

    let content = format!(
        "---\ntask_id: {}\ntask_name: {}\ndate: {}\ntokens_in: {}\ntokens_out: {}\ntool_rounds: {}\n---\n\n{}\n",
        task_id,
        task_name,
        now.format("%Y-%m-%d %H:%M UTC"),
        tokens_in,
        tokens_out,
        tool_rounds,
        output,
    );

    if let Err(e) = std::fs::write(&path, &content) {
        tracing::error!("Failed to write task output to {}: {}", path.display(), e);
        return None;
    }

    // Prune old files if over limit
    prune_task_output(&dir, MAX_TASK_OUTPUT_FILES);

    Some(path)
}

/// Keep only the most recent `max` files in the directory.
fn prune_task_output(dir: &Path, max: usize) {
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
            .collect(),
        Err(_) => return,
    };

    if entries.len() <= max {
        return;
    }

    // Sort by filename (which starts with timestamp, so lexicographic = chronological)
    entries.sort_by_key(|e| e.file_name());

    let to_remove = entries.len() - max;
    for entry in entries.iter().take(to_remove) {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Write a structured LOGBOOK entry with consistent format.
///
/// All LOGBOOK entries follow: `### YYYY-MM-DD HH:MM UTC — {source} ({details})\n\n{summary}\n`
///
/// Summary is truncated to 500 chars if too long.
pub fn write_entry(root_dir: &Path, source: &str, details: &str, summary: &str) {
    let logbook_path = root_dir.join("journal/LOGBOOK.md");
    rotate_if_needed(root_dir, &logbook_path);

    let now = Utc::now();
    let truncated = if summary.len() > crate::utils::LOGBOOK_TRUNCATE_LEN {
        format!(
            "{}...",
            crate::utils::safe_truncate(summary, crate::utils::LOGBOOK_TRUNCATE_LEN)
        )
    } else {
        summary.to_string()
    };

    let entry = format!(
        "\n### {} — {} ({})\n\n{}\n",
        now.format("%Y-%m-%d %H:%M UTC"),
        source,
        details,
        truncated,
    );

    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&logbook_path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(entry.as_bytes())
        })
    {
        tracing::error!("Failed to write to LOGBOOK: {}", e);
    }
}

/// Write an automatic LOGBOOK entry when a session ends.
pub fn log_session_end(
    root_dir: &Path,
    channel: &str,
    message_count: usize,
    archive_path: Option<&Path>,
) {
    let archive_note = match archive_path {
        Some(p) => {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
            format!("Archived to {}.", name)
        }
        None => "No archive created.".to_string(),
    };
    write_entry(
        root_dir,
        "Session end",
        &format!("{}, {} messages", channel, message_count),
        &archive_note,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logbook_path(root: &Path) -> std::path::PathBuf {
        let journal = root.join("journal");
        std::fs::create_dir_all(&journal).unwrap();
        journal.join("LOGBOOK.md")
    }

    #[test]
    fn small_logbook_is_not_rotated() {
        let tmp = tempfile::tempdir().unwrap();
        let path = logbook_path(tmp.path());
        std::fs::write(&path, "# LOGBOOK\n\n### entry\n").unwrap();

        rotate_if_needed(tmp.path(), &path);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# LOGBOOK\n\n### entry\n"
        );
        assert!(!tmp.path().join("archives").exists());
    }

    /// PN-75: 200 lines is not 200 lines' worth of bytes — one runaway entry
    /// must still trigger rotation.
    #[test]
    fn oversized_logbook_rotates_on_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = logbook_path(tmp.path());

        let mut content = String::from("# LOGBOOK\n\n");
        content.push_str(&"g".repeat(MAX_BYTES + 1024));
        content.push('\n');
        std::fs::write(&path, &content).unwrap();

        rotate_if_needed(tmp.path(), &path);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.len() <= MAX_BYTES,
            "rotated LOGBOOK still {} bytes",
            after.len()
        );
        assert!(after.starts_with("# LOGBOOK"));
        assert!(after.contains("Rotated"));

        // The full content is preserved in the archive.
        let archives = std::fs::read_dir(tmp.path().join("archives")).unwrap();
        let archived: Vec<_> = archives.flatten().collect();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            std::fs::read_to_string(archived[0].path()).unwrap(),
            content
        );
    }
}
