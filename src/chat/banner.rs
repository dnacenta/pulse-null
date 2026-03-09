use std::path::Path;

use echo_system_types::monitoring::{
    CognitiveMonitor, CognitiveStatus, DocumentHealth, PipelineMonitor, ThresholdStatus, Trend,
};

use super::theme::Theme;
use crate::config::Config;

const LOGO: &str = r#"
  ╔═╗╦ ╦╦  ╔═╗╔═╗   ╔╗╔╦ ╦╦  ╦
  ╠═╝║ ║║  ╚═╗║╣    ║║║║ ║║  ║
  ╩  ╚═╝╩═╝╚═╝╚═╝───╝╚╝╚═╝╩═╝╩═╝"#;

/// Render the startup banner to stdout.
pub fn render(config: &Config, root_dir: &Path, plugin_count: usize) {
    let theme = Theme::nord();
    render_with_theme(config, root_dir, plugin_count, &theme);
}

/// Render the startup banner with a specific theme.
pub fn render_with_theme(config: &Config, root_dir: &Path, plugin_count: usize, theme: &Theme) {
    let version = env!("CARGO_PKG_VERSION");
    let r = theme.reset;

    // Logo lines (skip first empty line)
    let logo_lines: Vec<&str> = LOGO.lines().skip(1).collect();

    // Metadata lines
    let mk = theme.meta_key;
    let mv = theme.meta_value;
    let meta_lines = [
        format!("{mk}entity  {r}{mv}{}{r}", config.entity.name),
        format!("{mk}user    {r}{mv}{}{r}", config.entity.owner_alias),
        format!("{mk}model   {r}{mv}{}{r}", config.llm.model),
        format!(
            "{mk}plugins {r}{mv}{}{r}",
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
            println!(
                "{}{:<width$}{r}{}",
                theme.logo,
                logo_line,
                meta_lines[i],
                width = logo_width
            );
        } else {
            println!("{}{}{r}", theme.logo, logo_line);
        }
    }

    // Version line below logo
    println!("  {}v{}{}", theme.version, version, r);

    println!("{}{}{}", theme.separator, SEPARATOR, r);

    // Pipeline health
    if config.pipeline.enabled {
        let monitor = praxis_echo::runtime::PraxisMonitor::new();
        render_pipeline(root_dir, config, &monitor, theme);
    }

    // Cognitive health
    if config.monitoring.enabled {
        let monitor = vigil_echo::runtime::VigilMonitor::new();
        render_vigil(root_dir, config, &monitor, theme);
    }

    println!();
}

const SEPARATOR: &str = "  ─────────────────────────────────────────────────────────────";

/// Render pipeline document progress bars.
fn render_pipeline(root_dir: &Path, config: &Config, monitor: &dyn PipelineMonitor, theme: &Theme) {
    let thresholds = config.pipeline.to_thresholds();
    let health = monitor.calculate(root_dir, &thresholds);

    println!();

    print_doc_bar("learning", &health.learning, theme);
    print_doc_bar("thoughts", &health.thoughts, theme);
    print_doc_bar("curiosity", &health.curiosity, theme);
    print_doc_bar("reflections", &health.reflections, theme);
    print_doc_bar("praxis", &health.praxis, theme);

    for warning in &health.warnings {
        println!("  {}!{} {}", theme.warning, theme.reset, warning);
    }
}

/// Print a single document progress bar with color.
fn print_doc_bar(name: &str, doc: &DocumentHealth, theme: &Theme) {
    let bar = status_bar(doc.count, doc.hard);
    let count_label = format!("{}/{}", doc.count, doc.hard);
    let r = theme.reset;

    let color = match doc.status {
        ThresholdStatus::Green => theme.bar_green,
        ThresholdStatus::Yellow => theme.bar_yellow,
        ThresholdStatus::Red => theme.bar_red,
    };

    let status_word = match doc.status {
        ThresholdStatus::Green => "ok",
        ThresholdStatus::Yellow => "warning",
        ThresholdStatus::Red => "full",
    };

    println!(
        "  {:<14} {}{}  {:<6}{r}  {}",
        name, color, bar, count_label, status_word
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
fn render_vigil(root_dir: &Path, config: &Config, monitor: &dyn CognitiveMonitor, theme: &Theme) {
    let r = theme.reset;
    let health = monitor.assess(
        root_dir,
        config.monitoring.window_size,
        config.monitoring.min_samples,
    );

    println!("{}{}{}", theme.separator, SEPARATOR, r);

    if !health.sufficient_data {
        println!("  {}cognitive health  awaiting data{}", theme.dim, r);
        return;
    }

    let status_display = match health.status {
        CognitiveStatus::Healthy => {
            format!("{}HEALTHY{}", theme.status_healthy, r)
        }
        CognitiveStatus::Watch => {
            format!("{}WATCH{}", theme.status_watch, r)
        }
        CognitiveStatus::Concern => {
            format!("{}CONCERN{}", theme.status_watch, r)
        }
        CognitiveStatus::Alert => {
            format!("{}ALERT{}", theme.status_alert, r)
        }
    };

    println!("  cognitive health  {}", status_display);

    let vocab = trend_arrow(&health.vocabulary_trend, theme);
    let questions = trend_arrow(&health.question_trend, theme);
    let grounding = trend_arrow(&health.evidence_trend, theme);
    let lifecycle = trend_arrow(&health.progress_trend, theme);

    println!(
        "  vocabulary {}  questions {}  grounding {}  lifecycle {}",
        vocab, questions, grounding, lifecycle
    );
}

/// Convert a trend to a colored arrow character.
fn trend_arrow(trend: &Trend, theme: &Theme) -> String {
    let r = theme.reset;
    match trend {
        Trend::Improving => format!("{}▲{}", theme.trend_up, r),
        Trend::Stable => "─".to_string(),
        Trend::Declining => format!("{}▼{}", theme.trend_down, r),
    }
}
