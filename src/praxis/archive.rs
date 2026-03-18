use std::fs;

use super::parser;
use super::state;
use super::PraxisConfig;

const BOLD: &str = "\x1b[1m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

struct ArchiveCandidate {
    document: String,
    current: usize,
    hard_limit: usize,
    overflow: usize,
}

pub fn run(config: &PraxisConfig, dry_run: bool) -> Result<(), String> {
    let scan = parser::scan_with_config(config);

    let mut candidates: Vec<ArchiveCandidate> = Vec::new();

    // Check each document against hard limits
    if scan.learning.active > 8 {
        candidates.push(ArchiveCandidate {
            document: "LEARNING.md".to_string(),
            current: scan.learning.active,
            hard_limit: 8,
            overflow: scan.learning.active - 8,
        });
    }
    if scan.thoughts.active > 10 {
        candidates.push(ArchiveCandidate {
            document: "THOUGHTS.md".to_string(),
            current: scan.thoughts.active,
            hard_limit: 10,
            overflow: scan.thoughts.active - 10,
        });
    }
    if scan.curiosity.active > 7 {
        candidates.push(ArchiveCandidate {
            document: "CURIOSITY.md".to_string(),
            current: scan.curiosity.active,
            hard_limit: 7,
            overflow: scan.curiosity.active - 7,
        });
    }
    if scan.reflections.total > 20 {
        candidates.push(ArchiveCandidate {
            document: "REFLECTIONS.md".to_string(),
            current: scan.reflections.total,
            hard_limit: 20,
            overflow: scan.reflections.total - 20,
        });
    }
    if scan.praxis.active > 10 {
        candidates.push(ArchiveCandidate {
            document: "PRAXIS.md".to_string(),
            current: scan.praxis.active,
            hard_limit: 10,
            overflow: scan.praxis.active - 10,
        });
    }

    if candidates.is_empty() {
        println!("{GREEN}✓{RESET} All documents within thresholds. Nothing to archive.");
        return Ok(());
    }

    let mode = if dry_run { "DRY RUN" } else { "ARCHIVE" };
    println!("\n{BOLD}praxis-echo{RESET} — {mode}\n");

    for c in &candidates {
        println!(
            "  {YELLOW}⚠{RESET} {}: {}/{} ({} over limit)",
            c.document, c.current, c.hard_limit, c.overflow
        );
    }

    if dry_run {
        println!("\n  {YELLOW}Dry run{RESET} — no changes made.");
        println!("  Run without --dry-run to archive overflow content.");
        println!();
        return Ok(());
    }

    // Create archive directories and placeholder files
    let archives = super::archives_dir(&config.docs_dir);
    let today = state::today_iso();

    for c in &candidates {
        let sub = match c.document.as_str() {
            "LEARNING.md" => "learning",
            "THOUGHTS.md" => "thoughts",
            "CURIOSITY.md" => "curiosity",
            "REFLECTIONS.md" => "reflections",
            "PRAXIS.md" => "praxis",
            _ => continue,
        };
        let archive_dir = archives.join(sub);
        if !archive_dir.exists() {
            fs::create_dir_all(&archive_dir)
                .map_err(|e| format!("Failed to create archive dir: {e}"))?;
        }

        // Create an archive marker file — actual content migration is manual
        // because automatically removing markdown sections is fragile
        let marker = archive_dir.join(format!("archive-needed-{today}.md"));
        let content = format!(
            "# Archive Needed — {}\n\n\
             Date: {today}\n\
             Current count: {}\n\
             Hard limit: {}\n\
             Overflow: {} items need archiving\n\n\
             Review the document and move the oldest/most integrated items here.\n",
            c.document, c.current, c.hard_limit, c.overflow
        );
        fs::write(&marker, content).map_err(|e| format!("Failed to write archive marker: {e}"))?;
        println!(
            "  {GREEN}✓{RESET} Created archive marker: {}",
            marker.display()
        );
    }

    // Update state
    let mut st = state::load(config)?;
    st.last_archive = Some(state::now_iso());
    st.pipeline.total_archival_ops += 1;
    state::save(&st, config)?;

    println!("\n  Archive markers created. Review each document and move overflow content.\n");

    Ok(())
}
