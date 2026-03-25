//! LOGBOOK rotation — prevents unbounded growth that causes death spirals.

use std::path::Path;

use chrono::Utc;

/// Maximum lines before rotation triggers.
const MAX_LINES: usize = 200;
/// Lines to keep from the end after rotation.
const KEEP_TAIL: usize = 50;
/// Lines to keep from the start (title/header).
const KEEP_HEADER: usize = 3;

/// Rotate LOGBOOK.md when it exceeds MAX_LINES.
/// Archives the full file and keeps header + most recent KEEP_TAIL lines.
pub fn rotate_if_needed(root_dir: &Path, logbook_path: &Path) {
    let content = match std::fs::read_to_string(logbook_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= MAX_LINES {
        return;
    }

    // Archive the full logbook
    let archive_dir = root_dir.join("archives");
    let _ = std::fs::create_dir_all(&archive_dir);
    let now = Utc::now().format("%Y-%m-%d-%H%M");
    let archive_name = format!("LOGBOOK-rotated-{}.md", now);
    let archive_path = archive_dir.join(&archive_name);
    if let Err(e) = std::fs::write(&archive_path, &content) {
        tracing::error!("Failed to archive LOGBOOK for rotation: {}", e);
        return;
    }

    // Keep header + most recent entries
    let header: Vec<&str> = lines.iter().take(KEEP_HEADER).copied().collect();
    let tail: Vec<&str> = lines
        .iter()
        .rev()
        .take(KEEP_TAIL)
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

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
            "LOGBOOK rotated: {} lines → ~{} lines (archived to {})",
            lines.len(),
            KEEP_HEADER + KEEP_TAIL + 2,
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
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "md"))
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

/// Write an automatic LOGBOOK entry when a session ends.
pub fn log_session_end(
    root_dir: &Path,
    channel: &str,
    message_count: usize,
    archive_path: Option<&Path>,
) {
    let logbook_path = root_dir.join("journal/LOGBOOK.md");
    rotate_if_needed(root_dir, &logbook_path);

    let now = Utc::now();
    let archive_note = match archive_path {
        Some(p) => {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
            format!("Archived to {}.", name)
        }
        None => "No archive created.".to_string(),
    };

    let entry = format!(
        "\n### {} — Session end ({}, {} messages)\n\n{}\n",
        now.format("%Y-%m-%d %H:%M UTC"),
        channel,
        message_count,
        archive_note,
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
        tracing::error!("Failed to write session end to LOGBOOK: {}", e);
    }
}
