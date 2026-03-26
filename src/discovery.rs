use std::path::{Path, PathBuf};

use crate::config::Config;

/// A discovered entity directory with its loaded config.
pub struct DiscoveredEntity {
    pub name: String,
    pub dir: PathBuf,
    pub config: Config,
}

/// Determine the entity home directory.
///
/// Returns `None` if CWD contains `pulse-null.toml` (single-entity mode).
/// Otherwise checks CWD/entities/ and ~/pulse-null/entities/.
pub fn find_entity_home() -> Option<PathBuf> {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return None,
    };

    // Single entity mode: CWD has pulse-null.toml
    if cwd.join("pulse-null.toml").exists() {
        return None;
    }

    // CWD/entities/
    let local_entities = cwd.join("entities");
    if local_entities.is_dir() {
        return Some(local_entities);
    }

    // ~/pulse-null/entities/
    if let Ok(home) = std::env::var("HOME") {
        let home_entities = PathBuf::from(home).join("pulse-null").join("entities");
        if home_entities.is_dir() {
            return Some(home_entities);
        }
    }

    // No entities dir found — return CWD/entities as desired location
    // (will be created by wizard if needed)
    Some(local_entities)
}

/// Scan the entity home directory for valid entity directories.
pub fn discover_entities(entity_home: &Path) -> Vec<DiscoveredEntity> {
    let mut entities = Vec::new();

    let entries = match std::fs::read_dir(entity_home) {
        Ok(e) => e,
        Err(_) => return entities,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let config_path = path.join("pulse-null.toml");
        if !config_path.exists() {
            continue;
        }
        match Config::load_from(&path) {
            Ok(config) => {
                entities.push(DiscoveredEntity {
                    name: config.entity.name.clone(),
                    dir: path,
                    config,
                });
            }
            Err(e) => {
                tracing::warn!("Skipping {}: config error: {}", path.display(), e);
            }
        }
    }

    entities.sort_by(|a, b| a.name.cmp(&b.name));
    entities
}
