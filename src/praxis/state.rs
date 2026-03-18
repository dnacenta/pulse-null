use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    pub version: u32,
    pub last_pulse: Option<String>,
    pub last_review: Option<String>,
    pub last_archive: Option<String>,
    #[serde(default)]
    pub session_start_snapshot: Option<Snapshot>,
    #[serde(default)]
    pub pipeline: PipelineState,
    #[serde(default)]
    pub staleness: StalenessState,
    #[serde(default)]
    pub session_history: Vec<SessionRecord>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Snapshot {
    pub learning_threads: usize,
    pub active_thoughts: usize,
    pub open_questions: usize,
    pub observation_count: usize,
    pub active_policies: usize,
    pub session_log_entries: usize,
    #[serde(default)]
    pub document_hashes: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct PipelineState {
    pub last_movement: Option<String>,
    pub frozen_session_count: u32,
    pub total_graduations: u32,
    pub total_dissolutions: u32,
    pub total_archival_ops: u32,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct StalenessState {
    #[serde(default)]
    pub thoughts: HashMap<String, String>,
    #[serde(default)]
    pub questions: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SessionRecord {
    pub date: String,
    pub learning_delta: i32,
    pub thoughts_touched: i32,
    pub graduations: i32,
    pub dissolutions: i32,
    pub reflections_added: i32,
    pub pipeline_active: bool,
}

pub fn load(config: &super::PraxisConfig) -> Result<State, String> {
    let path = super::state_file(&config.claude_dir);
    load_from(&path)
}

pub fn load_from(path: &Path) -> Result<State, String> {
    if !path.exists() {
        return Ok(State {
            version: 1,
            ..Default::default()
        });
    }
    let content = fs::read_to_string(path).map_err(|e| format!("Failed to read state: {e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse state: {e}"))
}

pub fn save(state: &State, config: &super::PraxisConfig) -> Result<(), String> {
    let path = super::state_file(&config.claude_dir);
    save_to(state, &path)
}

pub fn save_to(state: &State, path: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| format!("Failed to serialize state: {e}"))?;
    fs::write(path, format!("{json}\n")).map_err(|e| format!("Failed to write state: {e}"))
}

pub fn now_iso() -> String {
    // Simple UTC timestamp without chrono dependency
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    // Convert to rough ISO format
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Calculate year/month/day from days since epoch
    let (year, month, day) = days_to_date(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

pub fn today_iso() -> String {
    let ts = now_iso();
    ts[..10].to_string()
}

fn days_to_date(days_since_epoch: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days_since_epoch + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}
