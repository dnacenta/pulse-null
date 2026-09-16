use console::style;

use crate::config::Config;
use crate::init::claude_code_bootstrap::{self, printable, ItemStatus};

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

    println!();
    println!(
        "  {}",
        style(format!(
            "Checking Claude Code integration for {}...",
            root_dir.display()
        ))
        .bold()
    );
    println!();

    let results = claude_code_bootstrap::ensure(&root_dir);

    let mut changed = 0;
    let mut existing = 0;
    let mut skipped = 0;

    for item in &results {
        println!("  {item}");
        match &item.status {
            ItemStatus::Created | ItemStatus::Updated => changed += 1,
            ItemStatus::Exists => existing += 1,
            ItemStatus::Skipped(_) => skipped += 1,
            _ => {}
        }
    }

    // Leftovers of the pre-PN-104 layout: `$HOME/.claude` symlinks into this
    // entity are removed (they are ours), user-level recall-echo hooks that
    // carry no entity root are only reported (that file is the user's).
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if let Some(home) = home {
        let legacy = claude_code_bootstrap::legacy_home_links(&root_dir, &home);
        if !legacy.is_empty() {
            println!();
            println!(
                "  {}",
                style("Retiring legacy user-level links into this entity:").bold()
            );
            for link in &legacy {
                match std::fs::remove_file(link) {
                    Ok(()) => {
                        changed += 1;
                        println!(
                            "    {} {} removed",
                            style("✓").green(),
                            printable(&link.display().to_string())
                        );
                    }
                    Err(e) => {
                        skipped += 1;
                        println!(
                            "    {} {} could not be removed: {e}",
                            style("⚠").yellow(),
                            printable(&link.display().to_string())
                        );
                    }
                }
            }
        }

        let stale = claude_code_bootstrap::user_hooks_missing_root(&home);
        if !stale.is_empty() {
            let settings = home.join(".claude/settings.json");
            println!();
            println!(
                "  {} {} has recall-echo hooks without an entity root.",
                style("⚠").yellow(),
                settings.display()
            );
            println!("    They fire for every entity this user runs and resolve to the wrong one.");
            println!("    The entity now carries its own hooks in .claude/settings.json — remove these by hand:");
            for command in &stale {
                println!("      {}", printable(command));
            }
        }
    }

    println!();
    if changed > 0 {
        println!(
            "  {} Repaired {} item(s). {} already ok.",
            style("✓").green().bold(),
            changed,
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
