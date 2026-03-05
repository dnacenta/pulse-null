use console::style;

use echo_system_types::monitoring::{DocumentHealth, PipelineMonitor, ThresholdStatus};
use praxis_echo::runtime::PraxisMonitor;

use crate::config::Config;

pub async fn health_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    if !config.pipeline.enabled {
        println!("Pipeline monitoring is disabled.");
        return Ok(());
    }

    let monitor = PraxisMonitor::new();
    let thresholds = config.pipeline.to_thresholds();
    let pipeline_health = monitor.calculate(&root_dir, &thresholds);
    let state = monitor.load_state(&root_dir);

    println!();
    println!("  {}", style("Pipeline Health").bold());
    println!();

    print_doc_status("LEARNING", &pipeline_health.learning);
    print_doc_status("THOUGHTS", &pipeline_health.thoughts);
    print_doc_status("CURIOSITY", &pipeline_health.curiosity);
    print_doc_status("REFLECTIONS", &pipeline_health.reflections);
    print_doc_status("PRAXIS", &pipeline_health.praxis);

    if state.sessions_without_movement >= config.pipeline.freeze_threshold {
        println!();
        println!(
            "  {} Pipeline frozen — no movement for {} sessions.",
            style("!!").red().bold(),
            state.sessions_without_movement
        );
    }

    for warning in &pipeline_health.warnings {
        println!("  {} {}", style("!").yellow(), warning);
    }

    println!();
    Ok(())
}

fn print_doc_status(name: &str, doc: &DocumentHealth) {
    let status_color = match doc.status {
        ThresholdStatus::Green => style(format!("{}/{}", doc.count, doc.hard)).green(),
        ThresholdStatus::Yellow => style(format!("{}/{}", doc.count, doc.hard)).yellow(),
        ThresholdStatus::Red => style(format!("{}/{}", doc.count, doc.hard)).red().bold(),
    };

    let bar = status_bar(doc.count, doc.hard);
    println!("  {:<14} {} [{}]", name, status_color, bar,);
}

fn status_bar(count: usize, hard: usize) -> String {
    let width = 20;
    let filled = if hard > 0 {
        (count * width / hard).min(width)
    } else {
        0
    };
    let empty = width - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

pub async fn stale_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    if !config.pipeline.enabled {
        println!("Pipeline monitoring is disabled.");
        return Ok(());
    }

    println!();
    println!("  {}", style("Stale Items").bold());
    println!();

    // Check thoughts for staleness
    let thoughts_path = root_dir.join("journal/THOUGHTS.md");
    if thoughts_path.exists() {
        check_staleness(
            &thoughts_path,
            "THOUGHTS",
            config.pipeline.thoughts_staleness_days,
        )?;
    }

    // Check curiosity for staleness
    let curiosity_path = root_dir.join("journal/CURIOSITY.md");
    if curiosity_path.exists() {
        check_staleness(
            &curiosity_path,
            "CURIOSITY",
            config.pipeline.curiosity_staleness_days,
        )?;
    }

    println!();
    Ok(())
}

fn check_staleness(
    path: &std::path::Path,
    doc_name: &str,
    threshold_days: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = std::fs::metadata(path)?;
    let modified = metadata.modified()?;
    let elapsed = modified.elapsed().unwrap_or_default();
    let days = elapsed.as_secs() / 86400;

    if days >= threshold_days as u64 {
        println!(
            "  {} {} last modified {} days ago (threshold: {} days)",
            style("!").yellow(),
            doc_name,
            days,
            threshold_days,
        );
    } else {
        println!(
            "  {} {} modified {} days ago (ok)",
            style("✓").green(),
            doc_name,
            days,
        );
    }

    Ok(())
}
