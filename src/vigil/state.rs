use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// A single signal vector collected at a point in time.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SignalVector {
    pub timestamp: String,
    pub trigger: String,
    pub signals: Signals,
    #[serde(default)]
    pub document_hashes: HashMap<String, String>,
}

/// The 4 Phase-1 signals. Null means document was missing.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Signals {
    pub vocabulary_diversity: Option<f64>,
    pub question_generation: Option<f64>,
    pub thought_lifecycle: Option<f64>,
    pub evidence_grounding: Option<f64>,
}

/// Per-signal trend direction.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Trend {
    Improving,
    Stable,
    Declining,
}

/// Per-signal trend info.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SignalTrend {
    pub current: Option<f64>,
    pub trend: Trend,
    pub delta: f64,
}

/// Alert level.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum AlertLevel {
    Healthy,
    Watch,
    Concern,
    Alert,
}

/// Analysis result written to analysis.json.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Analysis {
    pub timestamp: String,
    pub alert_level: AlertLevel,
    pub signals: HashMap<String, SignalTrend>,
    pub improving_count: usize,
    pub stable_count: usize,
    pub declining_count: usize,
    pub highlight: Option<String>,
    pub watch_messages: Vec<String>,
    pub data_points: usize,
}

/// Configuration with thresholds.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub thresholds: HashMap<String, ThresholdPair>,
    pub window_size: usize,
    pub max_history: usize,
    pub alert_after_sessions: usize,
    pub cooldown_seconds: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThresholdPair {
    pub decline: f64,
    pub improve: f64,
}

impl Default for Config {
    fn default() -> Self {
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "vocabulary_diversity".to_string(),
            ThresholdPair {
                decline: -0.05,
                improve: 0.05,
            },
        );
        thresholds.insert(
            "evidence_grounding".to_string(),
            ThresholdPair {
                decline: -0.10,
                improve: 0.10,
            },
        );
        thresholds.insert(
            "question_generation".to_string(),
            ThresholdPair {
                decline: -1.0,
                improve: 1.0,
            },
        );
        thresholds.insert(
            "thought_lifecycle".to_string(),
            ThresholdPair {
                decline: -0.10,
                improve: 0.10,
            },
        );
        Config {
            thresholds,
            window_size: 10,
            max_history: 50,
            alert_after_sessions: 7,
            cooldown_seconds: 60,
        }
    }
}

/// Pulse state (last run time for cooldown).
#[derive(Serialize, Deserialize, Default)]
pub struct PulseState {
    pub last_pulse: Option<String>,
}

// --- Load/save helpers ---

pub fn load_signals() -> Result<Vec<SignalVector>, String> {
    let path = super::signals_file()?;
    load_signals_from(&path)
}

pub fn load_signals_from(path: &Path) -> Result<Vec<SignalVector>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path).map_err(|e| format!("Failed to read signals: {e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse signals: {e}"))
}

pub fn save_signals(signals: &[SignalVector]) -> Result<(), String> {
    let path = super::signals_file()?;
    save_signals_to(signals, &path)
}

pub fn save_signals_to(signals: &[SignalVector], path: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(signals)
        .map_err(|e| format!("Failed to serialize signals: {e}"))?;
    fs::write(path, format!("{json}\n")).map_err(|e| format!("Failed to write signals: {e}"))
}

pub fn load_analysis() -> Result<Option<Analysis>, String> {
    let path = super::analysis_file()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path).map_err(|e| format!("Failed to read analysis: {e}"))?;
    let analysis =
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse analysis: {e}"))?;
    Ok(Some(analysis))
}

pub fn save_analysis(analysis: &Analysis) -> Result<(), String> {
    let path = super::analysis_file()?;
    let json = serde_json::to_string_pretty(analysis)
        .map_err(|e| format!("Failed to serialize analysis: {e}"))?;
    fs::write(path, format!("{json}\n")).map_err(|e| format!("Failed to write analysis: {e}"))
}

pub fn load_config() -> Result<Config, String> {
    let path = super::config_file()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let content = fs::read_to_string(&path).map_err(|e| format!("Failed to read config: {e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse config: {e}"))
}

pub fn load_pulse_state() -> Result<PulseState, String> {
    let path = super::vigil_dir()?.join("pulse-state.json");
    if !path.exists() {
        return Ok(PulseState::default());
    }
    let content =
        fs::read_to_string(&path).map_err(|e| format!("Failed to read pulse state: {e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse pulse state: {e}"))
}

pub fn save_pulse_state(state: &PulseState) -> Result<(), String> {
    let path = super::vigil_dir()?.join("pulse-state.json");
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| format!("Failed to serialize pulse state: {e}"))?;
    fs::write(path, format!("{json}\n")).map_err(|e| format!("Failed to write pulse state: {e}"))
}

// --- Timestamp helpers (no chrono dependency) ---

pub fn now_iso() -> String {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;
    let (year, month, day) = days_to_date(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

pub fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn parse_iso_epoch(ts: &str) -> Option<u64> {
    // Parse "YYYY-MM-DDThh:mm:ssZ" to rough epoch seconds
    if ts.len() < 19 {
        return None;
    }
    let year: u64 = ts[..4].parse().ok()?;
    let month: u64 = ts[5..7].parse().ok()?;
    let day: u64 = ts[8..10].parse().ok()?;
    let hours: u64 = ts[11..13].parse().ok()?;
    let minutes: u64 = ts[14..16].parse().ok()?;
    let seconds: u64 = ts[17..19].parse().ok()?;

    let days = date_to_days(year, month, day);
    Some(days * 86400 + hours * 3600 + minutes * 60 + seconds)
}

fn days_to_date(days_since_epoch: u64) -> (u64, u64, u64) {
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

fn date_to_days(year: u64, month: u64, day: u64) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
