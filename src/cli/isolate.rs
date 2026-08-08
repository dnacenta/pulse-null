//! `pulse-null isolate` — enter/exit/inspect Isolation Mode from the shell.
//!
//! Writes the marker file directly, so it works with the server down, the
//! coordinator wedged, or anything in between (coordinator spec, decision 5:
//! the trigger must not route through the thing that might be wedged). A
//! running server notices the marker on the next request — no restart needed.

use console::style;

use crate::config::Config;
use crate::server::isolation;

pub async fn on(reason: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    let already = isolation::is_active(&root_dir);
    let marker = isolation::enter(&root_dir, "cli", reason)?;
    if already {
        println!(
            "  {} already active (entered {} by {}).",
            style("ISOLATION").red().bold(),
            marker.entered_at.format("%Y-%m-%d %H:%M:%SZ"),
            marker.by
        );
    } else {
        println!(
            "  {} entered. Minimal core only — the running entity sheds the \
             coordinator, scheduler, and all state writes at its next turn.",
            style("ISOLATION").red().bold()
        );
        println!("  Exit with: pulse-null isolate off  (or /resume over chat)");
    }
    Ok(())
}

pub async fn off() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    if isolation::exit(&root_dir)? {
        println!("  {}", style(isolation::BACK_TO_NORMAL).green());
    } else {
        println!("  Not in isolation mode — nothing to resume.");
    }
    Ok(())
}

pub async fn status() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    match isolation::status(&root_dir) {
        Some(marker) => {
            println!(
                "  {} active — entered {} by {}{}",
                style("ISOLATION").red().bold(),
                marker.entered_at.format("%Y-%m-%d %H:%M:%SZ"),
                marker.by,
                marker.reason.map(|r| format!(" ({r})")).unwrap_or_default()
            );
        }
        None => println!("  Normal operation — not isolated."),
    }
    Ok(())
}
