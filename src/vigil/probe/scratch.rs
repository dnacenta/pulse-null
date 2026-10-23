//! Scratch copies of a pulse for probe items (PN-144).
//!
//! A probe item runs in its own working copy of the pulse home, with its own
//! vendor directory, temp, cache, state and runtime directories, all under
//! one private scratch directory that is removed at teardown:
//!
//! ```text
//!  <state>/pulse-null/s/<id>/            0700, removed at teardown
//!    base/<pulse>/                       filtered copy of the live home
//!    items/<n>/<pulse>/                  this item's working copy
//!    items/<n>/{vendor,tmp,cache,state,run}/
//! ```
//!
//! The base copy leaves out what a probe must never carry (`secrets/`, the
//! held-out `record/`, sessions, coordinator state, logs, locks, the config
//! with its secrets, the embedded graph). Each working copy then gets the
//! rewrites that keep the probe's own writes inside it: recall-echo capture,
//! sync and background extraction off, its socket and graph inside the
//! scratch, and the CLI's project hooks reduced to `consume`. The Landlock
//! sandbox built here makes the item the only place the child can write,
//! and the item and the read fence ([`super::fence`]) the only places it can
//! read.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::cli_provider::adapter::{CliAdapter, VendorProfile, VendorScratchSpec};
use crate::cli_provider::sandbox::Sandbox;
use crate::cli_provider::SpawnOverrides;
use crate::init::agent_bootstrap::ItemStatus;

use super::fence;

/// Top-level directories of the live home that never reach a scratch copy.
const EXCLUDED_TOP_DIRS: &[&str] = &["secrets", "record", "sessions", "coordinator", "logs"];

/// Top-level files that never reach a scratch copy: the daemon's pidfile and
/// the isolation marker are live state, not the pulse's.
const EXCLUDED_TOP_FILES: &[&str] = &[".pulse-null.pid", "ISOLATION"];

/// The config and its backups carry the pulse's secrets.
const CONFIG_PREFIX: &str = "pulse-null.toml";

/// The embedded graph's store. The rest of `memory/graph/` (the embedding
/// model cache, the stats cache the prompt reads) is copied.
const EMBEDDED_GRAPH_STORE: &str = "memory/graph/surreal";

/// A scratch dir (and its probe database) older than this belongs to a run
/// that crashed; the next run removes it.
pub const SCRATCH_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// The throwaway graph's probe-user password, in the scratch dir.
pub const GRAPH_PASSWORD_FILE: &str = "graph-password";

/// The file in a scratch dir that records when the run started.
const CREATED_FILE: &str = ".created";

/// recall-echo's config inside a pulse.
const RECALL_ECHO_CONFIG: &str = "memory/.recall-echo.toml";

/// The directories of one item, beside its working copy.
const ITEM_DIRS: [&str; 8] = [
    "vendor", "tmp", "cache", "state", "run", "home", "config", "data",
];

/// How long a probe's recall-echo daemon may idle before it exits.
const SCRATCH_DAEMON_IDLE_SECS: i64 = 60;

/// Why a scratch item could not be prepared.
#[derive(Debug, thiserror::Error)]
pub enum ScratchError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error("probe copy is not safe to run: {path}: {reason}")]
    UnsafeProjectFile { path: String, reason: String },
}

fn io_error(context: impl Into<String>) -> impl FnOnce(io::Error) -> ScratchError {
    let context = context.into();
    move |source| ScratchError::Io { context, source }
}

/// Where a probe item's recall-echo graph lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphTarget {
    /// An empty embedded graph inside the item's own copy (a pulse whose
    /// graph is not in server mode).
    Embedded,
    /// The run's copy of the pulse's graph on its throwaway server.
    Server {
        addr: String,
        namespace: String,
        database: String,
        username: String,
        password_file: PathBuf,
    },
}

/// The live pulse a scratch copies from.
#[derive(Debug, Clone)]
pub struct ScratchSpec<'a> {
    pub live_root: &'a Path,
    pub live_home: &'a Path,
    pub pulse: &'a str,
    /// The directory scratch directories are created in (see [`scratch_parent`]).
    pub parent: &'a Path,
    /// Leave everything on disk after teardown, for inspection.
    pub keep: bool,
}

/// What every item of one run is prepared with.
#[derive(Clone, Copy)]
pub struct ItemPlan<'a> {
    pub adapter: &'a dyn CliAdapter,
    pub recall_bin: &'a str,
    pub graph: &'a GraphTarget,
    /// TCP ports the child may connect to.
    pub tcp_connect: &'a [u16],
    /// What the child may read besides its own item ([`super::fence`]).
    pub read_roots: &'a [PathBuf],
}

/// Which arm an item serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// A working copy of the pulse.
    Pulse,
    /// An empty working directory with a bare vendor directory: the same
    /// CLI and model with no scaffold.
    Bare,
}

/// The unit sets this to the probe-private state root (`/var/lib/pulse-null-probe`).
/// Scratch, exports and the record live under it, never under the pulse's home.
pub const PROBE_STATE_ENV: &str = "PULSE_NULL_PROBE_STATE";

/// Where this process keeps probe-private state. `override_env` is
/// [`PROBE_STATE_ENV`] when the caller wants the process environment;
/// tests pass a fixed value. Without it, state is under `process_home`,
/// which is the probe user's home when the unit runs as that user — never
/// a home handed in for the pulse.
pub fn state_root(override_env: Option<&OsStr>, process_home: &Path) -> PathBuf {
    match override_env {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => process_home.join(".local/state/pulse-null/probe"),
    }
}

/// [`state_root`] from the process environment.
pub fn probe_state_root(process_home: &Path) -> PathBuf {
    state_root(std::env::var_os(PROBE_STATE_ENV).as_deref(), process_home)
}

/// Scratch directories, the host lock and graph exports, under the probe
/// state root. Kept short because recall-echo puts a Unix socket under it.
pub fn probe_scratch_parent(state: &Path) -> PathBuf {
    state.join("s")
}

/// The directory [`super::record::Record::open`] treats as a pulse root:
/// the record is `<state>/<pulse>/record/probes`, outside the live home.
pub fn probe_record_root(state: &Path, pulse: &str) -> Result<PathBuf, String> {
    let name = pulse.to_ascii_lowercase();
    if !is_plain_name(&name) {
        return Err(format!(
            "'{pulse}' is not a plain pulse name (letters, digits, - or _)"
        ));
    }
    Ok(state.join(name))
}

/// One run's scratch directory. Dropping it removes everything unless it was
/// created to be kept.
#[derive(Debug)]
pub struct Scratch {
    id: String,
    dir: PathBuf,
    base: PathBuf,
    live_root: PathBuf,
    live_home: PathBuf,
    pulse: String,
    keep: bool,
    removed: bool,
}

impl Scratch {
    /// Create the scratch directory and the filtered base copy of the pulse.
    pub fn create(spec: &ScratchSpec<'_>) -> Result<Self, ScratchError> {
        if !is_plain_name(spec.pulse) {
            return Err(ScratchError::UnsafeProjectFile {
                path: spec.pulse.to_string(),
                reason: "a pulse name used in a scratch path must be letters, digits, - or _"
                    .into(),
            });
        }
        create_private_dir_all(spec.parent).map_err(io_error("creating the scratch parent"))?;
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let dir = spec.parent.join(&id);
        create_private_dir(&dir).map_err(io_error("creating the scratch dir"))?;
        std::fs::write(dir.join(CREATED_FILE), chrono::Utc::now().to_rfc3339())
            .map_err(io_error("stamping the scratch dir"))?;
        let live_root = spec
            .live_root
            .canonicalize()
            .map_err(io_error("resolving the live pulse root"))?;
        let scratch = Self {
            base: dir.join("base").join(spec.pulse),
            id,
            dir,
            live_root,
            live_home: spec.live_home.to_path_buf(),
            pulse: spec.pulse.to_string(),
            keep: spec.keep,
            removed: false,
        };
        create_private_dir_all(&scratch.base).map_err(io_error("creating the base copy"))?;
        copy_tree(&scratch.live_root, &scratch.base, None, CopyMode::Filtered)
            .map_err(io_error("copying the pulse home"))?;
        Ok(scratch)
    }

    /// The random name of this scratch, which also appears in every path in it.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The filtered base copy.
    #[cfg(test)]
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Prepare item `index`: a fresh working copy (or an empty directory for
    /// a bare arm), its vendor directory, and the sandbox for its child.
    pub fn item(
        &self,
        index: usize,
        kind: ItemKind,
        plan: &ItemPlan<'_>,
    ) -> Result<ItemScratch, ScratchError> {
        let dir = self.dir.join("items").join(index.to_string());
        let root = item_root(&dir, &self.pulse)?;
        let dirs = ItemDirs::create(&dir)?;
        if kind == ItemKind::Pulse {
            self.fill_pulse_copy(&root, &dir, &dirs.run, plan)?;
        }
        let vendor_scratch = plan
            .adapter
            .scratch_vendor(&VendorScratchSpec {
                live_home: &self.live_home,
                live_root: &self.live_root,
                item_root: &root,
                dir: &dirs.vendor,
                profile: vendor_profile(kind),
            })
            .map_err(io_error("preparing the scratch vendor dir"))?;
        let watched = watch(&dir, vendor_scratch.watched);
        let env = dirs.child_env(vendor_scratch.env);
        let sandbox = self.item_sandbox(&dir, kind, plan);
        Ok(ItemScratch {
            dir,
            root,
            vendor: dirs.vendor,
            env,
            sandbox,
            tmp: dirs.tmp,
            watched,
            keep: self.keep,
        })
    }

    /// The pulse arm's working copy: the base copied in, its recall-echo
    /// config re-pointed at the item, and the CLI's project files checked.
    fn fill_pulse_copy(
        &self,
        root: &Path,
        dir: &Path,
        run_dir: &Path,
        plan: &ItemPlan<'_>,
    ) -> Result<(), ScratchError> {
        copy_tree(&self.base, root, Some(&self.live_root), CopyMode::Item)
            .map_err(io_error("copying the base into the item"))?;
        let graph = item_graph(plan.graph, dir)?;
        rewrite_recall_echo_config(root, &self.live_root, run_dir, &graph)?;
        check_project_items(plan.adapter.scratch_project(root, plan.recall_bin))?;
        Ok(())
    }

    /// The item child's sandbox: it writes only beneath the item's own
    /// directory (and the device files), and reads the fence's roots, plus
    /// the base's shared directories for the pulse arm.
    fn item_sandbox(&self, dir: &Path, kind: ItemKind, plan: &ItemPlan<'_>) -> Sandbox {
        let mut write_roots = vec![dir.to_path_buf()];
        write_roots.extend(
            fence::DEVICE_WRITES
                .iter()
                .map(PathBuf::from)
                .filter(|p| p.exists()),
        );
        let mut read_roots = plan.read_roots.to_vec();
        if kind == ItemKind::Pulse {
            read_roots.extend(SHARED_READ_ONLY.iter().map(|d| self.base.join(d)));
        }
        Sandbox {
            read_roots,
            write_roots,
            tcp_connect: plan.tcp_connect.to_vec(),
            tcp_bind: Vec::new(),
            background: true,
        }
    }

    /// Remove the scratch directory. A kept one loses only what holds a
    /// credential: the throwaway graph's probe password.
    pub fn teardown(mut self) -> io::Result<()> {
        self.removed = true;
        if self.keep {
            let _ = std::fs::remove_file(self.dir.join(GRAPH_PASSWORD_FILE));
            return Ok(());
        }
        std::fs::remove_dir_all(&self.dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.removed && !self.keep {
            if let Err(e) = std::fs::remove_dir_all(&self.dir) {
                tracing::warn!("probe scratch {} not removed: {e}", self.dir.display());
            }
        }
    }
}

/// One item's working copy, ready for one child.
#[derive(Debug)]
pub struct ItemScratch {
    dir: PathBuf,
    root: PathBuf,
    vendor: PathBuf,
    env: Vec<(OsString, OsString)>,
    sandbox: Sandbox,
    /// Where the child's prompt files are staged.
    tmp: PathBuf,
    /// Copied vendor files the CLI may rewrite, with their digest at copy time.
    watched: Vec<(PathBuf, String)>,
    keep: bool,
}

/// Create the item directory and its (empty) copy of the pulse, `<dir>/<pulse>`;
/// the copy's resolved path.
fn item_root(dir: &Path, pulse: &str) -> Result<PathBuf, ScratchError> {
    create_private_dir_all(dir).map_err(io_error("creating the item dir"))?;
    let root = dir.join(pulse);
    create_private_dir(&root).map_err(io_error("creating the item copy"))?;
    root.canonicalize()
        .map_err(io_error("resolving the item copy"))
}

/// An item's own directories ([`ITEM_DIRS`]): the vendor dir, and the
/// home, temp, cache, state, runtime, config and data dirs its child gets.
struct ItemDirs {
    vendor: PathBuf,
    tmp: PathBuf,
    cache: PathBuf,
    state: PathBuf,
    run: PathBuf,
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
}

impl ItemDirs {
    fn create(dir: &Path) -> Result<Self, ScratchError> {
        let mut dirs = Vec::new();
        for name in ITEM_DIRS {
            let path = dir.join(name);
            create_private_dir(&path).map_err(io_error(format!("creating {name}/")))?;
            dirs.push(path);
        }
        let [vendor, tmp, cache, state, run, home, config, data] =
            <[PathBuf; 8]>::try_from(dirs).expect("one path per item dir");
        Ok(Self {
            vendor,
            tmp,
            cache,
            state,
            run,
            home,
            config,
            data,
        })
    }

    /// The child's environment: the vendor scratch's, then its own home and
    /// XDG dirs.
    fn child_env(&self, vendor_env: Vec<(String, String)>) -> Vec<(OsString, OsString)> {
        let mut env: Vec<(OsString, OsString)> = vendor_env
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        // The child cannot read the live home, so it gets one of its own:
        // a CLI or tool that reads `~/.gitconfig` finds nothing rather than
        // a permission error.
        for (key, path) in [
            ("HOME", &self.home),
            ("XDG_CONFIG_HOME", &self.config),
            ("XDG_DATA_HOME", &self.data),
            ("TMPDIR", &self.tmp),
            ("XDG_CACHE_HOME", &self.cache),
            ("XDG_STATE_HOME", &self.state),
            ("XDG_RUNTIME_DIR", &self.run),
        ] {
            env.push((key.into(), path.as_os_str().to_owned()));
        }
        env
    }
}

/// How much of the live vendor dir an item's arm sees.
fn vendor_profile(kind: ItemKind) -> VendorProfile {
    match kind {
        ItemKind::Pulse => VendorProfile::Pulse,
        ItemKind::Bare => VendorProfile::Bare,
    }
}

/// The copied vendor files to check at teardown, with their digest now.
fn watch(dir: &Path, watched: Vec<PathBuf>) -> Vec<(PathBuf, String)> {
    watched
        .into_iter()
        .map(|path| {
            let digest = watched_digest(dir, &path).unwrap_or_default();
            (path, digest)
        })
        .collect()
}

/// What an item's teardown found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemTeardown {
    /// Copied vendor files the CLI changed during the item (a refreshed
    /// login token, say). They are reported, never copied back.
    pub changed_vendor_files: Vec<PathBuf>,
}

impl ItemScratch {
    /// The working copy: the CLI's cwd and `RECALL_ECHO_HOME`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The item's directory, the only place its child may write.
    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The item's scratch vendor directory.
    pub fn vendor(&self) -> &Path {
        &self.vendor
    }

    /// The env and sandbox every child of this item is spawned with.
    pub fn overrides(&self) -> SpawnOverrides {
        SpawnOverrides {
            env: self.env.clone(),
            sandbox: Some(self.sandbox.clone()),
            stage_dir: Some(self.tmp.clone()),
        }
    }

    /// Check the watched vendor files and remove the item (unless kept).
    /// A kept item still loses its copies of the vendor's login state: an
    /// inspection copy never holds a credential.
    pub fn finish(self) -> ItemTeardown {
        let changed_vendor_files = self
            .watched
            .iter()
            .filter(|(path, before)| watched_digest(&self.dir, path).unwrap_or_default() != *before)
            .map(|(path, _)| path.clone())
            .collect();
        if self.keep {
            // Never through a link the child made: a vendor dir it swapped
            // for a link to the live one makes this fail, not reach it.
            for (path, _) in &self.watched {
                if let Ok(rel) = path.strip_prefix(&self.dir) {
                    if let Err(e) = crate::fs_safe::unlink_beneath(&self.dir, rel) {
                        tracing::warn!("kept item: {} not removed: {e}", path.display());
                    }
                }
            }
        } else if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            tracing::warn!("probe item {} not removed: {e}", self.dir.display());
        }
        ItemTeardown {
            changed_vendor_files,
        }
    }
}

/// A file name that never reaches a scratch copy, wherever it sits.
fn excluded_anywhere(name: &str) -> bool {
    name == "secrets"
        || name.starts_with(".env")
        || name.ends_with(".lock")
        || name.ends_with(".pem")
        || name.ends_with(".key")
}

/// A top-level entry of the live home that never reaches a scratch copy.
fn excluded_at_top(name: &str, is_dir: bool) -> bool {
    if is_dir {
        EXCLUDED_TOP_DIRS.contains(&name)
    } else {
        EXCLUDED_TOP_FILES.contains(&name) || name.starts_with(CONFIG_PREFIX)
    }
}

/// How one tree copy treats what it copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyMode {
    /// The base copy of the live home: the exclusions apply.
    Filtered,
    /// An item's copy of the base: the archives are shared read-only
    /// instead of copied ([`SHARED_READ_ONLY`]).
    Item,
}

/// Directories an item reads from the base copy instead of copying: a
/// symlink to the base, which the item's sandbox may read and not write.
/// The archives are the bulk of a pulse home (Echo's ~300 MB) and nothing
/// a probe turn runs writes there.
const SHARED_READ_ONLY: &[&str] = &["archives"];

/// Copy `src` into `dst` (which exists). With `rebase_from`, absolute
/// symlinks beneath it are re-pointed at the same place beneath `dst`;
/// other links are copied as they are. Sockets, FIFOs and devices are
/// skipped.
fn copy_tree(src: &Path, dst: &Path, rebase_from: Option<&Path>, mode: CopyMode) -> io::Result<()> {
    let tree = CopyTree {
        src_root: src,
        dst_root: dst,
        rebase_from,
        mode,
    };
    tree.copy_dir(src, dst)
}

/// One tree copy's fixed parameters.
struct CopyTree<'a> {
    src_root: &'a Path,
    dst_root: &'a Path,
    rebase_from: Option<&'a Path>,
    mode: CopyMode,
}

impl CopyTree<'_> {
    fn copy_dir(&self, src: &Path, dst: &Path) -> io::Result<()> {
        for entry in std::fs::read_dir(src)? {
            self.copy_entry(src, dst, &entry?)?;
        }
        Ok(())
    }

    /// Copy one entry of `src` into `dst`, unless the copy leaves it out.
    fn copy_entry(&self, src: &Path, dst: &Path, entry: &std::fs::DirEntry) -> io::Result<()> {
        let name = entry.file_name();
        let kind = entry.file_type()?;
        let path = entry.path();
        let relative = path.strip_prefix(self.src_root).unwrap_or(&path);
        if self.leaves_out(src, &name.to_string_lossy(), kind, relative) {
            return Ok(());
        }
        self.place(kind, &path, &dst.join(&name), relative)
    }

    /// The base copy's exclusions (an item's copy of the base excludes
    /// nothing more).
    fn leaves_out(&self, src: &Path, name: &str, kind: std::fs::FileType, relative: &Path) -> bool {
        self.mode == CopyMode::Filtered
            && (excluded_anywhere(name)
                || (src == self.src_root && excluded_at_top(name, kind.is_dir()))
                || relative == Path::new(EMBEDDED_GRAPH_STORE))
    }

    /// Whether an item shares this directory with the base read-only
    /// instead of copying it ([`SHARED_READ_ONLY`]).
    fn shares(&self, kind: std::fs::FileType, relative: &Path) -> bool {
        self.mode == CopyMode::Item
            && kind.is_dir()
            && SHARED_READ_ONLY.iter().any(|d| relative == Path::new(d))
    }

    /// Put `path` at `to`: a shared directory as a link to the base, a
    /// symlink re-pointed or as it is, a directory copied through, a file
    /// copied. Sockets, FIFOs and devices are skipped.
    fn place(
        &self,
        kind: std::fs::FileType,
        path: &Path,
        to: &Path,
        relative: &Path,
    ) -> io::Result<()> {
        // The embedding model cache is copied like everything else: a hard
        // link would share one inode across the base and every item, so a
        // child writing through it would tamper with the model the next
        // items load (audit R2-12).
        if self.shares(kind, relative) {
            std::os::unix::fs::symlink(path, to)
        } else if kind.is_symlink() {
            self.copy_link(path, to)
        } else if kind.is_dir() {
            create_private_dir(to)?;
            self.copy_dir(path, to)
        } else if kind.is_file() {
            std::fs::copy(path, to)
                .map(|_| ())
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", relative.display())))
        } else {
            Ok(())
        }
    }

    /// Copy the symlink `path` to `to`, an absolute target beneath
    /// `rebase_from` re-pointed at the same place beneath the copy.
    fn copy_link(&self, path: &Path, to: &Path) -> io::Result<()> {
        let target = std::fs::read_link(path)?;
        let rebased = self
            .rebase_from
            .filter(|_| target.is_absolute())
            .and_then(|from| target.strip_prefix(from).ok())
            .map(|inside| self.dst_root.join(inside));
        std::os::unix::fs::symlink(rebased.unwrap_or(target), to)
    }
}

/// The graph an item's copy points at. For the run's throwaway server, the
/// probe user's password is copied into the item's own directory (0600):
/// the run's copy sits in the scratch dir, outside the child's read fence,
/// and recall-echo in the child must be able to open its graph (audit R2-3).
fn item_graph(graph: &GraphTarget, item_dir: &Path) -> Result<GraphTarget, ScratchError> {
    let GraphTarget::Server {
        addr,
        namespace,
        database,
        username,
        password_file,
    } = graph
    else {
        return Ok(graph.clone());
    };
    let password = crate::fs_safe::read_bounded(password_file, 4096)
        .map_err(io_error("reading the probe graph password"))?
        .ok_or_else(|| ScratchError::Io {
            context: "reading the probe graph password".into(),
            source: io::Error::new(io::ErrorKind::NotFound, password_file.display().to_string()),
        })?;
    let own = item_dir.join(GRAPH_PASSWORD_FILE);
    write_private_file(&own, password.as_bytes())
        .map_err(io_error("writing the item's graph password"))?;
    Ok(GraphTarget::Server {
        addr: addr.clone(),
        namespace: namespace.clone(),
        database: database.clone(),
        username: username.clone(),
        password_file: own,
    })
}

/// Create `path` (which must not exist) 0600 with `bytes`.
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

/// Rewrite the item copy's recall-echo config so nothing it runs reaches a
/// live store: capture, pipeline sync and background extraction off, the
/// daemon socket inside the scratch, the docs dir and graph re-pointed. A
/// pulse without the file gets one, since recall-echo's defaults capture.
fn rewrite_recall_echo_config(
    item_root: &Path,
    live_root: &Path,
    run_dir: &Path,
    graph: &GraphTarget,
) -> Result<(), ScratchError> {
    let path = item_root.join(RECALL_ECHO_CONFIG);
    let unsafe_file = |reason: String| ScratchError::UnsafeProjectFile {
        path: path.display().to_string(),
        reason,
    };
    let text = crate::fs_safe::read_bounded(&path, 1024 * 1024)
        .map_err(|e| unsafe_file(e.to_string()))?
        .unwrap_or_default();
    let mut doc: toml::Table = toml::from_str(&text)
        .map_err(|e| unsafe_file(crate::plugins::config_error::describe(&e)))?;
    let rewritten = scratch_recall_echo_config(&mut doc, item_root, live_root, run_dir, graph);
    rewritten.map_err(unsafe_file)?;
    let rendered = toml::to_string(&doc).map_err(|e| unsafe_file(e.to_string()))?;
    let memory = item_root.join("memory");
    create_private_dir_all(&memory).map_err(io_error("creating memory/"))?;
    crate::init::agent_bootstrap::write_regular_file(&path, &rendered, item_root)
        .map_err(unsafe_file)
}

/// The rewrite itself, on a parsed document.
fn scratch_recall_echo_config(
    doc: &mut toml::Table,
    item_root: &Path,
    live_root: &Path,
    run_dir: &Path,
    graph: &GraphTarget,
) -> Result<(), String> {
    toml_table(doc, "capture")?.insert("enabled".into(), false.into());
    toml_table(doc, "extraction")?.insert("background_enabled".into(), false.into());
    let pipeline = toml_table(doc, "pipeline")?;
    pipeline.insert("auto_sync".into(), false.into());
    rebase_docs_dir(pipeline, item_root, live_root);
    let serve = toml_table(doc, "serve")?;
    serve.insert("socket_path".into(), path_value(&run_dir.join("re.sock")));
    serve.insert(
        "idle_timeout_secs".into(),
        toml::Value::Integer(SCRATCH_DAEMON_IDLE_SECS),
    );
    point_graph(toml_table(doc, "graph")?, graph);
    Ok(())
}

/// The table `key` of `doc`, made when missing; an error when `key` holds
/// something else.
fn toml_table<'a>(doc: &'a mut toml::Table, key: &str) -> Result<&'a mut toml::Table, String> {
    doc.entry(key)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("[{key}] is not a table"))
}

fn path_value(path: &Path) -> toml::Value {
    toml::Value::String(path.display().to_string())
}

/// A `docs_dir` beneath the live home moves to the same place in the item;
/// any other moves to the item's `journal/`.
fn rebase_docs_dir(pipeline: &mut toml::Table, item_root: &Path, live_root: &Path) {
    if let Some(docs) = pipeline.get("docs_dir").and_then(toml::Value::as_str) {
        let rebased = match Path::new(docs).strip_prefix(live_root) {
            Ok(inside) => item_root.join(inside),
            Err(_) => item_root.join("journal"),
        };
        pipeline.insert("docs_dir".into(), path_value(&rebased));
    }
}

/// Point `[graph]` at the item's graph, nothing of the live one left.
fn point_graph(graph_table: &mut toml::Table, graph: &GraphTarget) {
    for key in ["url", "namespace", "database", "username", "password_file"] {
        graph_table.remove(key);
    }
    match graph {
        GraphTarget::Embedded => {
            graph_table.insert("mode".into(), "embedded".into());
        }
        GraphTarget::Server {
            addr,
            namespace,
            database,
            username,
            password_file,
        } => {
            graph_table.insert("mode".into(), "server".into());
            graph_table.insert("url".into(), addr.as_str().into());
            graph_table.insert("namespace".into(), namespace.as_str().into());
            graph_table.insert("database".into(), database.as_str().into());
            graph_table.insert("username".into(), username.as_str().into());
            graph_table.insert("password_file".into(), path_value(password_file));
        }
    }
}

/// When the scratch run `id` under `parent` started, if it exists: its
/// stamp, else its directory's mtime.
fn scratch_started(parent: &Path, id: &str) -> Option<std::time::SystemTime> {
    let dir = parent.join(id);
    let stamped = std::fs::read_to_string(dir.join(CREATED_FILE))
        .ok()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t.trim()).ok())
        .map(std::time::SystemTime::from);
    stamped.or_else(|| std::fs::metadata(&dir).and_then(|m| m.modified()).ok())
}

/// Is the scratch run `id` still running (present and younger than `max_age`)?
pub fn is_live_scratch(parent: &Path, id: &str, max_age: std::time::Duration) -> bool {
    scratch_started(parent, id)
        .and_then(|started| started.elapsed().ok())
        .is_some_and(|age| age < max_age)
}

/// The host lock's file, in the scratch parent (shared by every pulse of
/// the user): one probe run at a time on this host.
pub const HOST_LOCK: &str = "host.lock";

/// Sweep stale scratch only while holding the host lock, taken without
/// waiting: then no other run is live, so nothing it is still using can be
/// mistaken for stale (audit R2-12). When another run holds the lock this
/// sweep is skipped; that run, or the next, sweeps.
pub fn sweep_under_host_lock(parent: &Path, max_age: std::time::Duration) -> Vec<PathBuf> {
    if create_private_dir_all(parent).is_err() {
        return Vec::new();
    }
    match super::record::try_lock_file(&parent.join(HOST_LOCK)) {
        Ok(Some(_lock)) => sweep_stale_scratch(parent, max_age),
        _ => Vec::new(),
    }
}

/// Remove scratch dirs under `parent` older than `max_age`: what a crashed
/// run left behind. Returns the dirs removed.
pub fn sweep_stale_scratch(parent: &Path, max_age: std::time::Duration) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let id = entry.file_name().to_string_lossy().into_owned();
        // Only a directory (never a link) named like a scratch id, holding
        // its stamp, is a scratch: nothing else in the parent is touched.
        if !kind.is_dir()
            || !is_scratch_id(&id)
            || !entry.path().join(CREATED_FILE).is_file()
            || is_live_scratch(parent, &id, max_age)
        {
            continue;
        }
        super::graph::kill_stale_server(&entry.path());
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => removed.push(entry.path()),
            Err(e) => tracing::warn!(
                "stale probe scratch {} not removed: {e}",
                entry.path().display()
            ),
        }
    }
    removed
}

/// Every project file the adapter rewrote must have been rewritten.
fn check_project_items(
    items: Vec<crate::init::agent_bootstrap::BootstrapItem>,
) -> Result<(), ScratchError> {
    for item in items {
        if let ItemStatus::Skipped(reason) | ItemStatus::Wrong(reason) = item.status {
            return Err(ScratchError::UnsafeProjectFile {
                path: item.path.display().to_string(),
                reason,
            });
        }
    }
    Ok(())
}

/// Largest watched vendor file digested; anything bigger counts as changed.
const MAX_WATCHED_BYTES: u64 = 4 * 1024 * 1024;

/// Hex SHA-256 of a watched vendor file in the item `dir`, read with no
/// symlink followed anywhere beneath `dir` (a child could swap the vendor
/// directory itself for a link) and only up to a bound.
fn watched_digest(dir: &Path, path: &Path) -> io::Result<String> {
    let rel = path
        .strip_prefix(dir)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "not in the item"))?;
    let bytes = crate::fs_safe::read_beneath(dir, rel, MAX_WATCHED_BYTES)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "gone"))?;
    Ok(hex(&Sha256::digest(bytes)))
}

/// Hex SHA-256 of a file's bytes. The file is a probe child's to replace,
/// so it is never followed through a link and never read past a bound.
#[cfg(test)]
pub fn file_digest(path: &Path) -> io::Result<String> {
    let text = crate::fs_safe::read_bounded_bytes(path, MAX_WATCHED_BYTES)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "gone"))?;
    Ok(hex(&Sha256::digest(text)))
}

/// A scratch id: 12 lowercase hex digits.
fn is_scratch_id(name: &str) -> bool {
    name.len() == 12
        && name
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// A name that is safe as one path component.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Hex encoding of a digest.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// A digest of a whole tree: every path (sorted), its kind, and its
/// contents or link target. Two equal digests mean nothing was added,
/// removed or changed. Unreadable entries count as their error. Top-level
/// entries named in `skip` are left out.
#[cfg(test)]
pub fn tree_digest(root: &Path, skip: &[&str]) -> String {
    let mut lines = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            e.depth() != 1
                || !skip
                    .iter()
                    .any(|s| e.file_name() == std::ffi::OsStr::new(s))
        })
    {
        let line = match entry {
            Ok(entry) => {
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .display()
                    .to_string();
                let kind = entry.file_type();
                if kind.is_symlink() {
                    let target = std::fs::read_link(path)
                        .map(|t| t.display().to_string())
                        .unwrap_or_default();
                    format!("l {relative} {target}")
                } else if kind.is_dir() {
                    format!("d {relative}")
                } else {
                    let digest = file_digest(path).unwrap_or_else(|e| e.to_string());
                    format!("f {relative} {digest}")
                }
            }
            Err(e) => format!("! {e}"),
        };
        lines.push(line);
    }
    hex(&Sha256::digest(lines.join("\n").as_bytes()))
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cli_provider::adapters::{claude::Claude, grok::Grok};
    use crate::init::agent_bootstrap::ensure_common;

    /// A fixture pulse with everything the copy must leave behind.
    pub(crate) fn fixture_pulse() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in [
            "secrets",
            "record/probes",
            "sessions",
            "coordinator",
            "logs",
            "memory/graph/surreal",
            "archives/conversations",
            "journal",
            "notes/logs",
        ] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        for (file, body) in [
            ("secrets/graph-password", "fixture"),
            ("record/probes/runs.jsonl", "{}\n"),
            ("sessions/owner.json", "{}"),
            ("coordinator/lease", "x"),
            ("logs/a.log", "x"),
            ("memory/graph/surreal/data", "x"),
            ("memory/graph/stats-cache.json", "{}"),
            ("pulse-null.toml", "[pulse]\nname = \"fixture\"\n"),
            ("pulse-null.toml.bak-1", "old"),
            (".pulse-null.pid", "1"),
            ("ISOLATION", "{}"),
            ("intents.json.lock", ""),
            (".env", "X=1"),
            ("SELF.md", "I am a fixture."),
            ("INSTRUCTIONS.md", "Be a fixture."),
            ("memory/MEMORY.md", "remember"),
            ("memory/EPHEMERAL.md", "recent"),
            ("archives/conversations/conversation-001.md", "hello"),
            (
                "notes/logs/kept.md",
                "nested logs/ is not the top-level logs/",
            ),
        ] {
            std::fs::write(root.join(file), body).unwrap();
        }
        ensure_common(&root, "claude-code");
        crate::cli_provider::adapters::claude::Claude
            .integration()
            .ensure(&root, "/usr/local/bin/recall-echo");
        std::os::unix::fs::symlink(root.join("journal"), root.join("journal-link")).unwrap();
        dir
    }

    pub(crate) struct Fixture {
        pub live: tempfile::TempDir,
        pub home: tempfile::TempDir,
        pub state: tempfile::TempDir,
    }

    impl Fixture {
        pub(crate) fn new() -> Self {
            Self {
                live: fixture_pulse(),
                home: tempfile::tempdir().unwrap(),
                state: tempfile::tempdir().unwrap(),
            }
        }

        pub(crate) fn root(&self) -> PathBuf {
            self.live.path().canonicalize().unwrap()
        }

        pub(crate) fn scratch(&self, keep: bool) -> Scratch {
            let root = self.root();
            Scratch::create(&ScratchSpec {
                live_root: &root,
                live_home: self.home.path(),
                pulse: "fixture",
                parent: &self.state.path().join("s"),
                keep,
            })
            .unwrap()
        }
    }

    pub(crate) fn plan<'a>(adapter: &'a dyn CliAdapter, graph: &'a GraphTarget) -> ItemPlan<'a> {
        ItemPlan {
            adapter,
            recall_bin: "/usr/local/bin/recall-echo",
            graph,
            tcp_connect: &[443],
            read_roots: test_read_roots(),
        }
    }

    /// The system part of the read fence, for tests that spawn a shell.
    pub(crate) fn test_read_roots() -> &'static [PathBuf] {
        static ROOTS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
        ROOTS.get_or_init(fence::system_reads)
    }

    #[test]
    fn the_base_copy_leaves_secrets_record_and_live_state_behind() {
        let fixture = Fixture::new();
        let scratch = fixture.scratch(false);
        let base = scratch.base();
        for gone in [
            "secrets",
            "record",
            "sessions",
            "coordinator",
            "logs",
            "memory/graph/surreal",
            "pulse-null.toml",
            "pulse-null.toml.bak-1",
            ".pulse-null.pid",
            "ISOLATION",
            "intents.json.lock",
            ".env",
        ] {
            assert!(!base.join(gone).exists(), "{gone} must not be copied");
        }
        for kept in [
            "SELF.md",
            "memory/MEMORY.md",
            "archives/conversations/conversation-001.md",
            "notes/logs/kept.md",
            ".claude/settings.json",
            "memory/graph/stats-cache.json",
        ] {
            assert!(base.join(kept).exists(), "{kept} must be copied");
        }
    }

    #[test]
    fn an_item_copy_is_rewritten_to_stay_inside_the_scratch() {
        let fixture = Fixture::new();
        let scratch = fixture.scratch(false);
        let graph = GraphTarget::Embedded;
        let item = scratch
            .item(0, ItemKind::Pulse, &plan(&Claude, &graph))
            .unwrap();
        let root = item.root();
        assert!(root.starts_with(scratch.dir()));

        let config: toml::Table =
            toml::from_str(&std::fs::read_to_string(root.join(RECALL_ECHO_CONFIG)).unwrap())
                .unwrap();
        assert_eq!(config["capture"]["enabled"].as_bool(), Some(false));
        assert_eq!(config["pipeline"]["auto_sync"].as_bool(), Some(false));
        assert_eq!(
            config["extraction"]["background_enabled"].as_bool(),
            Some(false)
        );
        assert_eq!(
            config["pipeline"]["docs_dir"].as_str(),
            Some(root.join("journal").display().to_string().as_str())
        );
        let socket = config["serve"]["socket_path"].as_str().unwrap();
        assert!(Path::new(socket).starts_with(item.dir()), "{socket}");
        assert_eq!(config["graph"]["mode"].as_str(), Some("embedded"));
        assert!(config["graph"].get("database").is_none());

        let settings = std::fs::read_to_string(root.join(".claude/settings.json")).unwrap();
        assert!(!settings.contains("archive-session"), "{settings}");
        assert!(!settings.contains("checkpoint"), "{settings}");
        assert!(!settings.contains(&fixture.root().display().to_string()));
        assert!(settings.contains("consume"));

        let link = std::fs::read_link(root.join("journal-link")).unwrap();
        assert_eq!(
            link,
            root.join("journal"),
            "links into the live home are re-pointed"
        );
    }

    /// Audit #14: an item shares the bulk directories with the base instead
    /// of copying them; the read-only one stays unwritable.
    #[test]
    fn an_item_shares_the_archives_and_copies_the_model_cache() {
        use std::os::unix::fs::MetadataExt;
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.root().join("memory/graph/models/m")).unwrap();
        std::fs::write(fixture.root().join("memory/graph/models/m/blob"), "weights").unwrap();
        let scratch = fixture.scratch(false);
        let graph = GraphTarget::Embedded;
        let item = scratch
            .item(0, ItemKind::Pulse, &plan(&Claude, &graph))
            .unwrap();
        let archives = item.root().join("archives");
        assert_eq!(
            std::fs::read_link(&archives).unwrap(),
            scratch.base().join("archives")
        );
        assert_eq!(
            std::fs::read_to_string(archives.join("conversations/conversation-001.md")).unwrap(),
            "hello"
        );
        let blob = |root: &Path| {
            std::fs::metadata(root.join("memory/graph/models/m/blob"))
                .unwrap()
                .ino()
        };
        assert_ne!(
            blob(item.root()),
            blob(scratch.base()),
            "the model cache is copied, never hard-linked (R2-12)"
        );
        std::fs::write(item.root().join("memory/graph/models/m/blob"), "tampered").unwrap();
        assert_eq!(
            std::fs::read_to_string(scratch.base().join("memory/graph/models/m/blob")).unwrap(),
            "weights",
            "an item's write reaches no other item"
        );
        let sandbox = item.overrides().sandbox.unwrap();
        assert!(sandbox
            .read_roots
            .contains(&scratch.base().join("archives")));
        assert!(!sandbox
            .write_roots
            .iter()
            .any(|r| scratch.base().starts_with(r) || r.starts_with(scratch.base())));
    }

    #[test]
    fn item_env_points_every_writable_location_into_the_item() {
        let fixture = Fixture::new();
        let scratch = fixture.scratch(false);
        let graph = GraphTarget::Embedded;
        let item = scratch
            .item(3, ItemKind::Pulse, &plan(&Claude, &graph))
            .unwrap();
        let overrides = item.overrides();
        for key in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "TMPDIR",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
            "CLAUDE_CONFIG_DIR",
        ] {
            let value = overrides
                .env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| PathBuf::from(v))
                .unwrap_or_else(|| panic!("{key} set"));
            assert!(value.starts_with(item.dir()), "{key}={}", value.display());
        }
        let sandbox = overrides.sandbox.unwrap();
        assert_eq!(sandbox.write_roots[0], item.dir());
        assert!(sandbox
            .write_roots
            .iter()
            .all(|r| r.starts_with(item.dir()) || r.starts_with("/dev")));
        assert_eq!(sandbox.tcp_connect, vec![443]);
        assert!(sandbox.tcp_bind.is_empty());
        assert!(
            !sandbox
                .read_roots
                .iter()
                .any(|r| fixture.root().starts_with(r) || r.starts_with(fixture.home.path())),
            "nothing live is readable"
        );
        assert!(overrides.stage_dir.unwrap().starts_with(item.dir()));
    }

    #[test]
    fn a_bare_item_is_an_empty_directory() {
        let fixture = Fixture::new();
        let scratch = fixture.scratch(false);
        let graph = GraphTarget::Embedded;
        let item = scratch
            .item(0, ItemKind::Bare, &plan(&Grok, &graph))
            .unwrap();
        assert_eq!(std::fs::read_dir(item.root()).unwrap().count(), 0);
        assert!(item.overrides().env.iter().any(|(k, _)| k == "GROK_HOME"));
    }

    #[test]
    fn an_adapter_without_a_scratch_layout_is_refused() {
        let fixture = Fixture::new();
        let scratch = fixture.scratch(false);
        let graph = GraphTarget::Embedded;
        let codex = crate::cli_provider::adapters::codex::Codex;
        let err = scratch
            .item(0, ItemKind::Pulse, &plan(&codex, &graph))
            .unwrap_err();
        assert!(err.to_string().contains("no probe scratch layout"), "{err}");
    }

    #[test]
    fn teardown_removes_the_scratch_and_keep_leaves_it() {
        let fixture = Fixture::new();
        let scratch = fixture.scratch(false);
        let dir = scratch.dir().to_path_buf();
        scratch.teardown().unwrap();
        assert!(!dir.exists());

        let kept = fixture.scratch(true);
        let dir = kept.dir().to_path_buf();
        kept.teardown().unwrap();
        assert!(dir.exists());

        let dropped = fixture.scratch(false);
        let dir = dropped.dir().to_path_buf();
        drop(dropped);
        assert!(!dir.exists(), "a dropped scratch cleans up after itself");
    }

    /// Audit #20: a kept scratch never keeps a copied login.
    #[test]
    fn a_kept_scratch_drops_its_copied_login_state() {
        let fixture = Fixture::new();
        let live_grok = fixture.home.path().join(".grok");
        std::fs::create_dir_all(&live_grok).unwrap();
        std::fs::write(live_grok.join("auth.json"), "fixture-login").unwrap();
        let scratch = fixture.scratch(true);
        std::fs::write(scratch.dir().join(GRAPH_PASSWORD_FILE), "fixture").unwrap();
        let graph = GraphTarget::Embedded;
        let item = scratch
            .item(0, ItemKind::Pulse, &plan(&Grok, &graph))
            .unwrap();
        let copied = item.vendor().join("grok/auth.json");
        let root = item.root().to_path_buf();
        assert!(copied.exists());
        item.finish();
        assert!(!copied.exists(), "the login copy goes");
        assert!(root.join("SELF.md").exists(), "the working copy stays");
        let dir = scratch.dir().to_path_buf();
        scratch.teardown().unwrap();
        assert!(dir.exists());
        assert!(!dir.join(GRAPH_PASSWORD_FILE).exists());
    }

    #[test]
    fn a_changed_watched_vendor_file_is_reported_not_copied_back() {
        let fixture = Fixture::new();
        let live_grok = fixture.home.path().join(".grok");
        std::fs::create_dir_all(&live_grok).unwrap();
        std::fs::write(live_grok.join("login-state"), "fixture-v1").unwrap();
        let scratch = fixture.scratch(false);
        let graph = GraphTarget::Embedded;
        let item = scratch
            .item(0, ItemKind::Pulse, &plan(&Grok, &graph))
            .unwrap();
        std::fs::write(item.vendor().join("grok/login-state"), "fixture-v2").unwrap();
        let teardown = item.finish();
        assert_eq!(teardown.changed_vendor_files.len(), 1);
        assert_eq!(
            std::fs::read_to_string(live_grok.join("login-state")).unwrap(),
            "fixture-v1"
        );
    }

    #[test]
    fn tree_digest_sees_additions_and_edits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "1").unwrap();
        let before = tree_digest(dir.path(), &[]);
        assert_eq!(before, tree_digest(dir.path(), &[]));
        std::fs::write(dir.path().join("a"), "2").unwrap();
        let edited = tree_digest(dir.path(), &[]);
        assert_ne!(before, edited);
        std::fs::write(dir.path().join("b"), "").unwrap();
        assert_ne!(edited, tree_digest(dir.path(), &[]));
        assert_eq!(
            tree_digest(dir.path(), &["b"]),
            edited,
            "skipped entries do not count"
        );
    }

    #[test]
    fn a_server_graph_target_points_the_copy_at_the_probe_database() {
        let fixture = Fixture::new();
        let scratch = fixture.scratch(false);
        std::fs::write(scratch.dir().join(GRAPH_PASSWORD_FILE), "fixture-pw").unwrap();
        let graph = GraphTarget::Server {
            addr: "127.0.0.1:8787".into(),
            namespace: "nullarc_probe".into(),
            database: "probe_fixture_abc".into(),
            username: "probe_abc".into(),
            password_file: scratch.dir().join(GRAPH_PASSWORD_FILE),
        };
        let item = scratch
            .item(0, ItemKind::Pulse, &plan(&Claude, &graph))
            .unwrap();
        let config: toml::Table =
            toml::from_str(&std::fs::read_to_string(item.root().join(RECALL_ECHO_CONFIG)).unwrap())
                .unwrap();
        let g = &config["graph"];
        assert_eq!(g["mode"].as_str(), Some("server"));
        assert_eq!(g["namespace"].as_str(), Some("nullarc_probe"));
        assert_eq!(g["database"].as_str(), Some("probe_fixture_abc"));
        assert_eq!(g["username"].as_str(), Some("probe_abc"));
        // R2-3: the password the child reads is its own copy, inside the
        // item (its read fence), private to the owner.
        let own = Path::new(g["password_file"].as_str().unwrap());
        assert!(own.starts_with(item.dir()), "{}", own.display());
        assert_eq!(std::fs::read_to_string(own).unwrap(), "fixture-pw");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(own).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// Audit #19: the sweep touches only stamped scratch dirs, never a link
    /// or anything else that sits in the parent.
    #[test]
    fn the_sweep_leaves_everything_but_stamped_scratch_alone() {
        let parent = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let long_ago = (chrono::Utc::now() - chrono::Duration::hours(7)).to_rfc3339();
        let unrelated = parent.path().join("not-a-scratch");
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::write(unrelated.join(CREATED_FILE), &long_ago).unwrap();
        let unstamped = parent.path().join("cccccccccccc");
        std::fs::create_dir_all(&unstamped).unwrap();
        std::fs::write(elsewhere.path().join(CREATED_FILE), &long_ago).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), parent.path().join("dddddddddddd")).unwrap();
        let six_hours = std::time::Duration::from_secs(6 * 3600);
        assert!(sweep_stale_scratch(parent.path(), six_hours).is_empty());
        assert!(unrelated.exists() && unstamped.exists());
        assert!(elsewhere.path().join(CREATED_FILE).exists());
    }

    #[test]
    fn a_pulse_name_that_is_not_a_plain_name_is_refused() {
        let fixture = Fixture::new();
        let root = fixture.root();
        for bad in ["../escape", "a/b", ".hidden", ""] {
            assert!(
                Scratch::create(&ScratchSpec {
                    live_root: &root,
                    live_home: fixture.home.path(),
                    pulse: bad,
                    parent: &fixture.state.path().join("s"),
                    keep: false,
                })
                .is_err(),
                "{bad:?}"
            );
        }
    }

    /// R2-12: a kept item whose vendor directory the child swapped for a
    /// link to the live one: teardown reaches nothing live, to read or to
    /// unlink.
    #[test]
    fn a_kept_item_never_unlinks_through_a_child_made_link() {
        let fixture = Fixture::new();
        let live_grok = fixture.home.path().join(".grok");
        std::fs::create_dir_all(&live_grok).unwrap();
        std::fs::write(live_grok.join("auth.json"), "live-login").unwrap();
        let scratch = fixture.scratch(true);
        let graph = GraphTarget::Embedded;
        let item = scratch
            .item(0, ItemKind::Pulse, &plan(&Grok, &graph))
            .unwrap();
        let vendor = item.vendor().join("grok");
        std::fs::remove_dir_all(&vendor).unwrap();
        std::os::unix::fs::symlink(&live_grok, &vendor).unwrap();
        let teardown = item.finish();
        assert_eq!(
            std::fs::read_to_string(live_grok.join("auth.json")).unwrap(),
            "live-login",
            "the live login survives"
        );
        assert_eq!(
            teardown.changed_vendor_files.len(),
            1,
            "reported as changed"
        );
    }

    #[test]
    fn a_watched_file_swapped_for_a_link_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("auth.json");
        std::os::unix::fs::symlink("/dev/zero", &watched).unwrap();
        assert!(
            file_digest(&watched).is_err(),
            "a link is refused, not read"
        );
    }

    #[test]
    fn the_sweep_removes_only_stale_scratch_dirs() {
        let parent = tempfile::tempdir().unwrap();
        let stale = parent.path().join("aaaaaaaaaaaa");
        let fresh = parent.path().join("bbbbbbbbbbbb");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        let long_ago = chrono::Utc::now() - chrono::Duration::hours(7);
        std::fs::write(stale.join(CREATED_FILE), long_ago.to_rfc3339()).unwrap();
        std::fs::write(fresh.join(CREATED_FILE), chrono::Utc::now().to_rfc3339()).unwrap();
        let six_hours = std::time::Duration::from_secs(6 * 3600);
        assert!(!is_live_scratch(parent.path(), "aaaaaaaaaaaa", six_hours));
        assert!(is_live_scratch(parent.path(), "bbbbbbbbbbbb", six_hours));
        assert!(!is_live_scratch(parent.path(), "cccccccccccc", six_hours));
        assert_eq!(
            sweep_stale_scratch(parent.path(), six_hours),
            vec![stale.clone()]
        );
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn the_sweep_waits_for_no_one_and_skips_while_a_run_holds_the_host_lock() {
        let parent = tempfile::tempdir().unwrap();
        let stale = parent.path().join("aaaaaaaaaaaa");
        std::fs::create_dir_all(&stale).unwrap();
        let long_ago = chrono::Utc::now() - chrono::Duration::hours(7);
        std::fs::write(stale.join(CREATED_FILE), long_ago.to_rfc3339()).unwrap();
        let six_hours = std::time::Duration::from_secs(6 * 3600);
        let held = super::super::record::try_lock_file(&parent.path().join(HOST_LOCK))
            .unwrap()
            .unwrap();
        assert!(sweep_under_host_lock(parent.path(), six_hours).is_empty());
        assert!(stale.exists(), "a running run's host is not swept");
        drop(held);
        assert_eq!(
            sweep_under_host_lock(parent.path(), six_hours),
            vec![stale.clone()]
        );
        assert!(!stale.exists());
    }

    #[test]
    fn probe_state_stays_out_of_a_pulse_home_when_the_unit_sets_it() {
        let pulse_home = Path::new("/home/pulse");
        let set = state_root(Some(OsStr::new("/var/lib/pulse-null-probe")), pulse_home);
        assert_eq!(set, Path::new("/var/lib/pulse-null-probe"));
        assert!(!set.starts_with(pulse_home));
        let own = state_root(None, Path::new("/var/lib/pulse-null-probe"));
        assert!(own.starts_with("/var/lib/pulse-null-probe"));
        assert!(!own.starts_with(pulse_home));
        let record = probe_record_root(&set, "Echo").unwrap();
        assert_eq!(
            record,
            Path::new("/var/lib/pulse-null-probe/echo"),
            "the record root is the probe state, not the live home"
        );
        assert!(probe_record_root(&set, "../x").is_err());
        assert_eq!(
            probe_scratch_parent(&set),
            Path::new("/var/lib/pulse-null-probe/s")
        );
    }
}
