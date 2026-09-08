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

    // Flat layout (PN-104): the entity is created directly under base_dir,
    // e.g. ~/pulse-null/<name>. A pre-existing `entities/` subdirectory is
    // honoured so older trees keep their shape.
    let entities_dir = base_dir.join("entities");
    let target = if entities_dir.is_dir() {
        entities_dir
    } else {
        base_dir
    };
    std::fs::create_dir_all(&target)?;

    wizard::run(&target).await
}
