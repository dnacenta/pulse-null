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
    // Single entity mode: CWD is inside an entity (any ancestor holds
    // pulse-null.toml) — the same walk `Config::load()` does, so `up` agrees
    // with every other subcommand about which entity a directory belongs to.
    if cwd
        .ancestors()
        .any(|dir| dir.join("pulse-null.toml").exists())
    {
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
        .map(|entries| entries.flatten().any(|e| is_entity_child(&e)))
        .unwrap_or(false)
}

/// A real subdirectory (not a symlink — a link can point at a tree someone
/// else controls) holding a `pulse-null.toml`.
fn is_entity_child(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|t| t.is_dir()) && entry.path().join("pulse-null.toml").exists()
}

/// Booting an entity runs its configured binaries with our rights; only
/// directories we own qualify. `DirEntry::metadata` does not follow
/// symlinks, so it describes the same thing `is_entity_child` judged.
fn owned_by_us(entry: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    entry.metadata().is_ok_and(|m| m.uid() == me)
}

/// A port for a new entity in `entity_home`: the first from 3200 upward
/// that no sibling's `pulse-null.toml` already claims. Every entity binding
/// its own configured port is what makes those ports stable.
pub fn suggest_port(entity_home: &Path) -> u16 {
    let taken: std::collections::BTreeSet<u16> = std::fs::read_dir(entity_home)
        .map(|entries| {
            entries
                .flatten()
                .filter(is_entity_child)
                .filter_map(|e| configured_port(&e.path().join("pulse-null.toml")))
                .collect()
        })
        .unwrap_or_default();
    (3200..u16::MAX)
        .find(|p| !taken.contains(p))
        .unwrap_or(3200)
}

/// `[server] port` from a config file, read leniently: a sibling whose
/// config would not pass full validation still holds its port.
fn configured_port(config_path: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(config_path).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    let port = doc.get("server")?.get("port")?.as_integer()?;
    u16::try_from(port).ok()
}

/// Entity names become directory names under the entity home, so they are
/// kept to a safe shape: lowercase ASCII letters, digits, `-` and `_`, 1–32
/// characters, starting with a letter or digit.
pub fn validate_entity_name(name: &str) -> Result<String, String> {
    let name = name.trim().to_lowercase();
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(name)
    } else {
        Err(
            "use 1–32 lowercase letters, digits, '-' or '_', starting with a letter or digit"
                .into(),
        )
    }
}

/// Scan the entity home directory for valid entity directories. Two
/// directories claiming the same entity name would silently shadow each
/// other in the registry, so only the first (by path) is kept and the
/// duplicate is reported.
pub fn discover_entities(entity_home: &Path) -> Vec<DiscoveredEntity> {
    let mut entities = Vec::new();

    let entries = match std::fs::read_dir(entity_home) {
        Ok(e) => e,
        Err(_) => return entities,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !is_entity_child(&entry) {
            continue;
        }
        if !owned_by_us(&entry) {
            tracing::warn!(
                "Skipping {}: not owned by the running user — an entity is booted with this user's rights",
                path.display()
            );
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

    entities.sort_by(|a, b| a.dir.cmp(&b.dir));
    let mut seen = std::collections::HashSet::new();
    entities.retain(|e| {
        if seen.insert(e.name.clone()) {
            true
        } else {
            tracing::warn!(
                "Skipping {}: another entity directory already uses the name \"{}\"",
                e.dir.display(),
                e.name
            );
            false
        }
    });
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
    fn symlinked_children_are_not_entities() {
        let cwd = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        entity_at(elsewhere.path(), "real");
        std::os::unix::fs::symlink(elsewhere.path().join("real"), cwd.path().join("linked"))
            .unwrap();
        assert!(!has_entity_children(cwd.path()));
        assert!(discover_entities(cwd.path()).is_empty());
    }

    #[test]
    fn a_config_in_cwd_means_single_entity() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("pulse-null.toml"), "").unwrap();
        entity_at(cwd.path(), "nested");
        assert_eq!(resolve_entity_home(cwd.path(), None), None);
    }

    #[test]
    fn a_subdirectory_of_an_entity_is_still_that_entity() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("pulse-null.toml"), "").unwrap();
        let sub = cwd.path().join("journal");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(resolve_entity_home(&sub, None), None);
    }

    #[test]
    fn suggest_port_skips_ports_siblings_claim() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(suggest_port(home.path()), 3200);
        let toml = |port: u16| {
            format!(
                "[entity]\nname = \"e{port}\"\nowner_name = \"D\"\n[server]\nhost = \"127.0.0.1\"\nport = {port}\n[llm]\nprovider = \"claude-code\"\nmodel = \"x\"\n"
            )
        };
        for port in [3200u16, 3201] {
            let d = home.path().join(format!("e{port}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("pulse-null.toml"), toml(port)).unwrap();
        }
        assert_eq!(suggest_port(home.path()), 3202);
    }

    #[test]
    fn entity_names_are_validated() {
        assert_eq!(validate_entity_name(" Synth "), Ok("synth".into()));
        assert_eq!(validate_entity_name("echo-2_b"), Ok("echo-2_b".into()));
        assert!(validate_entity_name("").is_err());
        assert!(validate_entity_name("../x").is_err());
        assert!(validate_entity_name("a b").is_err());
        assert!(validate_entity_name("-lead").is_err());
        assert!(validate_entity_name(&"x".repeat(33)).is_err());
    }
}
