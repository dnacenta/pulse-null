use std::path::PathBuf;

use console::style;

use crate::init::wizard;

pub async fn run(dir: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let base_dir = match dir {
        Some(d) => PathBuf::from(d),
        None => std::env::current_dir()?,
    };

    // Legacy check: if CWD already has pulse-null.toml, warn and exit
    if base_dir.join("pulse-null.toml").exists() {
        println!();
        println!(
            "  {} This directory already contains a pulse-null.toml (single-entity mode).",
            style("⚠").yellow()
        );
        println!("  To create entities in multi-entity mode, run from a parent directory.");
        println!(
            "  Or use {} to target a different location.",
            style("pulse-null init --dir /path/to/project").cyan()
        );
        println!();
        return Ok(());
    }

    // Multi-entity is the default: create entities/ dir
    let entities_dir = base_dir.join("entities");
    if !entities_dir.exists() {
        std::fs::create_dir_all(&entities_dir)?;
    }

    wizard::run(&entities_dir).await
}
