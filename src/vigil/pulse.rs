use owo_colors::OwoColorize;

use super::state::{self, AlertLevel, Trend};

pub fn run() -> Result<(), String> {
    // Cooldown check
    let config = state::load_config()?;
    let pulse_state = state::load_pulse_state()?;
    if let Some(last) = &pulse_state.last_pulse {
        if let Some(last_epoch) = state::parse_iso_epoch(last) {
            let now = state::now_epoch_secs();
            if now.saturating_sub(last_epoch) < config.cooldown_seconds {
                return Ok(());
            }
        }
    }

    // Update pulse timestamp
    state::save_pulse_state(&state::PulseState {
        last_pulse: Some(state::now_iso()),
    })?;

    // Load analysis
    let analysis = match state::load_analysis()? {
        Some(a) => a,
        None => {
            println!("[VIGIL — Cognitive Health]\n");
            println!("No data yet. Signals will appear after the first `vigil-echo collect`.\n");
            println!("[END VIGIL]");
            return Ok(());
        }
    };

    // Format output
    let level_str = match &analysis.alert_level {
        AlertLevel::Healthy => format!("{}", "HEALTHY".green()),
        AlertLevel::Watch => format!("{}", "WATCH".yellow()),
        AlertLevel::Concern => format!("{}", "CONCERN".red()),
        AlertLevel::Alert => format!("{}", "ALERT".red().bold()),
    };

    println!("[VIGIL — Cognitive Health]\n");
    println!(
        "Overall: {} | {} improving, {} stable, {} declining",
        level_str, analysis.improving_count, analysis.stable_count, analysis.declining_count
    );

    if let Some(highlight) = &analysis.highlight {
        println!("Highlight: {}", highlight);
    }

    for msg in &analysis.watch_messages {
        println!("Watch: {}", msg);
    }

    // Show signal summary
    if !analysis.signals.is_empty() {
        println!();
        for (name, trend) in &analysis.signals {
            let arrow = match trend.trend {
                Trend::Improving => format!("{}", "↑".green()),
                Trend::Stable => "→".to_string(),
                Trend::Declining => format!("{}", "↓".red()),
            };
            let val = trend
                .current
                .map(|v| format!("{:.2}", v))
                .unwrap_or("—".to_string());
            println!("  {} {} {}", arrow, friendly_name(name), val);
        }
    }

    if analysis.data_points < 3 {
        println!(
            "\nCalibrating: {} data points collected (need 3+ for trends)",
            analysis.data_points
        );
    }

    println!("\n[END VIGIL]");

    Ok(())
}

fn friendly_name(name: &str) -> &str {
    match name {
        "vocabulary_diversity" => "vocabulary diversity",
        "question_generation" => "question generation",
        "thought_lifecycle" => "thought lifecycle",
        "evidence_grounding" => "evidence grounding",
        _ => name,
    }
}
