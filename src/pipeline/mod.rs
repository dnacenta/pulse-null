pub mod archive;
pub mod health;

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const STATE_FILENAME: &str = "pipeline-state.json";

/// Persistent pipeline state tracked across sessions
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PipelineState {
    pub last_updated: Option<DateTime<Utc>>,
    pub session_count: u32,
    pub sessions_without_movement: u32,
    pub last_counts: DocumentCounts,
}

/// Entry counts per document
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DocumentCounts {
    pub learning: usize,
    pub thoughts: usize,
    pub curiosity: usize,
    pub reflections: usize,
    pub praxis: usize,
}

impl PipelineState {
    pub fn load(root_dir: &Path) -> Self {
        let path = root_dir.join(STATE_FILENAME);
        if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self, root_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let path = root_dir.join(STATE_FILENAME);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Update state with new counts — detects movement (or lack thereof)
    pub fn update_counts(&mut self, new_counts: &DocumentCounts) {
        if *new_counts == self.last_counts {
            self.sessions_without_movement += 1;
        } else {
            self.sessions_without_movement = 0;
        }
        self.last_counts = new_counts.clone();
        self.session_count += 1;
        self.last_updated = Some(Utc::now());
    }
}
