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
/// Otherwise the entity home is the first of: CWD itself when it already
/// holds entities as direct children (the flat `~/pulse-null/<name>` layout,
/// PN-104), the legacy `CWD/entities/`, `~/pulse-null/` with entity
/// children, the legacy `~/pulse-null/entities/`; failing all of those, CWD
/// is the place new entities will be created.
pub fn find_entity_home() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_entity_home(&cwd, home.as_deref())
}

/// The pure resolution, separated from process state so it can be tested.
fn resolve_entity_home(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    // Single entity mode: CWD has pulse-null.toml
    if cwd.join("pulse-null.toml").exists() {
        return None;
    }

    // Flat layout: entities are direct children of CWD
    if has_entity_children(cwd) {
        return Some(cwd.to_path_buf());
    }

    // Legacy: CWD/entities/
    let local_entities = cwd.join("entities");
    if local_entities.is_dir() {
        return Some(local_entities);
    }

    if let Some(home) = home {
        let install = home.join("pulse-null");
        // Flat layout under the install root: ~/pulse-null/<name>
        if has_entity_children(&install) {
            return Some(install);
        }
        // Legacy: ~/pulse-null/entities/
        let home_entities = install.join("entities");
        if home_entities.is_dir() {
            return Some(home_entities);
        }
    }

    // Nothing yet — new entities go straight into CWD (flat layout).
    Some(cwd.to_path_buf())
}

/// Does `dir` hold at least one entity as a direct child?
pub fn has_entity_children(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().is_dir() && e.path().join("pulse-null.toml").exists())
        })
        .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entity_at(dir: &Path, name: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("pulse-null.toml"), "").unwrap();
    }

    #[test]
    fn flat_children_are_entity_home() {
        let cwd = tempfile::tempdir().unwrap();
        entity_at(cwd.path(), "echo");
        entity_at(cwd.path(), "synth");
        std::fs::write(cwd.path().join("notes.md"), "").unwrap();
        assert_eq!(
            resolve_entity_home(cwd.path(), None),
            Some(cwd.path().to_path_buf())
        );
        assert!(has_entity_children(cwd.path()));
    }

    #[test]
    fn legacy_entities_dir_still_found() {
        let cwd = tempfile::tempdir().unwrap();
        entity_at(&cwd.path().join("entities"), "nova");
        assert_eq!(
            resolve_entity_home(cwd.path(), None),
            Some(cwd.path().join("entities"))
        );
    }

    #[test]
    fn home_install_root_is_probed_flat_then_legacy() {
        let cwd = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        entity_at(&home.path().join("pulse-null"), "echo");
        assert_eq!(
            resolve_entity_home(cwd.path(), Some(home.path())),
            Some(home.path().join("pulse-null"))
        );

        let home2 = tempfile::tempdir().unwrap();
        entity_at(&home2.path().join("pulse-null/entities"), "echo");
        assert_eq!(
            resolve_entity_home(cwd.path(), Some(home2.path())),
            Some(home2.path().join("pulse-null/entities"))
        );
    }

    #[test]
    fn empty_dir_is_desired_home() {
        let cwd = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_entity_home(cwd.path(), Some(home.path())),
            Some(cwd.path().to_path_buf())
        );
        assert!(!has_entity_children(cwd.path()));
    }

    #[test]
    fn a_config_in_cwd_means_single_entity() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("pulse-null.toml"), "").unwrap();
        entity_at(cwd.path(), "nested");
        assert_eq!(resolve_entity_home(cwd.path(), None), None);
    }
}
