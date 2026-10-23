//! Sealed probe item sets (PN-144 audit #8, R2-11; mobile-app spec §1a.1
//! G7 note).
//!
//! The item texts are not in the repo: pulses work on the repo, and a pulse
//! that can study its probes measures nothing (Goodhart). Nor are they
//! anywhere the live pulse can read. The probe runs as its own user
//! (`pulse-probe` unless `[probe] user` says otherwise), and the sets live
//! in a root-owned directory and reach a run only as systemd credentials:
//!
//! ```text
//!  reviewed copy ── (root) probe sets install --from <dir> --pulse <p> ──▶
//!      /etc/pulse-null-probe/sets/<p>/{<set files>, fact-key}   root 0700 / 0600
//!      (every file must match its committed digest, or nothing is installed;
//!       fact-key is made once, never rotated)
//!  pulse-null-probe@<p>.service ── LoadCredential= ──▶ $CREDENTIALS_DIRECTORY
//!  probe run ── reads and re-checks every digest ──▶ refuses a set that differs
//! ```
//!
//! A run (or `probe facts …`) without the credentials refuses. The repo pins
//! each set file's SHA-256 and its public label and minimum n
//! (`assets/probes/v1/sealed.toml`); the schema, the rules, the scorer and
//! the methods pages stay public. `fact-key` lets the record name which
//! side of a paired fact was planted without saying it (`paired`).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::scratch::hex;

/// The committed digests of the v1 sets.
const SEALED_V1: &str = include_str!("../../../assets/probes/v1/sealed.toml");

/// Where the sets are installed, one directory per pulse (root 0700).
pub const SETS_ROOT: &str = "/etc/pulse-null-probe/sets";

/// The environment variable systemd names the credentials directory in.
pub const CREDENTIALS_ENV: &str = "CREDENTIALS_DIRECTORY";

/// The key the record's paired-fact assignments are committed with.
pub const FACT_KEY_FILE: &str = "fact-key";

/// Largest set file read.
const MAX_SET_BYTES: u64 = 256 * 1024;

/// Shortest fact key accepted.
const MIN_FACT_KEY_LEN: usize = 32;

/// Why sealed sets could not be loaded or installed.
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error(
        "the sealed probe sets reach a run only as the probe unit's credentials \
         ({CREDENTIALS_ENV} is not set): run it with `systemctl start pulse-null-probe@<pulse>`, \
         or for `probe facts …` the systemd-run command in docs/methods/probe-runner.md"
    )]
    NoCredentials,
    #[error(
        "probe set {file} is not among the credentials; install the reviewed sets (as root) \
         with `pulse-null probe sets install --from <dir> --pulse <pulse>`"
    )]
    NotInstalled { file: String },
    #[error(
        "probe set {file} does not match the digest this binary was built with (found \
         {found}, expected {expected}); the run refuses it"
    )]
    Mismatch {
        file: String,
        found: String,
        expected: String,
    },
    #[error("{file}: {reason}")]
    Unreadable { file: String, reason: String },
    #[error("the sealed-set registry is malformed: {0}")]
    Registry(String),
}

/// One pinned set file, with what is public about it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    file: String,
    sha256: String,
    /// `<set>/v<version>`.
    label: String,
    /// The set's pre-registered minimum n (a threshold, public per G7).
    min_n: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    #[serde(rename = "set")]
    sets: Vec<Entry>,
}

/// The digests a binary accepts, by file name, and each set's public label
/// and minimum n.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registry {
    entries: BTreeMap<String, Entry>,
}

impl Registry {
    /// The digests committed for v1.
    pub fn v1() -> Result<Self, SealError> {
        Self::parse(SEALED_V1)
    }

    fn parse(text: &str) -> Result<Self, SealError> {
        let file: RegistryFile =
            toml::from_str(text).map_err(|e| SealError::Registry(e.to_string()))?;
        let mut entries = BTreeMap::new();
        for entry in file.sets {
            let plain = entry
                .file
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                && !entry.file.starts_with('.')
                && entry.file != FACT_KEY_FILE;
            if !plain || entry.sha256.len() != 64 {
                return Err(SealError::Registry(format!("bad entry {}", entry.file)));
            }
            entries.insert(entry.file.clone(), entry);
        }
        Ok(Self { entries })
    }

    /// A registry pinning `texts` as they are, for tests; the label and
    /// minimum n come from each file.
    #[cfg(test)]
    pub(crate) fn pinning(texts: &[(&str, &str)]) -> Self {
        Self {
            entries: texts
                .iter()
                .map(|(file, text)| {
                    let doc: toml::Table = toml::from_str(text).unwrap();
                    let entry = Entry {
                        file: file.to_string(),
                        sha256: digest(text),
                        label: format!(
                            "{}/v{}",
                            doc["set"].as_str().unwrap(),
                            doc["version"].as_integer().unwrap()
                        ),
                        min_n: u32::try_from(doc["min_n"].as_integer().unwrap()).unwrap(),
                    };
                    (file.to_string(), entry)
                })
                .collect(),
        }
    }

    /// The file names this registry pins.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Each set's minimum n, by its label: public, so `probe status` needs
    /// no credentials.
    #[must_use]
    pub fn minimum_n(&self) -> BTreeMap<String, u32> {
        self.entries
            .values()
            .map(|e| (e.label.clone(), e.min_n))
            .collect()
    }

    fn check(&self, file: &str, text: &str) -> Result<(), SealError> {
        let expected = &self
            .entries
            .get(file)
            .ok_or_else(|| SealError::Registry(format!("{file} is not pinned")))?
            .sha256;
        let found = digest(text);
        if &found == expected {
            Ok(())
        } else {
            Err(SealError::Mismatch {
                file: file.to_string(),
                found,
                expected: expected.clone(),
            })
        }
    }

    /// The parsed sets agree with the public label and minimum n pinned for
    /// them (a registry edited without its set, or the reverse).
    fn check_public(&self, installed: &InstalledSets) -> Result<(), SealError> {
        let mut found: BTreeMap<String, u32> = super::sets::all(installed)
            .map_err(|reason| SealError::Unreadable {
                file: "item sets".into(),
                reason,
            })?
            .iter()
            .map(|s| (s.label(), s.set.min_n))
            .collect();
        let pairs = super::paired::LoadedPairs::from_installed(installed).map_err(|reason| {
            SealError::Unreadable {
                file: "paired-facts".into(),
                reason,
            }
        })?;
        found.insert(pairs.label(), pairs.set.min_n);
        if found == self.minimum_n() {
            Ok(())
        } else {
            Err(SealError::Registry(format!(
                "the sets' labels and minimum n ({found:?}) differ from the registry's ({:?})",
                self.minimum_n()
            )))
        }
    }
}

/// Set texts whose digests matched the registry, by file name, and the
/// fact key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSets {
    texts: BTreeMap<String, String>,
    fact_key: String,
}

impl InstalledSets {
    /// The text of `file`, verified.
    pub fn text(&self, file: &str) -> Result<&str, SealError> {
        self.texts
            .get(file)
            .map(String::as_str)
            .ok_or_else(|| SealError::NotInstalled {
                file: file.to_string(),
            })
    }

    /// The key paired-fact assignments are committed with.
    #[must_use]
    pub fn fact_key(&self) -> &str {
        &self.fact_key
    }
}

/// The install directory of `pulse` beneath `root` (normally [`SETS_ROOT`]).
pub fn pulse_dir(root: &Path, pulse: &str) -> Result<PathBuf, SealError> {
    if crate::config::validate::is_plain_pulse_name(pulse) {
        Ok(root.join(pulse.to_ascii_lowercase()))
    } else {
        Err(SealError::Registry(format!(
            "'{pulse}' is not a plain pulse name"
        )))
    }
}

/// The sets from this process's systemd credentials, checked. Refused
/// outside a unit that loads them.
pub fn load_credentials(registry: &Registry) -> Result<InstalledSets, SealError> {
    let dir = std::env::var_os(CREDENTIALS_ENV)
        .filter(|d| !d.is_empty())
        .ok_or(SealError::NoCredentials)?;
    load(Path::new(&dir), registry)
}

/// Read every pinned set and the fact key from `dir` (the credentials
/// directory), checking each digest and the public fields. Any missing or
/// different file refuses the lot.
pub fn load(dir: &Path, registry: &Registry) -> Result<InstalledSets, SealError> {
    let mut texts = BTreeMap::new();
    for file in registry.files() {
        let text = read_set(&dir.join(file), file)?.ok_or_else(|| SealError::NotInstalled {
            file: file.to_string(),
        })?;
        registry.check(file, &text)?;
        texts.insert(file.to_string(), text);
    }
    let fact_key = read_set(&dir.join(FACT_KEY_FILE), FACT_KEY_FILE)?
        .map(|k| k.trim().to_string())
        .filter(|k| k.len() >= MIN_FACT_KEY_LEN)
        .ok_or_else(|| SealError::NotInstalled {
            file: FACT_KEY_FILE.to_string(),
        })?;
    let installed = InstalledSets { texts, fact_key };
    registry.check_public(&installed)?;
    Ok(installed)
}

/// Install every pinned set from `from` into `dest` (created root-style
/// 0700, files 0600), after checking all of them, their public fields and
/// that each parses. A fact key is made if `dest` has none; an existing one
/// is kept (a new one would orphan every fact in flight). Nothing is
/// installed unless every file matches. Returns the files written.
pub fn install(from: &Path, dest: &Path, registry: &Registry) -> Result<Vec<String>, SealError> {
    let texts = read_reviewed(from, registry)?;
    let existing_key = existing_fact_key(dest);
    let fact_key = existing_key
        .clone()
        .unwrap_or_else(crate::surrealdb_manager::generate_password);
    let installed = InstalledSets {
        texts: texts.clone(),
        fact_key: fact_key.clone(),
    };
    registry.check_public(&installed)?;
    let mut written = write_sets(dest, &texts)?;
    if existing_key.is_none() {
        written.push(write_fact_key(dest, &fact_key)?);
    }
    Ok(written)
}

/// Every pinned set from the reviewed copy in `from`, each checked against
/// its digest. One missing or different file refuses the lot.
fn read_reviewed(from: &Path, registry: &Registry) -> Result<BTreeMap<String, String>, SealError> {
    let mut texts = BTreeMap::new();
    for file in registry.files() {
        let text = read_set(&from.join(file), file)?.ok_or_else(|| SealError::Unreadable {
            file: file.to_string(),
            reason: format!("not found in {}", from.display()),
        })?;
        registry.check(file, &text)?;
        texts.insert(file.to_string(), text);
    }
    Ok(texts)
}

/// The fact key an earlier install left in `dest`, when it has a usable one.
fn existing_fact_key(dest: &Path) -> Option<String> {
    read_set(&dest.join(FACT_KEY_FILE), FACT_KEY_FILE)
        .ok()
        .flatten()
        .map(|k| k.trim().to_string())
        .filter(|k| k.len() >= MIN_FACT_KEY_LEN)
}

/// Create `dest` (0700) and write every set into it (0600). Returns the
/// files written.
fn write_sets(dest: &Path, texts: &BTreeMap<String, String>) -> Result<Vec<String>, SealError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dest)
        .map_err(|e| unreadable(dest, e))?;
    let mut written = Vec::new();
    for (file, text) in texts {
        write_private(&dest.join(file), text).map_err(|e| unreadable(&dest.join(file), e))?;
        written.push(file.clone());
    }
    Ok(written)
}

/// Write a new fact key into `dest` (0600). Returns its file name.
fn write_fact_key(dest: &Path, fact_key: &str) -> Result<String, SealError> {
    let path = dest.join(FACT_KEY_FILE);
    write_private(&path, fact_key).map_err(|e| unreadable(&path, e))?;
    Ok(FACT_KEY_FILE.to_string())
}

fn unreadable(path: &Path, e: std::io::Error) -> SealError {
    SealError::Unreadable {
        file: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// A set file's text: bounded, a regular file, never through a link.
fn read_set(path: &Path, file: &str) -> Result<Option<String>, SealError> {
    crate::fs_safe::read_bounded(path, MAX_SET_BYTES).map_err(|e| SealError::Unreadable {
        file: file.to_string(),
        reason: e.to_string(),
    })
}

/// Replace `path` with `text`, 0600, through a temporary file.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("new");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

fn digest(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Public fixture sets for tests: the shape of v1 (ids, families,
    /// truths, rules, arms), none of its wording.
    pub(crate) const FIXTURES: [(&str, &str); 3] = [
        (
            "known-answer.toml",
            include_str!("../../../tests/fixtures/probes/known-answer.toml"),
        ),
        (
            "situation-use.toml",
            include_str!("../../../tests/fixtures/probes/situation-use.toml"),
        ),
        (
            "paired-facts.toml",
            include_str!("../../../tests/fixtures/probes/paired-facts.toml"),
        ),
    ];

    /// The fixture fact key.
    pub(crate) const FIXTURE_FACT_KEY: &str = "fixture-fact-key-0123456789abcdefghij";

    /// The fixture sets as verified installed sets.
    pub(crate) fn fixture_sets() -> InstalledSets {
        InstalledSets {
            texts: FIXTURES
                .iter()
                .map(|(f, t)| (f.to_string(), t.to_string()))
                .collect(),
            fact_key: FIXTURE_FACT_KEY.to_string(),
        }
    }

    /// The fixture sets as a credentials directory would hold them, in
    /// `dir`, pinned by a registry made from them.
    pub(crate) fn fixture_credentials(dir: &Path) -> Registry {
        for (file, text) in FIXTURES {
            std::fs::write(dir.join(file), text).unwrap();
        }
        std::fs::write(dir.join(FACT_KEY_FILE), FIXTURE_FACT_KEY).unwrap();
        Registry::pinning(&FIXTURES)
    }

    fn reviewed_copy() -> tempfile::TempDir {
        let from = tempfile::tempdir().unwrap();
        for (file, text) in FIXTURES {
            std::fs::write(from.path().join(file), text).unwrap();
        }
        from
    }

    #[test]
    fn the_committed_registry_pins_every_v1_set_with_its_public_fields() {
        let registry = Registry::v1().unwrap();
        let files: Vec<&str> = registry.files().collect();
        assert_eq!(
            files,
            [
                "known-answer.toml",
                "paired-facts.toml",
                "situation-use.toml"
            ]
        );
        let min_n = registry.minimum_n();
        assert_eq!(min_n["known-answer/v1"], 30);
        assert_eq!(min_n["situation-use/v1"], 20);
        assert_eq!(min_n["paired-facts/v1"], 10);
    }

    #[test]
    fn install_writes_private_files_and_a_fact_key_that_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dest = pulse_dir(root.path(), "Echo").unwrap();
        assert!(dest.ends_with("echo"));
        let registry = Registry::pinning(&FIXTURES);
        let from = reviewed_copy();
        let written = install(from.path(), &dest, &registry).unwrap();
        assert!(written.contains(&FACT_KEY_FILE.to_string()));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dest), 0o700);
        for file in registry.files().chain([FACT_KEY_FILE]) {
            assert_eq!(mode(&dest.join(file)), 0o600, "{file}");
        }
        let key = std::fs::read_to_string(dest.join(FACT_KEY_FILE)).unwrap();
        assert!(key.len() >= MIN_FACT_KEY_LEN);
        // A second install keeps the key: facts in flight stay readable.
        let again = install(from.path(), &dest, &registry).unwrap();
        assert!(!again.contains(&FACT_KEY_FILE.to_string()));
        assert_eq!(
            std::fs::read_to_string(dest.join(FACT_KEY_FILE)).unwrap(),
            key
        );
        let loaded = load(&dest, &registry).unwrap();
        assert_eq!(loaded.fact_key(), key);
        assert!(pulse_dir(root.path(), "../x").is_err());
    }

    #[test]
    fn a_set_that_differs_from_its_digest_is_refused() {
        let creds = tempfile::tempdir().unwrap();
        let registry = fixture_credentials(creds.path());
        let edited = creds.path().join("situation-use.toml");
        let mut text = std::fs::read_to_string(&edited).unwrap();
        text.push_str("\n# a quiet edit\n");
        std::fs::write(&edited, text).unwrap();
        assert!(matches!(
            load(creds.path(), &registry),
            Err(SealError::Mismatch { file, .. }) if file == "situation-use.toml"
        ));

        // An install with one wrong file installs nothing.
        let dest = tempfile::tempdir().unwrap();
        let from = reviewed_copy();
        let changed = from.path().join("paired-facts.toml");
        let text = std::fs::read_to_string(&changed).unwrap();
        std::fs::write(&changed, format!("{text}\n# changed")).unwrap();
        let target = dest.path().join("echo");
        assert!(install(from.path(), &target, &registry).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn missing_sets_a_missing_key_or_no_credentials_are_refused() {
        let creds = tempfile::tempdir().unwrap();
        let registry = fixture_credentials(creds.path());
        std::fs::remove_file(creds.path().join(FACT_KEY_FILE)).unwrap();
        assert!(matches!(
            load(creds.path(), &registry),
            Err(SealError::NotInstalled { file }) if file == FACT_KEY_FILE
        ));
        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(empty.path(), &registry),
            Err(SealError::NotInstalled { .. })
        ));
        // The test process has no systemd credentials.
        if std::env::var_os(CREDENTIALS_ENV).is_none() {
            assert!(matches!(
                load_credentials(&registry),
                Err(SealError::NoCredentials)
            ));
        }
    }

    #[test]
    fn a_registry_that_disagrees_with_its_set_is_refused() {
        let creds = tempfile::tempdir().unwrap();
        let mut registry = fixture_credentials(creds.path());
        registry.entries.get_mut("known-answer.toml").unwrap().min_n = 3;
        assert!(matches!(
            load(creds.path(), &registry),
            Err(SealError::Registry(_))
        ));
    }

    /// By hand, on the reviewed copies (never in the gate):
    /// `PULSE_NULL_PROBE_REVIEW_SETS=<dir> cargo test reviewed_sets -- --ignored`.
    /// They must match the committed digests and public fields, parse, and
    /// v1 must carry at least 30 pairs (one use each).
    #[test]
    #[ignore = "reads the reviewed sealed sets (see doc comment)"]
    fn reviewed_sets_match_the_committed_digests_and_parse() {
        let dir = std::env::var("PULSE_NULL_PROBE_REVIEW_SETS").expect(
            "set PULSE_NULL_PROBE_REVIEW_SETS to the reviewed copies: this test checks them",
        );
        let dest = tempfile::tempdir().unwrap();
        let registry = Registry::v1().unwrap();
        install(Path::new(&dir), dest.path(), &registry).unwrap();
        let installed = load(dest.path(), &registry).unwrap();
        let sets = crate::vigil::probe::sets::all(&installed).unwrap();
        assert_eq!(sets.len(), 2);
        let pairs = crate::vigil::probe::paired::LoadedPairs::from_installed(&installed).unwrap();
        assert!(pairs.set.pairs.len() >= 30, "{}", pairs.set.pairs.len());
        for set in &sets {
            let prompt = match set.set.delivery {
                crate::vigil::probe::sets::Delivery::Batched => set.batched_prompt(),
                crate::vigil::probe::sets::Delivery::PerItem => set.item_prompt(&set.set.items[0]),
            };
            assert!(
                !prompt.to_lowercase().contains("check run by your runtime"),
                "no per-turn notice: {prompt}"
            );
        }
    }
}
