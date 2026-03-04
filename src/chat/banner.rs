use std::path::Path;

use praxis_echo::runtime::{self as pipeline, ThresholdStatus};
use vigil_echo::runtime::{self as vigil, CognitiveStatus, Trend};

use crate::config::Config;

const LOGO: &str = r#"
  ╔═╗╦ ╦╦  ╔═╗╔═╗   ╔╗╔╦ ╦╦  ╦
  ╠═╝║ ║║  ╚═╗║╣    ║║║║ ║║  ║
  ╩  ╚═╝╩═╝╚═╝╚═╝───╝╚╝╚═╝╩═╝╩═╝"#;

const SEPARATOR: &str = "  ─────────────────────────────────────────────────────────────";

/// Render the startup banner to stdout.
pub fn render(config: &Config, root_dir: &Path, plugin_count: usize) {
    let version = env!("CARGO_PKG_VERSION");

    // Logo lines (skip first empty line)
    let logo_lines: Vec<&str> = LOGO.lines().skip(1).collect();

    // Metadata lines aligned to the right of the logo
    let meta_lines = [
        format!("entity  {}", config.entity.name),
        format!("user    {}", config.entity.owner_alias),
        format!("model   {}", config.llm.model),
        format!(
            "plugins {}",
            if plugin_count > 0 {
                format!("{} configured", plugin_count)
            } else {
                "none".to_string()
            }
        ),
    ];

    println!();

    // Print logo + metadata side by side
    let logo_width = 40;
    for (i, logo_line) in logo_lines.iter().enumerate() {
        if i < meta_lines.len() {
            println!("{:<width$}{}", logo_line, meta_lines[i], width = logo_width);
        } else {
            println!("{}", logo_line);
        }
    }

    // Version line below logo
    println!("  v{}", version);

    println!("{}", SEPARATOR);

    // Pipeline health
    if config.pipeline.enabled {
        render_pipeline(root_dir, config);
    }

    // Cognitive health
    if config.monitoring.enabled {
        render_vigil(root_dir, config);
    }

    println!();
}

/// Render pipeline document progress bars.
fn render_pipeline(root_dir: &Path, config: &Config) {
    let thresholds = config.pipeline.to_thresholds();
    let health = pipeline::calculate(root_dir, &thresholds);

    println!();

    print_doc_bar("learning", &health.learning);
    print_doc_bar("thoughts", &health.thoughts);
    print_doc_bar("curiosity", &health.curiosity);
    print_doc_bar("reflections", &health.reflections);
    print_doc_bar("praxis", &health.praxis);

    for warning in &health.warnings {
        println!("  \x1b[33m!\x1b[0m {}", warning);
    }
}

/// Print a single document progress bar with color.
fn print_doc_bar(name: &str, doc: &pipeline::DocumentHealth) {
    let bar = status_bar(doc.count, doc.hard);
    let count_label = format!("{}/{}", doc.count, doc.hard);

    let (color_start, color_end) = match doc.status {
        ThresholdStatus::Green => ("\x1b[32m", "\x1b[0m"),
        ThresholdStatus::Yellow => ("\x1b[33m", "\x1b[0m"),
        ThresholdStatus::Red => ("\x1b[31m", "\x1b[0m"),
    };

    let status_word = match doc.status {
        ThresholdStatus::Green => "ok",
        ThresholdStatus::Yellow => "warning",
        ThresholdStatus::Red => "full",
    };

    println!(
        "  {:<14} {}{}  {:<6}{}  {}",
        name, color_start, bar, count_label, color_end, status_word
    );
}

/// Build a progress bar string.
fn status_bar(count: usize, hard: usize) -> String {
    let width = 10;
    let filled = if hard > 0 {
        (count * width / hard).min(width)
    } else {
        0
    };
    let empty = width - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

/// Render cognitive health signals.
fn render_vigil(root_dir: &Path, config: &Config) {
    let health = vigil::assess(
        root_dir,
        config.monitoring.window_size,
        config.monitoring.min_samples,
    );

    println!("{}", SEPARATOR);

    if !health.sufficient_data {
        println!("  \x1b[2mcognitive health  awaiting data\x1b[0m");
        return;
    }

    let status_display = match health.status {
        CognitiveStatus::Healthy => "\x1b[32mHEALTHY\x1b[0m",
        CognitiveStatus::Watch => "\x1b[33mWATCH\x1b[0m",
        CognitiveStatus::Concern => "\x1b[33mCONCERN\x1b[0m",
        CognitiveStatus::Alert => "\x1b[31mALERT\x1b[0m",
    };

    println!("  cognitive health  {}", status_display);

    let vocab = trend_arrow(&health.vocabulary_trend);
    let questions = trend_arrow(&health.question_trend);
    let grounding = trend_arrow(&health.evidence_trend);
    let lifecycle = trend_arrow(&health.progress_trend);

    println!(
        "  vocabulary {}  questions {}  grounding {}  lifecycle {}",
        vocab, questions, grounding, lifecycle
    );
}

/// Convert a trend to a colored arrow character.
fn trend_arrow(trend: &Trend) -> &'static str {
    match trend {
        Trend::Improving => "\x1b[32m▲\x1b[0m",
        Trend::Stable => "─",
        Trend::Declining => "\x1b[31m▼\x1b[0m",
    }
}
