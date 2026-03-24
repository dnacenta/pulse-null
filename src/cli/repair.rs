use console::style;

use crate::config::Config;
use crate::init::claude_code_bootstrap::{self, ItemStatus};

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    if config.llm.provider != "claude-code" {
        println!(
            "  Provider is '{}', not 'claude-code'. Nothing to repair.",
            config.llm.provider
        );
        return Ok(());
    }

    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| "Could not determine home directory")?;

    println!();
    println!("  {}", style("Checking Claude Code integration...").bold());
    println!();

    let results = claude_code_bootstrap::ensure(&root_dir, &home);

    let mut created = 0;
    let mut existing = 0;
    let mut skipped = 0;

    for item in &results {
        println!("  {item}");
        match &item.status {
            ItemStatus::Created => created += 1,
            ItemStatus::Exists => existing += 1,
            ItemStatus::Skipped(_) => skipped += 1,
            _ => {}
        }
    }

    println!();
    if created > 0 {
        println!(
            "  {} Repaired {} item(s). {} already ok.",
            style("✓").green().bold(),
            created,
            existing
        );
    } else {
        println!(
            "  {} Everything looks good. {} item(s) verified.",
            style("✓").green().bold(),
            existing
        );
    }
    if skipped > 0 {
        println!(
            "  {} {} item(s) skipped — check warnings above.",
            style("⚠").yellow(),
            skipped
        );
    }
    println!();

    Ok(())
}
