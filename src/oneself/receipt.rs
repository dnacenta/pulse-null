//! The daemon's receipt writer.
//!
//! One task appends for a pulse. Turns hand it rows through a bounded
//! channel and never wait on the file. A full channel or a failed append
//! is counted and later written as one `gap` row. Receipts that are off
//! do not open a file.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use super::chain;
use super::event::{ActionClass, FdSource, Identity, Outcome, ReceiptEvent};
use super::protect::{Protection, ProtectionProbe};
use super::store::{
    default_receipts_path, ListenEnv, OpenChoice, ReceiptStore, StoreError, StoreSource,
};
use crate::config::{receipts_mode, Config};

const CAPACITY: usize = 1024;
const STATE_OFF: u8 = 0;
const STATE_OK: u8 = 1;
const STATE_DEGRADED: u8 = 2;
const STATE_UNPROTECTED: u8 = 3;
const STATE_UNAVAILABLE: u8 = 4;

/// Digests computed for a receipt row. Stays put when receipts are off.
#[cfg(test)]
pub(crate) static DIGESTS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn digest(bytes: &[u8]) -> String {
    #[cfg(test)]
    {
        if FORBID_DIGEST.with(std::cell::Cell::get) {
            panic!("digest while receipts are off");
        }
        DIGESTS.fetch_add(1, Ordering::Relaxed);
    }
    chain::sha256_hex(bytes)
}

// When set on this thread, `digest` panics. Other threads are unaffected.
#[cfg(test)]
thread_local! {
    static FORBID_DIGEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What `/health` reports for receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptSnapshot {
    pub state: &'static str,
    pub seq: Option<u64>,
}

struct Health {
    state: AtomicU8,
    seq: AtomicU64,
    has_seq: AtomicBool,
    overflow: AtomicU64,
}

impl Health {
    fn new(state: u8) -> Self {
        Self {
            state: AtomicU8::new(state),
            seq: AtomicU64::new(0),
            has_seq: AtomicBool::new(false),
            overflow: AtomicU64::new(0),
        }
    }

    fn set(&self, state: u8) {
        self.state.store(state, Ordering::Relaxed);
    }

    fn note_seq(&self, seq: u64) {
        self.seq.store(seq, Ordering::Relaxed);
        self.has_seq.store(true, Ordering::Relaxed);
    }

    fn snapshot(&self) -> ReceiptSnapshot {
        let mut state = self.state.load(Ordering::Relaxed);
        if state == STATE_OK && self.overflow.load(Ordering::Relaxed) > 0 {
            state = STATE_DEGRADED;
        }
        let seq = if self.has_seq.load(Ordering::Relaxed)
            && state != STATE_OFF
            && state != STATE_UNPROTECTED
            && state != STATE_UNAVAILABLE
        {
            Some(self.seq.load(Ordering::Relaxed))
        } else {
            None
        };
        ReceiptSnapshot {
            state: state_name(state),
            seq,
        }
    }
}

fn state_name(state: u8) -> &'static str {
    match state {
        STATE_OK => "ok",
        STATE_DEGRADED => "degraded",
        STATE_UNPROTECTED => "unprotected",
        STATE_UNAVAILABLE => "unavailable",
        _ => "off",
    }
}

enum Job {
    Event(ReceiptEvent),
    Shutdown {
        reason: String,
        done: oneshot::Sender<()>,
    },
    #[cfg(test)]
    FailNext {
        n: u64,
        done: oneshot::Sender<()>,
    },
    #[cfg(test)]
    Barrier(oneshot::Sender<()>),
}

/// Hands rows to the writer. Cheap to clone. [`Appender::off`] does nothing
/// and does not touch a file.
#[derive(Clone)]
pub struct Appender {
    tx: Option<mpsc::Sender<Job>>,
    health: Arc<Health>,
    /// Last clone restores the dumpable flag. Only set after it was cleared.
    /// Owning the guard is the use; nothing reads the field.
    #[cfg(test)]
    #[expect(
        dead_code,
        reason = "dropped with the last appender to restore the dumpable flag"
    )]
    restore: Option<Arc<RestoreDumpable>>,
}

impl Appender {
    /// Receipts off. No task, no file.
    #[must_use]
    pub fn off() -> Self {
        Self {
            tx: None,
            health: Arc::new(Health::new(STATE_OFF)),
            #[cfg(test)]
            restore: None,
        }
    }

    /// Whether a turn should build a receipt. False does no hashing.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.tx.is_some()
    }

    /// Open the store when the effective mode is on, and spawn the writer.
    ///
    /// `listen` is the snapshot `main` took before the runtime. This function
    /// does not read `LISTEN_*`. A required store that is not protected is
    /// not created and not written.
    pub async fn start(
        config: &Config,
        root: &Path,
        listen: ListenEnv,
        probe: Arc<dyn ProtectionProbe + Send + Sync>,
        provider_name: &str,
    ) -> Self {
        Self::boot(config, root, listen, probe, provider_name, CAPACITY, None).await
    }

    #[cfg(test)]
    pub async fn start_paused(
        config: &Config,
        root: &Path,
        probe: Arc<dyn ProtectionProbe + Send + Sync>,
        provider_name: &str,
        capacity: usize,
        release: oneshot::Receiver<()>,
    ) -> Self {
        Self::boot(
            config,
            root,
            ListenEnv::empty(),
            probe,
            provider_name,
            capacity,
            Some(release),
        )
        .await
    }

    async fn boot(
        config: &Config,
        root: &Path,
        listen: ListenEnv,
        probe: Arc<dyn ProtectionProbe + Send + Sync>,
        provider_name: &str,
        capacity: usize,
        release: Option<oneshot::Receiver<()>>,
    ) -> Self {
        let mode = receipts_mode(&config.receipts);
        if !mode.enabled {
            return Self::off();
        }
        disarm_core_dumps();
        let root_buf = root.to_path_buf();
        let decision = decide_source(config, &root_buf, listen, mode.require_protected);
        let source = match decision {
            SourceDecision::Open(source) => source,
            SourceDecision::Unprotected(reason) => {
                tracing::warn!("{reason}");
                return Self::unprotected();
            }
            SourceDecision::Unavailable(reason) => {
                tracing::warn!("{reason}");
                return Self::unavailable();
            }
        };
        if mode.require_protected && source_missing(&source) {
            tracing::warn!(
                path = %path_of(&source),
                "receipt file is missing; not creating it while protection is required"
            );
            return Self::unprotected();
        }
        let pulse = config.pulse.name.clone();
        let boot_config = config.clone();
        let probe_for_open = Arc::clone(&probe);
        let require = mode.require_protected;
        let opened = tokio::task::spawn_blocking(move || {
            let ident = identity(&root_buf, &boot_config);
            let opened =
                ReceiptStore::open_for_daemon(source, probe_for_open.as_ref(), &pulse, require);
            (opened, ident)
        })
        .await;
        let (opened, ident) = match opened {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!("receipt store did not open: {err}");
                return Self::degraded();
            }
        };
        let boot = boot_event(config, provider_name, ident);
        let (store, opened) = match opened {
            Ok(OpenChoice::Ready(pair)) => *pair,
            Ok(OpenChoice::Unprotected(protection)) => {
                tracing::warn!(
                    reason = %protection.unmet(),
                    "receipt store is not protected; writing nothing"
                );
                return Self::unprotected();
            }
            Err(err) => {
                return open_error(err);
            }
        };
        tracing::info!(
            recovered = opened.recovered,
            repaired_newline = opened.repaired_newline,
            recreated = opened.recreated,
            orphans = opened.orphans.len(),
            rows = opened.report.rows,
            head = %opened.report.head,
            "receipt store open"
        );
        let boot = with_protection(boot, &opened.protection);
        let orphans = opened.orphans;
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let health = Arc::new(Health::new(STATE_OK));
        let health_task = Arc::clone(&health);
        let (boot_tx, boot_rx) = oneshot::channel();
        let paused = release.is_some();
        tokio::spawn(async move {
            writer(store, rx, health_task, orphans, boot, release, boot_tx).await;
        });
        if paused {
            let _ = boot_rx.await;
        }
        Self {
            tx: Some(tx),
            health,
            #[cfg(test)]
            restore: take_restore(),
        }
    }

    fn unprotected() -> Self {
        Self {
            tx: None,
            health: Arc::new(Health::new(STATE_UNPROTECTED)),
            #[cfg(test)]
            restore: take_restore(),
        }
    }

    fn degraded() -> Self {
        Self {
            tx: None,
            health: Arc::new(Health::new(STATE_DEGRADED)),
            #[cfg(test)]
            restore: take_restore(),
        }
    }

    fn unavailable() -> Self {
        Self {
            tx: None,
            health: Arc::new(Health::new(STATE_UNAVAILABLE)),
            #[cfg(test)]
            restore: take_restore(),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> ReceiptSnapshot {
        self.health.snapshot()
    }

    fn submit(&self, event: ReceiptEvent) {
        let Some(tx) = &self.tx else {
            return;
        };
        match tx.try_send(Job::Event(event)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_)) => {
                self.health.overflow.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Write the shutdown row and wait, briefly, for it to land.
    ///
    /// A turn never calls this. The graceful path does, after in-flight
    /// turns have finished.
    pub async fn shutdown(&self) {
        let Some(tx) = &self.tx else {
            return;
        };
        let (done, rx) = oneshot::channel();
        let job = Job::Shutdown {
            reason: "graceful".to_string(),
            done,
        };
        if tokio::time::timeout(Duration::from_secs(2), tx.send(job))
            .await
            .is_err()
        {
            tracing::warn!("receipt shutdown was not queued");
            return;
        }
        if tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .is_err()
        {
            tracing::warn!("receipt shutdown row was not written");
        }
    }

    /// Arm the next `n` appends to fail, and wait until the writer has.
    #[cfg(test)]
    pub async fn arm_fault(&self, n: u64) {
        let Some(tx) = &self.tx else {
            return;
        };
        let (done, rx) = oneshot::channel();
        let _ = tx.try_send(Job::FailNext { n, done });
        let _ = rx.await;
    }

    /// Wait until every row queued so far has been handled.
    #[cfg(test)]
    pub async fn drained(&self) {
        let Some(tx) = &self.tx else {
            return;
        };
        let (done, rx) = oneshot::channel();
        if tx.send(Job::Barrier(done)).await.is_err() {
            return;
        }
        let _ = rx.await;
    }
}

fn source_missing(source: &StoreSource) -> bool {
    match source {
        StoreSource::Path(path) => !path.exists(),
        StoreSource::SystemdFd { .. } => false,
    }
}

fn path_of(source: &StoreSource) -> String {
    match source {
        StoreSource::Path(path) => path.display().to_string(),
        StoreSource::SystemdFd { name, .. } => format!("systemd:{name}"),
    }
}

enum SourceDecision {
    Open(StoreSource),
    /// Keep serving, write nothing. `/health` says unprotected.
    Unprotected(String),
    /// Keep serving, write nothing. `/health` says unavailable.
    Unavailable(String),
}

fn open_error(err: StoreError) -> Appender {
    match err {
        StoreError::NotUsed { .. } => {
            tracing::warn!(
                "systemd receipts fd is already in use by another pulse; this pulse will not write"
            );
            Appender::unprotected()
        }
        StoreError::Foreign { .. } | StoreError::NotLedger { .. } => {
            tracing::warn!("{err}");
            Appender::unavailable()
        }
        other => {
            tracing::warn!("receipt store did not open: {other}");
            Appender::degraded()
        }
    }
}

fn decide_source(
    config: &Config,
    root: &Path,
    listen: ListenEnv,
    require_protected: bool,
) -> SourceDecision {
    let named = super::store::listen_fd(
        listen.pid.as_deref(),
        listen.fds.as_deref(),
        listen.names.as_deref(),
        std::process::id(),
        "receipts",
    );
    if named.is_some() {
        return SourceDecision::Open(StoreSource::SystemdFd {
            name: "receipts",
            listen,
        });
    }
    if require_protected {
        return SourceDecision::Unprotected(
            "receipts require protection; [receipts] path is ignored and nothing is opened"
                .to_string(),
        );
    }
    let requested = if config.receipts.path.is_empty() {
        default_receipts_path(&config.pulse.name)
    } else {
        PathBuf::from(&config.receipts.path)
    };
    match confine_receipt_path(root, &config.pulse.name, &requested) {
        Ok(path) => SourceDecision::Open(StoreSource::Path(path)),
        Err(reason) => SourceDecision::Unavailable(reason),
    }
}

/// The path is usable only under the pulse directory or `/var/lib/pulse-null/<pulse>/`.
///
/// The parent is canonicalised, so a symlink that leaves those directories is
/// refused. `..` is refused. A symlink at the file itself is refused.
fn confine_receipt_path(root: &Path, pulse: &str, requested: &Path) -> Result<PathBuf, String> {
    // A relative path is inside the pulse directory, not the process cwd.
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    if requested
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "[receipts] path {} has a .. component; nothing was created",
            requested.display()
        ));
    }
    let parent = match requested.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let canon_parent = parent.canonicalize().map_err(|err| {
        format!(
            "[receipts] path {} is not under an allowed directory ({err}); nothing was created",
            requested.display()
        )
    })?;
    let Some(name) = requested.file_name() else {
        return Err(format!(
            "[receipts] path {} has no file name; nothing was created",
            requested.display()
        ));
    };
    if name == ".." || name == "." {
        return Err(format!(
            "[receipts] path {} has no file name; nothing was created",
            requested.display()
        ));
    }
    let full = canon_parent.join(name);
    if std::fs::symlink_metadata(&full).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(format!(
            "[receipts] path {} is a symlink; nothing was created",
            full.display()
        ));
    }
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let var_dir = PathBuf::from("/var/lib/pulse-null").join(pulse);
    let var_canon = var_dir.canonicalize().unwrap_or(var_dir);
    let under = |base: &Path| full.starts_with(base) && full != base;
    if under(&root_canon) || under(&var_canon) {
        Ok(full)
    } else {
        Err(format!(
            "[receipts] path {} is outside the pulse directory and /var/lib/pulse-null/{pulse}; nothing was created",
            full.display()
        ))
    }
}

async fn writer(
    mut store: ReceiptStore,
    mut rx: mpsc::Receiver<Job>,
    health: Arc<Health>,
    orphans: Vec<String>,
    boot: ReceiptEvent,
    release: Option<oneshot::Receiver<()>>,
    boot_done: oneshot::Sender<()>,
) {
    let health_boot = Arc::clone(&health);
    let booted = tokio::task::spawn_blocking(move || {
        for stream in orphans {
            apply(&mut store, &lost_close(&stream), &health_boot);
        }
        apply(&mut store, &boot, &health_boot);
        store
    })
    .await;
    let Ok(mut store) = booted else {
        health.set(STATE_DEGRADED);
        let _ = boot_done.send(());
        return;
    };
    let _ = boot_done.send(());
    if let Some(release) = release {
        let _ = release.await;
    }
    while let Some(job) = rx.recv().await {
        match job {
            Job::Shutdown { reason, done } => {
                let health_job = Arc::clone(&health);
                let joined = tokio::task::spawn_blocking(move || {
                    apply(&mut store, &ReceiptEvent::Shutdown { reason }, &health_job);
                    store
                })
                .await;
                if let Ok(next) = joined {
                    drop(next);
                } else {
                    health.set(STATE_DEGRADED);
                }
                let _ = done.send(());
                break;
            }
            Job::Event(event) => {
                let health_job = Arc::clone(&health);
                let joined = tokio::task::spawn_blocking(move || {
                    apply(&mut store, &event, &health_job);
                    store
                })
                .await;
                match joined {
                    Ok(next) => store = next,
                    Err(_) => {
                        health.set(STATE_DEGRADED);
                        break;
                    }
                }
            }
            #[cfg(test)]
            Job::FailNext { n, done } => {
                store.fail_next_appends(n);
                let _ = done.send(());
            }
            #[cfg(test)]
            Job::Barrier(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn apply(store: &mut ReceiptStore, event: &ReceiptEvent, health: &Health) {
    let overflow = health.overflow.swap(0, Ordering::AcqRel);
    if overflow > 0 {
        store.lose(overflow);
    }
    match store.append(event.clone()) {
        Ok(head) => {
            health.note_seq(head.seq);
            if health.overflow.load(Ordering::Relaxed) == 0 {
                health.set(STATE_OK);
            } else {
                health.set(STATE_DEGRADED);
            }
            tracing::debug!(seq = head.seq, hash = %head.hash, "receipt appended");
        }
        Err(err) => {
            tracing::warn!("receipt append failed: {err}");
            health.set(STATE_DEGRADED);
        }
    }
}

fn lost_close(stream: &str) -> ReceiptEvent {
    ReceiptEvent::Close {
        stream: stream.to_string(),
        outcome: Outcome::Lost,
        action_class: ActionClass::None,
        provider: String::new(),
        adapter: String::new(),
        model: String::new(),
        fallback: false,
        input_sha256: None,
        output_sha256: None,
        tokens_in: None,
        tokens_out: None,
        duration_ms: 0,
        mailbox_head: (),
        folded: (),
        fence: (),
    }
}

fn boot_event(config: &Config, provider_name: &str, ident: Identity) -> ReceiptEvent {
    let fallback = config.llm.fallback_target().map(str::to_string);
    ReceiptEvent::Boot {
        pulse: config.pulse.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        provider: provider_name.to_string(),
        adapter: adapter_name(config),
        model: config.llm.model.clone(),
        fallback_model: fallback,
        identity: ident,
        fd_source: FdSource::SelfOpened,
        protected: false,
        append_only: false,
    }
}

fn with_protection(event: ReceiptEvent, protection: &Protection) -> ReceiptEvent {
    match event {
        ReceiptEvent::Boot {
            pulse,
            version,
            provider,
            adapter,
            model,
            fallback_model,
            identity,
            ..
        } => ReceiptEvent::Boot {
            pulse,
            version,
            provider,
            adapter,
            model,
            fallback_model,
            identity,
            fd_source: if protection.systemd_fd {
                FdSource::Systemd
            } else {
                FdSource::SelfOpened
            },
            protected: protection.is_protected(),
            append_only: protection.append_flag,
        },
        other => other,
    }
}

fn adapter_name(config: &Config) -> String {
    config.llm.cli_adapter().unwrap_or("").to_string()
}

fn identity(root: &Path, config: &Config) -> Identity {
    Identity {
        self_md: sha_file(&root.join("SELF.md")),
        rules: config
            .pulse
            .rules_dir
            .as_deref()
            .and_then(|dir| hash_rules(root, dir)),
        config: sha_file(&root.join("pulse-null.toml")),
    }
}

/// Hash a regular file. A fifo or other non-file is skipped so boot cannot block on it.
fn sha_file(path: &Path) -> Option<String> {
    read_regular(path).map(|bytes| digest(&bytes))
}

fn read_regular(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !(meta.is_file() || meta.file_type().is_symlink()) {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut &file, &mut bytes).ok()?;
    Some(bytes)
}

fn hash_rules(root: &Path, dir: &str) -> Option<String> {
    let path = {
        let raw = Path::new(dir);
        if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            root.join(raw)
        }
    };
    if !path.is_dir() {
        return None;
    }
    let mut files = Vec::new();
    let mut unreadable = 0u64;
    if collect_files(&path, &path, &mut files, &mut unreadable).is_err() && files.is_empty() {
        return None;
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut pairs: Vec<serde_json::Value> = files
        .iter()
        .map(|(rel, bytes)| match bytes {
            Some(bytes) => serde_json::json!([rel, digest(bytes)]),
            None => serde_json::json!([rel, "unreadable"]),
        })
        .collect();
    if unreadable > 0 {
        pairs.push(serde_json::json!(["unreadable", unreadable]));
    }
    let text = serde_json::to_string(&serde_json::Value::Array(pairs)).ok()?;
    Some(digest(text.as_bytes()))
}

fn collect_files(
    dir: &Path,
    root: &Path,
    out: &mut Vec<(String, Option<Vec<u8>>)>,
    unreadable: &mut u64,
) -> std::io::Result<()> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        names.push(entry?.path());
    }
    names.sort();
    for path in names {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => {
                *unreadable += 1;
                continue;
            }
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if meta.is_dir() && !meta.file_type().is_symlink() {
            collect_files(&path, root, out, unreadable)?;
            continue;
        }
        if meta.is_file() || meta.file_type().is_symlink() {
            match read_regular(&path) {
                Some(bytes) => out.push((rel, Some(bytes))),
                None => {
                    *unreadable += 1;
                    out.push((rel, None));
                }
            }
        }
    }
    Ok(())
}

/// Clear the dumpable flag so a crash does not write the process image.
///
/// Called only when receipts are on, including when the store is left
/// unwritten because it is not protected. Exec of another program sets the
/// flag back to 1; this process stays cleared until the test guard drops.
fn disarm_core_dumps() {
    // Same lock as a test that forks or reads the flag, so the two do not
    // overlap. The guard restores the flag when the appender drops.
    #[cfg(test)]
    let _hold = crate::process_state::Hold::acquire();
    // SAFETY: `PR_SET_DUMPABLE` takes an integer argument and no pointer.
    // Zero disables core dumps for this process.
    let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if rc != 0 {
        tracing::warn!(
            "could not clear the dumpable flag: {}",
            std::io::Error::last_os_error()
        );
    }
    #[cfg(test)]
    arm_restore();
}

#[cfg(test)]
struct RestoreDumpable;

#[cfg(test)]
impl Drop for RestoreDumpable {
    fn drop(&mut self) {
        let _hold = crate::process_state::Hold::acquire();
        // SAFETY: integer flag only. 1 is the ordinary value for this process.
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) };
    }
}

#[cfg(test)]
fn arm_restore() {
    RESTORE.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Arc::new(RestoreDumpable));
        }
    });
}

#[cfg(test)]
fn take_restore() -> Option<Arc<RestoreDumpable>> {
    RESTORE.with(|slot| slot.borrow_mut().take())
}

#[cfg(test)]
std::thread_local! {
    static RESTORE: std::cell::RefCell<Option<Arc<RestoreDumpable>>> = const { std::cell::RefCell::new(None) };
}

/// What a finished turn tells the close row. The request text is not here.
pub struct TurnRecord<'a> {
    pub text: Option<&'a str>,
    pub model: Option<&'a str>,
    pub isolation: bool,
    pub truncated: bool,
    pub refused: bool,
    /// The turn ran on the configured fallback target.
    pub fallback_used: bool,
    pub tokens_in: Option<u32>,
    pub tokens_out: Option<u32>,
}

/// Fields known when the turn starts. The sender string is not one of them.
pub struct OpenInfo {
    pub stream: String,
    pub channel: String,
    pub principal: String,
    pub principal_sha256: Option<String>,
    pub provider: String,
    pub adapter: String,
    pub model: String,
    pub input_sha256: String,
}

/// Drops to `close{aborted}` unless [`StreamReceipt::finish`] ran.
pub struct StreamReceipt {
    tx: Option<mpsc::Sender<Job>>,
    health: Arc<Health>,
    finished: bool,
    info: Option<OpenInfo>,
    started: Instant,
}

impl StreamReceipt {
    /// No row, no digest, no id. Drop and finish do nothing.
    #[must_use]
    pub fn inert() -> Self {
        Self {
            tx: None,
            health: Arc::new(Health::new(STATE_OFF)),
            finished: true,
            info: None,
            started: Instant::now(),
        }
    }

    /// Build `info` only when `appender` is on.
    #[must_use]
    pub fn open_if_on(appender: &Appender, build: impl FnOnce() -> OpenInfo) -> Self {
        if !appender.is_on() {
            return Self::inert();
        }
        Self::open(appender, build())
    }

    /// Queue the `open` row. A full channel counts toward a later `gap`.
    #[must_use]
    pub fn open(appender: &Appender, info: OpenInfo) -> Self {
        if !appender.is_on() {
            return Self::inert();
        }
        appender.submit(ReceiptEvent::Open {
            stream: info.stream.clone(),
            channel: info.channel.clone(),
            principal: info.principal.clone(),
            principal_sha256: info.principal_sha256.clone(),
            task_id: None,
            parent: None,
        });
        Self {
            tx: appender.tx.clone(),
            health: Arc::clone(&appender.health),
            finished: false,
            info: Some(info),
            started: Instant::now(),
        }
    }

    /// Queue the `close` row for a turn that produced a result.
    pub fn finish(mut self, end: TurnRecord<'_>) {
        self.finished = true;
        if self.tx.is_none() {
            return;
        }
        let Some(info) = &self.info else {
            return;
        };
        let event = close_event(info, &end, self.started.elapsed());
        self.send(event);
    }

    fn send(&self, event: ReceiptEvent) {
        let Some(tx) = &self.tx else {
            return;
        };
        match tx.try_send(Job::Event(event)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_)) => {
                self.health.overflow.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for StreamReceipt {
    fn drop(&mut self) {
        if self.finished || self.tx.is_none() {
            return;
        }
        let Some(info) = &self.info else {
            return;
        };
        let event = ReceiptEvent::Close {
            stream: info.stream.clone(),
            outcome: Outcome::Aborted,
            action_class: ActionClass::None,
            provider: info.provider.clone(),
            adapter: info.adapter.clone(),
            model: info.model.clone(),
            fallback: false,
            input_sha256: Some(info.input_sha256.clone()),
            output_sha256: None,
            tokens_in: None,
            tokens_out: None,
            duration_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            mailbox_head: (),
            folded: (),
            fence: (),
        };
        self.send(event);
    }
}

fn close_event(info: &OpenInfo, end: &TurnRecord<'_>, elapsed: Duration) -> ReceiptEvent {
    let model = end.model.unwrap_or(&info.model).to_string();
    let (outcome, action, output) = if let Some(text) = end.text {
        let hash = Some(digest(text.as_bytes()));
        if end.isolation {
            (Outcome::Isolated, ActionClass::Reply, hash)
        } else if end.truncated || end.refused {
            (Outcome::Refused, ActionClass::None, hash)
        } else {
            (Outcome::Acted, ActionClass::Reply, hash)
        }
    } else if end.refused {
        (Outcome::Refused, ActionClass::None, None)
    } else {
        (Outcome::Error, ActionClass::None, None)
    };
    ReceiptEvent::Close {
        stream: info.stream.clone(),
        outcome,
        action_class: action,
        provider: info.provider.clone(),
        adapter: info.adapter.clone(),
        model,
        fallback: end.fallback_used,
        input_sha256: Some(info.input_sha256.clone()),
        output_sha256: output,
        tokens_in: end.tokens_in.map(u64::from),
        tokens_out: end.tokens_out.map(u64::from),
        duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        mailbox_head: (),
        folded: (),
        fence: (),
    }
}

/// `owner`, `peer:<name>`, or `guest` plus the sender's digest.
///
/// The sender string is the digest input and is not returned.
#[must_use]
pub fn principal_of(key: &crate::session_key::SessionKey) -> (String, Option<String>) {
    use crate::session_key::Principal;
    match key.principal() {
        Principal::Owner => ("owner".to_string(), None),
        Principal::Peer(name) => (format!("peer:{name}"), None),
        Principal::Guest(sender) => ("guest".to_string(), Some(digest(sender.as_bytes()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use crate::config::{with_receipts_env, ReceiptsConfig, RECEIPTS_ENV};
    use crate::oneself::protect::{FakeProbe, LiveProbe};

    fn config_at(path: &Path, enabled: bool, require: bool) -> Config {
        let mut config = crate::config::test_support::minimal_config();
        config.receipts = ReceiptsConfig {
            enabled,
            path: path.display().to_string(),
            require_protected: require,
            content: "digest".to_string(),
        };
        config.pulse.name = "Echo".into();
        config.llm.model = "mock-model".into();
        config.llm.provider = "mock".into();
        config
    }

    fn unprotected() -> Arc<dyn ProtectionProbe + Send + Sync> {
        Arc::new(FakeProbe {
            systemd_fd: false,
            owner_root: false,
            mode_safe: false,
            append_flag: false,
            parent_safe: false,
        })
    }

    #[tokio::test]
    async fn off_does_not_create_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let config = config_at(&path, false, true);
        let appender = Appender::start(
            &config,
            dir.path(),
            ListenEnv::empty(),
            Arc::new(LiveProbe),
            "mock",
        )
        .await;
        assert_eq!(appender.snapshot().state, "off");
        assert!(appender.snapshot().seq.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn the_file_cannot_turn_required_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let config = config_at(&path, false, false);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let snapshot = with_receipts_env(|| {
            std::env::set_var(RECEIPTS_ENV, "required");
            struct Clear;
            impl Drop for Clear {
                fn drop(&mut self) {
                    std::env::remove_var(RECEIPTS_ENV);
                }
            }
            let _clear = Clear;
            rt.block_on(async {
                let appender = Appender::start(
                    &config,
                    dir.path(),
                    ListenEnv::empty(),
                    unprotected(),
                    "mock",
                )
                .await;
                appender.snapshot()
            })
        });
        assert_eq!(snapshot.state, "unprotected");
        assert!(snapshot.seq.is_none());
        assert!(!path.exists(), "required mode must not create the file");
    }

    #[tokio::test]
    async fn a_protected_fake_probe_writes_and_an_unprotected_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        std::fs::write(&path, b"").unwrap();
        let config = config_at(&path, true, true);
        let quiet = Appender::start(
            &config,
            dir.path(),
            ListenEnv::empty(),
            unprotected(),
            "mock",
        )
        .await;
        assert_eq!(quiet.snapshot().state, "unprotected");
        assert_eq!(std::fs::read(&path).unwrap(), b"");

        // Protection required ignores the path, including a probe that would pass.
        let skipped = Appender::start(
            &config,
            dir.path(),
            ListenEnv::empty(),
            Arc::new(FakeProbe::protected()),
            "mock",
        )
        .await;
        assert_eq!(skipped.snapshot().state, "unprotected");
        assert_eq!(std::fs::read(&path).unwrap(), b"");

        let writing_config = config_at(&path, true, false);
        let writing = Appender::start(
            &writing_config,
            dir.path(),
            ListenEnv::empty(),
            Arc::new(FakeProbe::protected()),
            "mock",
        )
        .await;
        writing.drained().await;
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"genesis\""), "{text}");
        assert!(text.contains("\"boot\""), "{text}");
        assert_eq!(writing.snapshot().state, "ok");
        assert!(writing.snapshot().seq.is_some());
        writing.shutdown().await;
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"shutdown\""), "{text}");
    }

    #[tokio::test]
    async fn a_full_channel_returns_and_counts_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let config = config_at(&path, true, false);
        let (release_tx, release_rx) = oneshot::channel();
        let appender = Appender::start_paused(
            &config,
            dir.path(),
            Arc::new(LiveProbe),
            "mock",
            1,
            release_rx,
        )
        .await;
        let info = OpenInfo {
            stream: "s-full".into(),
            channel: "tui".into(),
            principal: "owner".into(),
            principal_sha256: None,
            provider: "mock".into(),
            adapter: String::new(),
            model: "mock-model".into(),
            input_sha256: chain::sha256_hex(b"one"),
        };
        let first = StreamReceipt::open(&appender, info);
        let started = Instant::now();
        let second = StreamReceipt::open(
            &appender,
            OpenInfo {
                stream: "s-overflow".into(),
                channel: "tui".into(),
                principal: "owner".into(),
                principal_sha256: None,
                provider: "mock".into(),
                adapter: String::new(),
                model: "mock-model".into(),
                input_sha256: chain::sha256_hex(b"two"),
            },
        );
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(first);
        drop(second);
        let _ = release_tx.send(());
        appender.drained().await;
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"gap\""), "{text}");
        assert!(!text.contains("one"));
        assert!(!text.contains("two"));
    }

    #[test]
    fn guest_principal_hashes_the_sender_and_drops_the_string() {
        let key = crate::session_key::SessionKey::guest("zephyr-sender");
        let (principal, hash) = principal_of(&key);
        assert_eq!(principal, "guest");
        assert_eq!(
            hash.as_deref(),
            Some(chain::sha256_hex(b"zephyr-sender").as_str())
        );
        let rendered = format!("{principal} {hash:?}");
        assert!(!rendered.contains("zephyr-sender"));
    }

    #[test]
    fn receipts_off_does_not_hash_or_build_a_row() {
        FORBID_DIGEST.with(|flag| flag.set(true));
        struct Allow;
        impl Drop for Allow {
            fn drop(&mut self) {
                FORBID_DIGEST.with(|flag| flag.set(false));
            }
        }
        let _allow = Allow;
        let appender = Appender::off();
        assert!(!appender.is_on());
        let receipt = StreamReceipt::open_if_on(&appender, || {
            let _ = digest(b"request");
            OpenInfo {
                stream: "unused".into(),
                channel: "tui".into(),
                principal: "owner".into(),
                principal_sha256: None,
                provider: "mock".into(),
                adapter: String::new(),
                model: "mock-model".into(),
                input_sha256: digest(b"request"),
            }
        });
        let inert = StreamReceipt::open(
            &appender,
            OpenInfo {
                stream: "unused".into(),
                channel: "tui".into(),
                principal: "owner".into(),
                principal_sha256: None,
                provider: "mock".into(),
                adapter: String::new(),
                model: "mock-model".into(),
                input_sha256: "nope".into(),
            },
        );
        let end = TurnRecord {
            text: Some("reply"),
            model: Some("mock-model"),
            isolation: false,
            truncated: false,
            refused: false,
            fallback_used: false,
            tokens_in: None,
            tokens_out: None,
        };
        receipt.finish(end);
        drop(inert);
    }

    fn close_fallback(path: &Path, used: bool, model: &str) -> bool {
        let info = OpenInfo {
            stream: "s-fallback".into(),
            channel: "tui".into(),
            principal: "owner".into(),
            principal_sha256: None,
            provider: "mock".into(),
            adapter: String::new(),
            model: model.to_string(),
            input_sha256: chain::sha256_hex(b"in"),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let config = config_at(path, true, false);
            let appender = Appender::start(
                &config,
                path.parent().unwrap(),
                ListenEnv::empty(),
                Arc::new(LiveProbe),
                "mock",
            )
            .await;
            StreamReceipt::open(&appender, info).finish(TurnRecord {
                text: Some("reply"),
                model: Some(model),
                isolation: false,
                truncated: false,
                refused: false,
                fallback_used: used,
                tokens_in: None,
                tokens_out: None,
            });
            appender.drained().await;
        });
        let text = std::fs::read_to_string(path).unwrap();
        let close = text
            .lines()
            .find(|line| line.contains("\"kind\":\"close\""))
            .unwrap();
        close.contains("\"fallback\":true")
    }

    #[test]
    fn the_fallback_flag_follows_the_turn_not_the_model_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        assert!(!close_fallback(&path, false, "fallback-model"));
        std::fs::remove_file(&path).unwrap();
        assert!(close_fallback(&path, true, "fallback-model"));
    }

    #[tokio::test]
    async fn a_disabled_fallback_is_not_named_on_the_boot_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let mut config = config_at(&path, true, false);
        config.llm.fallback_model = Some("fallback-model".into());
        config.llm.fallback_on_refusal = false;
        let appender = Appender::start(
            &config,
            dir.path(),
            ListenEnv::empty(),
            Arc::new(LiveProbe),
            "mock",
        )
        .await;
        appender.drained().await;
        let text = std::fs::read_to_string(&path).unwrap();
        let boot = text
            .lines()
            .find(|line| line.contains("\"kind\":\"boot\""))
            .unwrap();
        assert!(!boot.contains("fallback_model"), "{boot}");
        assert!(!boot.contains("fallback-model"), "{boot}");
    }

    #[tokio::test]
    async fn another_pulses_file_is_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let bravo = config_at(&path, true, false);
        let mut bravo = bravo;
        bravo.pulse.name = "bravo".into();
        let writer = Appender::start(
            &bravo,
            dir.path(),
            ListenEnv::empty(),
            Arc::new(LiveProbe),
            "mock",
        )
        .await;
        writer.drained().await;
        drop(writer);
        let before = std::fs::read(&path).unwrap();
        assert!(std::str::from_utf8(&before).unwrap().contains("bravo"));
        let mut alpha = config_at(&path, true, false);
        alpha.pulse.name = "alpha".into();
        let refused = Appender::start(
            &alpha,
            dir.path(),
            ListenEnv::empty(),
            Arc::new(LiveProbe),
            "mock",
        )
        .await;
        assert_eq!(refused.snapshot().state, "unavailable");
        assert!(refused.snapshot().seq.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn a_non_empty_file_without_a_genesis_is_left_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let torn = b"not-a-genesis-and-no-newline";
        std::fs::write(&path, torn).unwrap();
        let config = config_at(&path, true, false);
        let appender = Appender::start(
            &config,
            dir.path(),
            ListenEnv::empty(),
            Arc::new(LiveProbe),
            "mock",
        )
        .await;
        assert_eq!(appender.snapshot().state, "unavailable");
        assert_eq!(std::fs::read(&path).unwrap(), torn);
    }

    #[tokio::test]
    async fn a_path_outside_the_pulse_is_refused_and_nothing_is_created() {
        let pulse = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("receipts.jsonl");
        let config = config_at(&target, true, false);
        let appender = Appender::start(
            &config,
            pulse.path(),
            ListenEnv::empty(),
            Arc::new(LiveProbe),
            "mock",
        )
        .await;
        assert_eq!(appender.snapshot().state, "unavailable");
        assert!(!target.exists());

        let link = pulse.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let via_link = link.join("via-link.jsonl");
        let config = config_at(&via_link, true, false);
        let appender = Appender::start(
            &config,
            pulse.path(),
            ListenEnv::empty(),
            Arc::new(LiveProbe),
            "mock",
        )
        .await;
        assert_eq!(appender.snapshot().state, "unavailable");
        assert!(!outside.path().join("via-link.jsonl").exists());

        let dotted = pulse.path().join("..").join("outside.jsonl");
        let config = config_at(&dotted, true, false);
        let appender = Appender::start(
            &config,
            pulse.path(),
            ListenEnv::empty(),
            Arc::new(LiveProbe),
            "mock",
        )
        .await;
        assert_eq!(appender.snapshot().state, "unavailable");
        let escaped = pulse.path().parent().unwrap().join("outside.jsonl");
        assert!(!escaped.exists());
    }

    #[test]
    fn required_mode_ignores_the_configured_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let bytes = b"leave-these-bytes";
        std::fs::write(&path, bytes).unwrap();
        let config = config_at(&path, false, false);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let snapshot = with_receipts_env(|| {
            std::env::set_var(RECEIPTS_ENV, "required");
            struct Clear;
            impl Drop for Clear {
                fn drop(&mut self) {
                    std::env::remove_var(RECEIPTS_ENV);
                }
            }
            let _clear = Clear;
            rt.block_on(async {
                let appender = Appender::start(
                    &config,
                    dir.path(),
                    ListenEnv::empty(),
                    Arc::new(LiveProbe),
                    "mock",
                )
                .await;
                appender.snapshot()
            })
        });
        assert_eq!(snapshot.state, "unprotected");
        assert!(snapshot.seq.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[tokio::test]
    async fn a_second_pulse_does_not_adopt_the_systemd_fd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let config = config_at(&path, true, false);
        let listen = ListenEnv::captured(
            Some(std::process::id().to_string()),
            Some("1".into()),
            Some("receipts".into()),
        );
        assert!(listen.claim_for_test());
        let appender =
            Appender::start(&config, dir.path(), listen, Arc::new(LiveProbe), "mock").await;
        assert_eq!(appender.snapshot().state, "unprotected");
        assert!(appender.snapshot().seq.is_none());
        assert!(!path.exists());
    }

    fn boot_identity(dir: &Path) -> serde_json::Value {
        let path = dir.join("receipts.jsonl");
        let config = config_at(&path, true, false);
        let mut config = config;
        config.pulse.rules_dir = Some(dir.join("rules").display().to_string());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let appender = Appender::start(
                &config,
                dir,
                ListenEnv::empty(),
                Arc::new(LiveProbe),
                "mock",
            )
            .await;
            appender.drained().await;
        });
        let text = std::fs::read_to_string(path).unwrap();
        let boot = text
            .lines()
            .find(|line| line.contains("\"kind\":\"boot\""))
            .unwrap();
        let row: serde_json::Value = serde_json::from_str(boot).unwrap();
        row["e"]["identity"].clone()
    }

    #[test]
    fn identity_follows_a_symlinked_rule_and_keeps_an_unreadable_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SELF.md"), b"self-bytes").unwrap();
        let rules = dir.path().join("rules");
        std::fs::create_dir(&rules).unwrap();
        let real = dir.path().join("real.md");
        std::fs::write(&real, b"ay").unwrap();
        std::os::unix::fs::symlink(&real, rules.join("a.md")).unwrap();
        std::fs::write(rules.join("b.md"), b"secret").unwrap();
        let mode = std::fs::Permissions::from_mode(0o000);
        std::fs::set_permissions(rules.join("b.md"), mode).unwrap();
        let ident = boot_identity(dir.path());
        assert_eq!(ident["self_md"], chain::sha256_hex(b"self-bytes"));
        let pairs = serde_json::json!([
            ["a.md", chain::sha256_hex(b"ay")],
            ["b.md", "unreadable"],
            ["unreadable", 1],
        ]);
        let expect = chain::sha256_hex(serde_json::to_string(&pairs).unwrap().as_bytes());
        assert_eq!(ident["rules"], expect);
        std::fs::set_permissions(rules.join("b.md"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
    }

    #[test]
    fn a_fifo_self_file_does_not_hang_boot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SELF.md");
        let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
        let started = Instant::now();
        let ident = boot_identity(dir.path());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(ident.get("self_md").is_none(), "{ident}");
    }
}
