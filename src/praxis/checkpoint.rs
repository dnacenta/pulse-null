use std::fs;

use super::parser;
use super::state;
use super::PraxisConfig;

pub fn run(config: &PraxisConfig) -> Result<(), String> {
    let scan = parser::scan_with_config(config);

    // Build checkpoint data
    let checkpoint = serde_json::json!({
        "timestamp": state::now_iso(),
        "trigger": "precompact",
        "learning_threads": scan.learning.active,
        "active_thoughts": scan.thoughts.active,
        "graduated_thoughts": scan.thoughts.graduated,
        "dissolved_thoughts": scan.thoughts.dissolved,
        "open_questions": scan.curiosity.active,
        "explored_questions": scan.curiosity.explored,
        "reflections_total": scan.reflections.total,
        "active_policies": scan.praxis.active,
        "session_log_entries": scan.session_log_entries,
        "document_hashes": scan.document_hashes,
    });

    // Find next checkpoint number
    let cp_dir = super::checkpoints_dir(&config.claude_dir);
    if !cp_dir.exists() {
        fs::create_dir_all(&cp_dir)
            .map_err(|e| format!("Failed to create checkpoints dir: {e}"))?;
    }

    let mut max_num = 0u32;
    if let Ok(entries) = fs::read_dir(&cp_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(num_str) = name
                .strip_prefix("checkpoint-")
                .and_then(|s| s.strip_suffix(".json"))
            {
                if let Ok(n) = num_str.parse::<u32>() {
                    max_num = max_num.max(n);
                }
            }
        }
    }

    let next = max_num + 1;
    let filename = format!("checkpoint-{:03}.json", next);
    let filepath = cp_dir.join(&filename);

    let json = serde_json::to_string_pretty(&checkpoint)
        .map_err(|e| format!("Failed to serialize checkpoint: {e}"))?;
    fs::write(&filepath, format!("{json}\n"))
        .map_err(|e| format!("Failed to write checkpoint: {e}"))?;

    println!(
        "[PRAXIS — Checkpoint #{:03} saved to {}]",
        next,
        filepath.display()
    );

    Ok(())
}
