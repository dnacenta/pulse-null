use std::path::{Path, PathBuf};

use crate::config::Config;

/// A discovered pulse directory with its loaded config.
pub struct DiscoveredPulse {
    pub name: String,
    pub dir: PathBuf,
    pub config: Config,
}

/// Determine the pulse home directory.
///
/// Returns `None` if CWD contains `pulse-null.toml` (single-pulse mode).
/// Otherwise the pulse home is the first of: CWD itself when it already
/// holds pulses as direct children (the flat `~/pulse-null/<name>` layout,
/// PN-104), a `CWD/pulses/` container (or its pre-PN-115 name
/// `CWD/entities/`), `~/pulse-null/` with pulse children, then
/// `~/pulse-null/pulses/` (or `~/pulse-null/entities/`); failing all of
/// those, CWD is the place new pulses will be created.
pub fn find_pulse_home() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_pulse_home(&cwd, home.as_deref())
}

/// The pure resolution, separated from process state so it can be tested.
fn resolve_pulse_home(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    // Single pulse mode: CWD is inside a pulse (any ancestor holds
    // pulse-null.toml) — the same walk `Config::load()` does, so `up` agrees
    // with every other subcommand about which pulse a directory belongs to.
    if cwd
        .ancestors()
        .any(|dir| dir.join("pulse-null.toml").exists())
    {
        return None;
    }

    // Flat layout: pulses are direct children of CWD
    if has_pulse_children(cwd) {
        return Some(cwd.to_path_buf());
    }

    // Container: CWD/pulses/ (or legacy CWD/entities/)
    if let Some(container) = pulse_container(cwd) {
        return Some(container);
    }

    if let Some(home) = home {
        let install = home.join("pulse-null");
        // Flat layout under the install root: ~/pulse-null/<name>
        if has_pulse_children(&install) {
            return Some(install);
        }
        // Container: ~/pulse-null/pulses/ (or legacy ~/pulse-null/entities/)
        if let Some(container) = pulse_container(&install) {
            return Some(container);
        }
    }

    // Nothing yet — new pulses go straight into CWD (flat layout).
    Some(cwd.to_path_buf())
}

/// Names a directory of pulses may carry, canonical first: `pulses/`, and
/// `entities/` as it was called before PN-115.
pub const PULSE_CONTAINER_NAMES: [&str; 2] = ["pulses", "entities"];

/// The existing pulse container directly under `dir`, canonical name first.
pub fn pulse_container(dir: &Path) -> Option<PathBuf> {
    PULSE_CONTAINER_NAMES
        .iter()
        .map(|name| dir.join(name))
        .find(|candidate| candidate.is_dir())
}

/// Is `dir` itself a pulse container (`pulses/` or legacy `entities/`)?
pub fn is_pulse_container(dir: &Path) -> bool {
    dir.file_name()
        .is_some_and(|name| PULSE_CONTAINER_NAMES.iter().any(|c| name == *c))
}

/// Does `dir` hold at least one pulse as a direct child?
pub fn has_pulse_children(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().any(|e| is_pulse_child(&e)))
        .unwrap_or(false)
}

/// A real subdirectory (not a symlink — a link can point at a tree someone
/// else controls) holding a `pulse-null.toml`.
fn is_pulse_child(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|t| t.is_dir()) && entry.path().join("pulse-null.toml").exists()
}

/// Booting a pulse runs its configured binaries with our rights; only
/// directories we own qualify. `DirEntry::metadata` does not follow
/// symlinks, so it describes the same thing `is_pulse_child` judged.
fn owned_by_us(entry: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    entry.metadata().is_ok_and(|m| m.uid() == me)
}

/// A port for a new pulse in `pulse_home`: the first from 3200 upward
/// that no sibling's `pulse-null.toml` already claims. Every pulse binding
/// its own configured port is what makes those ports stable.
pub fn suggest_port(pulse_home: &Path) -> u16 {
    let taken: std::collections::BTreeSet<u16> = std::fs::read_dir(pulse_home)
        .map(|entries| {
            entries
                .flatten()
                .filter(is_pulse_child)
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

/// Pulse names become directory names under the pulse home, so they are
/// kept to a safe shape: lowercase ASCII letters, digits, `-` and `_`, 1–32
/// characters, starting with a letter or digit.
pub fn validate_pulse_name(name: &str) -> Result<String, String> {
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

/// Scan the pulse home directory for valid pulse directories. Two
/// directories claiming the same pulse name would silently shadow each
/// other in the registry, so only the first (by path) is kept and the
/// duplicate is reported.
pub fn discover_pulses(pulse_home: &Path) -> Vec<DiscoveredPulse> {
    let mut pulses = Vec::new();

    let entries = match std::fs::read_dir(pulse_home) {
        Ok(e) => e,
        Err(_) => return pulses,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !is_pulse_child(&entry) {
            continue;
        }
        if !owned_by_us(&entry) {
            tracing::warn!(
                "Skipping {}: not owned by the running user — a pulse is booted with this user's rights",
                path.display()
            );
            continue;
        }
        match Config::load_from(&path) {
            Ok(config) => {
                pulses.push(DiscoveredPulse {
                    name: config.pulse.name.clone(),
                    dir: path,
                    config,
                });
            }
            Err(e) => {
                tracing::warn!("Skipping {}: config error: {}", path.display(), e);
            }
        }
    }

    pulses.sort_by(|a, b| a.dir.cmp(&b.dir));
    let mut seen = std::collections::HashSet::new();
    pulses.retain(|e| {
        if seen.insert(e.name.clone()) {
            true
        } else {
            tracing::warn!(
                "Skipping {}: another pulse directory already uses the name \"{}\"",
                e.dir.display(),
                e.name
            );
            false
        }
    });
    pulses.sort_by(|a, b| a.name.cmp(&b.name));
    pulses
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pulse_at(dir: &Path, name: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("pulse-null.toml"), "").unwrap();
    }

    #[test]
    fn flat_children_are_pulse_home() {
        let cwd = tempfile::tempdir().unwrap();
        pulse_at(cwd.path(), "echo");
        pulse_at(cwd.path(), "synth");
        std::fs::write(cwd.path().join("notes.md"), "").unwrap();
        assert_eq!(
            resolve_pulse_home(cwd.path(), None),
            Some(cwd.path().to_path_buf())
        );
        assert!(has_pulse_children(cwd.path()));
    }

    #[test]
    fn legacy_entities_dir_still_found() {
        let cwd = tempfile::tempdir().unwrap();
        pulse_at(&cwd.path().join("entities"), "nova");
        assert_eq!(
            resolve_pulse_home(cwd.path(), None),
            Some(cwd.path().join("entities"))
        );
    }

    #[test]
    fn pulses_dir_is_found_and_wins_over_legacy_entities() {
        let cwd = tempfile::tempdir().unwrap();
        pulse_at(&cwd.path().join("pulses"), "echo");
        assert_eq!(
            resolve_pulse_home(cwd.path(), None),
            Some(cwd.path().join("pulses"))
        );
        pulse_at(&cwd.path().join("entities"), "nova");
        assert_eq!(
            resolve_pulse_home(cwd.path(), None),
            Some(cwd.path().join("pulses"))
        );
        assert!(is_pulse_container(&cwd.path().join("pulses")));
        assert!(is_pulse_container(&cwd.path().join("entities")));
        assert!(!is_pulse_container(cwd.path()));
    }

    #[test]
    fn home_install_root_is_probed_flat_then_legacy() {
        let cwd = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        pulse_at(&home.path().join("pulse-null"), "echo");
        assert_eq!(
            resolve_pulse_home(cwd.path(), Some(home.path())),
            Some(home.path().join("pulse-null"))
        );

        let home2 = tempfile::tempdir().unwrap();
        pulse_at(&home2.path().join("pulse-null/entities"), "echo");
        assert_eq!(
            resolve_pulse_home(cwd.path(), Some(home2.path())),
            Some(home2.path().join("pulse-null/entities"))
        );

        let home3 = tempfile::tempdir().unwrap();
        pulse_at(&home3.path().join("pulse-null/pulses"), "echo");
        assert_eq!(
            resolve_pulse_home(cwd.path(), Some(home3.path())),
            Some(home3.path().join("pulse-null/pulses"))
        );
    }

    #[test]
    fn empty_dir_is_desired_home() {
        let cwd = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_pulse_home(cwd.path(), Some(home.path())),
            Some(cwd.path().to_path_buf())
        );
        assert!(!has_pulse_children(cwd.path()));
    }

    #[test]
    fn symlinked_children_are_not_pulses() {
        let cwd = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        pulse_at(elsewhere.path(), "real");
        std::os::unix::fs::symlink(elsewhere.path().join("real"), cwd.path().join("linked"))
            .unwrap();
        assert!(!has_pulse_children(cwd.path()));
        assert!(discover_pulses(cwd.path()).is_empty());
    }

    #[test]
    fn a_config_in_cwd_means_single_pulse() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("pulse-null.toml"), "").unwrap();
        pulse_at(cwd.path(), "nested");
        assert_eq!(resolve_pulse_home(cwd.path(), None), None);
    }

    #[test]
    fn a_subdirectory_of_a_pulse_is_still_that_pulse() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("pulse-null.toml"), "").unwrap();
        let sub = cwd.path().join("journal");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(resolve_pulse_home(&sub, None), None);
    }

    #[test]
    fn suggest_port_skips_ports_siblings_claim() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(suggest_port(home.path()), 3200);
        let toml = |port: u16| {
            format!(
                "[pulse]\nname = \"e{port}\"\nowner_name = \"D\"\n[server]\nhost = \"127.0.0.1\"\nport = {port}\n[llm]\nprovider = \"claude-code\"\nmodel = \"x\"\n"
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
    fn pulse_names_are_validated() {
        assert_eq!(validate_pulse_name(" Synth "), Ok("synth".into()));
        assert_eq!(validate_pulse_name("echo-2_b"), Ok("echo-2_b".into()));
        assert!(validate_pulse_name("").is_err());
        assert!(validate_pulse_name("../x").is_err());
        assert!(validate_pulse_name("a b").is_err());
        assert!(validate_pulse_name("-lead").is_err());
        assert!(validate_pulse_name(&"x".repeat(33)).is_err());
    }
}
