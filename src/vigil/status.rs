use owo_colors::OwoColorize;

use super::state::{self, AlertLevel, Analysis, Config, SignalVector, Trend};
use super::stats;

const SIGNAL_NAMES: [&str; 4] = [
    "vocabulary_diversity",
    "question_generation",
    "thought_lifecycle",
    "evidence_grounding",
];

const SPARKLINE_WIDTH: usize = 20;

pub fn run(json_output: bool) -> Result<(), String> {
    let config = state::load_config()?;
    let history = state::load_signals()?;
    let analysis = state::load_analysis()?;

    if json_output {
        return print_json(&config, &history, &analysis);
    }

    print_dashboard(&config, &history, &analysis)
}

fn print_dashboard(
    config: &Config,
    history: &[SignalVector],
    analysis: &Option<Analysis>,
) -> Result<(), String> {
    // Header
    println!();
    println!("  {} — cognitive health dashboard", "vigil-echo".bold());
    println!();

    // Status line
    print_status_line(analysis, history.len(), config.window_size);

    // Signals with sparklines
    println!();
    println!("  {}", "Signals".bold());
    if history.is_empty() {
        println!("    No signals collected yet. Run `vigil-echo collect` after a session.");
    } else {
        for &name in &SIGNAL_NAMES {
            print_signal_row(name, history, analysis);
        }
    }

    // Statistics
    if history.len() >= 3 {
        println!();
        println!("  {}", "Statistics".bold());
        for &name in &SIGNAL_NAMES {
            print_stats_row(name, history);
        }
    }

    // Anomalies
    let anomalies = detect_anomalies(history);
    if !anomalies.is_empty() {
        println!();
        println!("  {}", "Anomalies".bold());
        for anomaly in &anomalies {
            println!("    {} {anomaly}", "*".yellow());
        }
    }

    // Alerts from analysis
    if let Some(analysis) = analysis {
        if !analysis.watch_messages.is_empty() {
            println!();
            println!("  {}", "Alerts".bold());
            for msg in &analysis.watch_messages {
                println!("    {} {msg}", "!".yellow().bold());
            }
        }
        if let Some(highlight) = &analysis.highlight {
            println!();
            println!("  {} {highlight}", "✦".green());
        }
    }

    // Config
    println!();
    println!("  {}", "Config".bold());
    println!(
        "    Window: {} | Max history: {} | Cooldown: {}s",
        config.window_size, config.max_history, config.cooldown_seconds
    );
    println!();

    Ok(())
}

fn print_status_line(analysis: &Option<Analysis>, data_points: usize, window: usize) {
    let level = if let Some(a) = analysis {
        match &a.alert_level {
            AlertLevel::Healthy => format!("{}", "HEALTHY".green()),
            AlertLevel::Watch => format!("{}", "WATCH".yellow()),
            AlertLevel::Concern => format!("{}", "CONCERN".red()),
            AlertLevel::Alert => format!("{}", "ALERT".red().bold()),
        }
    } else {
        format!("{}", "NO DATA".dimmed())
    };

    let counts = if let Some(a) = analysis {
        format!(
            " | {} improving, {} stable, {} declining",
            a.improving_count, a.stable_count, a.declining_count
        )
    } else {
        String::new()
    };

    println!("  Status: {level}    {data_points} data points | window: {window}{counts}");
}

fn print_signal_row(name: &str, history: &[SignalVector], analysis: &Option<Analysis>) {
    let series = stats::signal_series(history, name);
    let current = series.last().copied();
    let spark = stats::sparkline(&series, SPARKLINE_WIDTH);

    // Color the value based on health thresholds
    let val_str = match current {
        Some(v) => {
            let formatted = format!("{:.2}", v);
            match signal_zone(name, v) {
                Zone::Healthy => format!("{}", formatted.green()),
                Zone::Watch => format!("{}", formatted.yellow()),
                Zone::Concern => format!("{}", formatted.red()),
            }
        }
        None => format!("{}", "--".dimmed()),
    };

    // Trend arrow and delta from analysis
    let (arrow, delta_str) = if let Some(analysis) = analysis {
        if let Some(trend) = analysis.signals.get(name) {
            let arrow = match trend.trend {
                Trend::Improving => format!("{}", "↑".green()),
                Trend::Stable => format!("{}", "→".dimmed()),
                Trend::Declining => format!("{}", "↓".red()),
            };
            (arrow, format!("{:+.2}", trend.delta))
        } else {
            (format!("{}", "?".dimmed()), String::new())
        }
    } else {
        (format!("{}", "?".dimmed()), String::new())
    };

    // Rarity indicator
    let rarity = if let Some(v) = current {
        if let (Some(m), Some(sd)) = (stats::mean(&series), stats::std_dev(&series)) {
            if sd > f64::EPSILON {
                let z = stats::z_score(v, m, sd);
                if z.abs() >= 2.0 {
                    format!("{}", "**".red())
                } else if z.abs() >= 1.0 {
                    format!("{}", "*".yellow())
                } else {
                    "  ".to_string()
                }
            } else {
                "  ".to_string()
            }
        } else {
            "  ".to_string()
        }
    } else {
        "  ".to_string()
    };

    println!(
        "    {:<24} {:>6}  {}  {} {:>6} {}",
        friendly_name(name),
        val_str,
        spark,
        arrow,
        delta_str,
        rarity,
    );
}

fn print_stats_row(name: &str, history: &[SignalVector]) {
    let series = stats::signal_series(history, name);
    if series.is_empty() {
        return;
    }

    let m = stats::mean(&series).unwrap_or(0.0);
    let sd = stats::std_dev(&series).unwrap_or(0.0);
    let current = series.last().copied().unwrap_or(0.0);
    let pctl = stats::percentile_rank(current, &series);
    let (streak_dir, streak_count) = stats::streak(&series);

    let streak_sym = match streak_dir {
        1 => "↑",
        -1 => "↓",
        _ => "=",
    };

    println!(
        "    {:<24} mean {:.2}  sd {:.2}  pctl {:>3.0}%  streak {}{:>2}",
        friendly_name(name),
        m,
        sd,
        pctl,
        streak_sym,
        streak_count,
    );
}

fn detect_anomalies(history: &[SignalVector]) -> Vec<String> {
    let mut anomalies = Vec::new();
    for &name in &SIGNAL_NAMES {
        let series = stats::signal_series(history, name);
        if series.len() < 5 {
            continue;
        }
        let current = match series.last() {
            Some(v) => *v,
            None => continue,
        };
        let m = match stats::mean(&series) {
            Some(v) => v,
            None => continue,
        };
        let sd = match stats::std_dev(&series) {
            Some(v) if v > f64::EPSILON => v,
            _ => continue,
        };
        let z = stats::z_score(current, m, sd);
        let pctl = stats::percentile_rank(current, &series);

        if z.abs() >= 2.0 {
            let direction = if z > 0.0 { "above" } else { "below" };
            anomalies.push(format!(
                "{} current reading ({:.2}) is {:.1} std devs {} mean ({}th percentile)",
                friendly_name(name),
                current,
                z.abs(),
                direction,
                pctl as usize,
            ));
        }
    }
    anomalies
}

// --- JSON output ---

fn print_json(
    config: &Config,
    history: &[SignalVector],
    analysis: &Option<Analysis>,
) -> Result<(), String> {
    let mut output = serde_json::Map::new();

    // Per-signal stats
    let mut signals_json = serde_json::Map::new();
    for &name in &SIGNAL_NAMES {
        let series = stats::signal_series(history, name);
        let mut sig = serde_json::Map::new();

        let current = series.last().copied();
        sig.insert("current".into(), json_opt(current));
        sig.insert("mean".into(), json_opt(stats::mean(&series)));
        sig.insert("std_dev".into(), json_opt(stats::std_dev(&series)));
        sig.insert(
            "sparkline".into(),
            serde_json::Value::String(stats::sparkline(&series, SPARKLINE_WIDTH)),
        );

        if let Some(v) = current {
            if let Some(n) = serde_json::Number::from_f64(stats::percentile_rank(v, &series)) {
                sig.insert("percentile".into(), serde_json::Value::Number(n));
            }
            if let (Some(m), Some(sd)) = (stats::mean(&series), stats::std_dev(&series)) {
                if sd > f64::EPSILON {
                    if let Some(n) = serde_json::Number::from_f64(stats::z_score(v, m, sd)) {
                        sig.insert("z_score".into(), serde_json::Value::Number(n));
                    }
                }
            }
            let zone = match signal_zone(name, v) {
                Zone::Healthy => "healthy",
                Zone::Watch => "watch",
                Zone::Concern => "concern",
            };
            sig.insert("health_zone".into(), serde_json::Value::String(zone.into()));
        }

        let (streak_dir, streak_count) = stats::streak(&series);
        sig.insert(
            "streak_direction".into(),
            serde_json::Value::Number(serde_json::Number::from(streak_dir as i64)),
        );
        sig.insert(
            "streak_count".into(),
            serde_json::Value::Number(serde_json::Number::from(streak_count as u64)),
        );

        if let Some(analysis) = analysis {
            if let Some(trend) = analysis.signals.get(name) {
                sig.insert(
                    "trend".into(),
                    serde_json::Value::String(format!("{:?}", trend.trend)),
                );
                if let Some(n) = serde_json::Number::from_f64(trend.delta) {
                    sig.insert("delta".into(), serde_json::Value::Number(n));
                }
            }
        }

        signals_json.insert(name.into(), serde_json::Value::Object(sig));
    }
    output.insert("signals".into(), serde_json::Value::Object(signals_json));

    // Alert level
    if let Some(analysis) = analysis {
        output.insert(
            "alert_level".into(),
            serde_json::Value::String(format!("{:?}", analysis.alert_level)),
        );
        output.insert(
            "data_points".into(),
            serde_json::Value::Number(serde_json::Number::from(analysis.data_points as u64)),
        );
        output.insert(
            "watch_messages".into(),
            serde_json::Value::Array(
                analysis
                    .watch_messages
                    .iter()
                    .map(|m| serde_json::Value::String(m.clone()))
                    .collect(),
            ),
        );
    }

    // Anomalies
    let anomalies = detect_anomalies(history);
    output.insert(
        "anomalies".into(),
        serde_json::Value::Array(
            anomalies
                .iter()
                .map(|a| serde_json::Value::String(a.clone()))
                .collect(),
        ),
    );

    // Config
    let mut cfg = serde_json::Map::new();
    cfg.insert(
        "window_size".into(),
        serde_json::Value::Number(serde_json::Number::from(config.window_size as u64)),
    );
    cfg.insert(
        "max_history".into(),
        serde_json::Value::Number(serde_json::Number::from(config.max_history as u64)),
    );
    output.insert("config".into(), serde_json::Value::Object(cfg));

    let json_str = serde_json::to_string_pretty(&serde_json::Value::Object(output))
        .map_err(|e| format!("JSON serialization failed: {e}"))?;
    println!("{json_str}");

    Ok(())
}

fn json_opt(opt: Option<f64>) -> serde_json::Value {
    opt.and_then(serde_json::Number::from_f64)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}

// --- Helpers ---

enum Zone {
    Healthy,
    Watch,
    Concern,
}

fn signal_zone(name: &str, value: f64) -> Zone {
    let (red_below, yellow_below) = match name {
        "vocabulary_diversity" => (0.25, 0.40),
        "question_generation" => (2.0, 4.0),
        "thought_lifecycle" => (0.15, 0.30),
        "evidence_grounding" => (0.40, 0.60),
        _ => (0.25, 0.50),
    };
    if value < red_below {
        Zone::Concern
    } else if value < yellow_below {
        Zone::Watch
    } else {
        Zone::Healthy
    }
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
