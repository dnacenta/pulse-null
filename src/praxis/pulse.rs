use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};

use super::parser;
use super::state;
use super::PraxisConfig;

/// Default soft and hard thresholds for standalone CLI: (soft, hard)
const LEARNING_THRESHOLD: (usize, usize) = (5, 8);
const THOUGHTS_THRESHOLD: (usize, usize) = (5, 10);
const CURIOSITY_THRESHOLD: (usize, usize) = (3, 7);
const REFLECTIONS_THRESHOLD: (usize, usize) = (15, 20);
const PRAXIS_THRESHOLD: (usize, usize) = (5, 10);

fn seconds_since_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn should_skip(last_pulse: &Option<String>, cooldown_secs: u64) -> bool {
    if let Some(ts) = last_pulse {
        if let Ok(last) = parse_iso_to_epoch(ts) {
            return seconds_since_epoch() - last < cooldown_secs;
        }
    }
    false
}

fn parse_iso_to_epoch(ts: &str) -> Result<u64, ()> {
    // Parse "YYYY-MM-DDThh:mm:ssZ" to rough epoch seconds
    if ts.len() < 19 {
        return Err(());
    }
    let year: u64 = ts[..4].parse().map_err(|_| ())?;
    let month: u64 = ts[5..7].parse().map_err(|_| ())?;
    let day: u64 = ts[8..10].parse().map_err(|_| ())?;
    let hour: u64 = ts[11..13].parse().map_err(|_| ())?;
    let min: u64 = ts[14..16].parse().map_err(|_| ())?;
    let sec: u64 = ts[17..19].parse().map_err(|_| ())?;
    // Rough approximation (good enough for cooldown checks)
    Ok(((year * 365 + month * 30 + day) * 86400) + hour * 3600 + min * 60 + sec)
}

fn threshold_label(count: usize, soft: usize, hard: usize) -> &'static str {
    if count >= hard {
        " ⚠ OVER LIMIT"
    } else if count >= soft {
        " ⚡ approaching limit"
    } else {
        ""
    }
}

/// Run pulse with explicit config (plugin context).
pub fn run_with_config(config: &PraxisConfig) -> Result<(), String> {
    let mut st = state::load(config)?;

    // Idempotency: skip if pulsed within cooldown window
    if should_skip(&st.last_pulse, config.pulse_cooldown_secs) {
        return Ok(());
    }

    let scan = parser::scan_with_config_and_staleness(config, config.thoughts_staleness_days);

    // Save session-start snapshot
    st.session_start_snapshot = Some(state::Snapshot {
        learning_threads: scan.learning.active,
        active_thoughts: scan.thoughts.active,
        open_questions: scan.curiosity.active,
        observation_count: scan.reflections.total,
        active_policies: scan.praxis.active,
        session_log_entries: scan.session_log_entries,
        document_hashes: scan.document_hashes.clone(),
    });

    st.last_pulse = Some(state::now_iso());
    state::save(&st, config)?;

    // Append pipeline snapshot to history log for calibration analysis
    append_history_snapshot(
        &config.claude_dir,
        &scan,
        st.last_pulse.as_deref().unwrap_or(""),
    );

    // Output pipeline state for agent context
    println!("[PRAXIS — Pipeline State]");

    // Document counts
    println!(
        "  LEARNING:    {} active threads{}",
        scan.learning.active,
        threshold_label(
            scan.learning.active,
            LEARNING_THRESHOLD.0,
            LEARNING_THRESHOLD.1
        )
    );
    println!(
        "  THOUGHTS:    {} active, {} graduated, {} dissolved{}",
        scan.thoughts.active,
        scan.thoughts.graduated,
        scan.thoughts.dissolved,
        threshold_label(
            scan.thoughts.active,
            THOUGHTS_THRESHOLD.0,
            THOUGHTS_THRESHOLD.1
        )
    );
    println!(
        "  CURIOSITY:   {} open, {} explored{}",
        scan.curiosity.active,
        scan.curiosity.explored,
        threshold_label(
            scan.curiosity.active,
            CURIOSITY_THRESHOLD.0,
            CURIOSITY_THRESHOLD.1
        )
    );
    println!(
        "  REFLECTIONS: {} total (observations: {}){}",
        scan.reflections.total,
        scan.reflections.active,
        threshold_label(
            scan.reflections.total,
            REFLECTIONS_THRESHOLD.0,
            REFLECTIONS_THRESHOLD.1
        )
    );
    println!(
        "  PRAXIS:      {} active, {} retired{}",
        scan.praxis.active,
        scan.praxis.graduated,
        threshold_label(scan.praxis.active, PRAXIS_THRESHOLD.0, PRAXIS_THRESHOLD.1)
    );

    // Reflection log
    if scan.session_log_entries > 0 {
        let date_range = match (&scan.session_log_oldest, &scan.session_log_newest) {
            (Some(old), Some(new)) => format!(" ({old} → {new})"),
            _ => String::new(),
        };
        println!(
            "  LOG:         {} entries{date_range}",
            scan.session_log_entries
        );
    }

    // Staleness warnings
    if !scan.stale_thoughts.is_empty() {
        println!();
        println!(
            "  ⚠ Stale thoughts (untouched >{} days):",
            config.thoughts_staleness_days
        );
        for t in &scan.stale_thoughts {
            let date = t
                .last_touched
                .as_ref()
                .or(t.started.as_ref())
                .map(|s| s.as_str())
                .unwrap_or("unknown");
            println!("    - {} (last: {date})", t.title);
        }
    }

    // Frozen pipeline warning
    if st.pipeline.frozen_session_count >= config.freeze_threshold {
        println!();
        println!(
            "  ⚠ Pipeline frozen — no movement in {} sessions. Ideas should flow.",
            st.pipeline.frozen_session_count
        );
    }

    println!("[END PRAXIS]");

    Ok(())
}

/// Maximum lines to retain in pipeline history.
const MAX_HISTORY_LINES: usize = 500;

/// Append a pipeline count snapshot to the history JSONL file.
fn append_history_snapshot(
    claude_dir: &std::path::Path,
    scan: &parser::PipelineScan,
    timestamp: &str,
) {
    let praxis_dir = claude_dir.join("praxis");
    let _ = fs::create_dir_all(&praxis_dir);
    let history_path = praxis_dir.join("pipeline-history.jsonl");

    let line = format!(
        "{{\"timestamp\":\"{}\",\"learning\":{},\"thoughts\":{},\"curiosity\":{},\"reflections\":{},\"praxis\":{}}}",
        timestamp,
        scan.learning.active,
        scan.thoughts.active,
        scan.curiosity.active,
        scan.reflections.total,
        scan.praxis.active,
    );

    // Append the new line
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_path)
    {
        let _ = writeln!(file, "{}", line);
    }

    // Trim to MAX_HISTORY_LINES if needed
    if let Ok(file) = fs::File::open(&history_path) {
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
        if lines.len() > MAX_HISTORY_LINES {
            let trimmed = &lines[lines.len() - MAX_HISTORY_LINES..];
            let _ = fs::write(&history_path, trimmed.join("\n") + "\n");
        }
    }
}
