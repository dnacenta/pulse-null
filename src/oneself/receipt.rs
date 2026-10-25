//! The daemon's receipt writer.
//!
//! One task appends for a pulse. Turns hand it rows through a bounded
//! channel and never wait on the file. A full channel or a failed append
//! is counted and later written as one `gap` row. Receipts that are off
//! do not open a file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use super::chain;
use super::event::{ActionClass, FdSource, Identity, Outcome, ReceiptEvent};
use super::protect::{Protection, ProtectionProbe};
use super::store::{default_receipts_path, ListenEnv, OpenChoice, ReceiptStore, StoreSource};
use crate::config::{receipts_mode, Config};

const CAPACITY: usize = 1024;
const STATE_OFF: u8 = 0;
const STATE_OK: u8 = 1;
const STATE_DEGRADED: u8 = 2;
const STATE_UNPROTECTED: u8 = 3;

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
}

impl Appender {
    /// Receipts off. No task, no file.
    #[must_use]
    pub fn off() -> Self {
        Self {
            tx: None,
            health: Arc::new(Health::new(STATE_OFF)),
        }
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
        let source = store_source(config, listen);
        if mode.require_protected && source_missing(&source) {
            tracing::warn!(
                path = %path_of(&source),
                "receipt file is missing; not creating it while protection is required"
            );
            return Self::unprotected();
        }
        let pulse = config.pulse.name.clone();
        let boot = boot_event(config, root, provider_name);
        let probe_for_open = Arc::clone(&probe);
        let opened = tokio::task::spawn_blocking(move || {
            ReceiptStore::open_for_daemon(
                source,
                probe_for_open.as_ref(),
                &pulse,
                mode.require_protected,
            )
        })
        .await;
        let (store, opened) = match opened {
            Ok(Ok(OpenChoice::Ready(pair))) => *pair,
            Ok(Ok(OpenChoice::Unprotected(protection))) => {
                tracing::warn!(
                    reason = %protection.unmet(),
                    "receipt store is not protected; writing nothing"
                );
                return Self::unprotected();
            }
            Ok(Err(err)) => {
                tracing::warn!("receipt store did not open: {err}");
                return Self::degraded();
            }
            Err(err) => {
                tracing::warn!("receipt store did not open: {err}");
                return Self::degraded();
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
        }
    }

    fn unprotected() -> Self {
        Self {
            tx: None,
            health: Arc::new(Health::new(STATE_UNPROTECTED)),
        }
    }

    fn degraded() -> Self {
        Self {
            tx: None,
            health: Arc::new(Health::new(STATE_DEGRADED)),
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

fn store_source(config: &Config, listen: ListenEnv) -> StoreSource {
    let named = super::store::listen_fd(
        listen.pid.as_deref(),
        listen.fds.as_deref(),
        listen.names.as_deref(),
        std::process::id(),
        "receipts",
    );
    if named.is_some() {
        StoreSource::SystemdFd {
            name: "receipts",
            listen,
        }
    } else if !config.receipts.path.is_empty() {
        StoreSource::Path(PathBuf::from(&config.receipts.path))
    } else {
        StoreSource::Path(default_receipts_path(&config.pulse.name))
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

fn boot_event(config: &Config, root: &Path, provider_name: &str) -> ReceiptEvent {
    let fallback = config
        .llm
        .fallback_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);
    ReceiptEvent::Boot {
        pulse: config.pulse.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        provider: provider_name.to_string(),
        adapter: adapter_name(config),
        model: config.llm.model.clone(),
        fallback_model: fallback,
        identity: identity(root, config),
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

fn sha_file(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| chain::sha256_hex(&bytes))
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
    collect_files(&path, &path, &mut files).ok()?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let pairs: Vec<serde_json::Value> = files
        .iter()
        .map(|(rel, bytes)| serde_json::json!([rel, chain::sha256_hex(bytes)]))
        .collect();
    let text = serde_json::to_string(&serde_json::Value::Array(pairs)).ok()?;
    Some(chain::sha256_hex(text.as_bytes()))
}

fn collect_files(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        names.push(entry?.path());
    }
    names.sort();
    for path in names {
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            collect_files(&path, root, out)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, std::fs::read(&path)?));
        }
    }
    Ok(())
}

/// Clear the dumpable flag so a crash does not write the process image.
///
/// Called only when receipts are on, including when the store is left
/// unwritten because it is not protected.
fn disarm_core_dumps() {
    // SAFETY: `PR_SET_DUMPABLE` takes an integer argument and no pointer.
    // Zero disables core dumps for this process.
    let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if rc != 0 {
        tracing::warn!(
            "could not clear the dumpable flag: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// What a finished turn tells the close row. The request text is not here.
pub struct TurnRecord<'a> {
    pub text: Option<&'a str>,
    pub model: Option<&'a str>,
    pub isolation: bool,
    pub truncated: bool,
    pub refused: bool,
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
    pub fallback_model: Option<String>,
    pub input_sha256: String,
}

/// Drops to `close{aborted}` unless [`StreamReceipt::finish`] ran.
pub struct StreamReceipt {
    tx: Option<mpsc::Sender<Job>>,
    health: Arc<Health>,
    finished: bool,
    info: OpenInfo,
    started: Instant,
}

impl StreamReceipt {
    /// Queue the `open` row. A full channel counts toward a later `gap`.
    #[must_use]
    pub fn open(appender: &Appender, info: OpenInfo) -> Self {
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
            info,
            started: Instant::now(),
        }
    }

    /// Queue the `close` row for a turn that produced a result.
    pub fn finish(mut self, end: TurnRecord<'_>) {
        self.finished = true;
        self.send(close_event(&self.info, &end, self.started.elapsed()));
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
        if self.finished {
            return;
        }
        let event = ReceiptEvent::Close {
            stream: self.info.stream.clone(),
            outcome: Outcome::Aborted,
            action_class: ActionClass::None,
            provider: self.info.provider.clone(),
            adapter: self.info.adapter.clone(),
            model: self.info.model.clone(),
            fallback: false,
            input_sha256: Some(self.info.input_sha256.clone()),
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
        let hash = Some(chain::sha256_hex(text.as_bytes()));
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
    let fallback = info
        .fallback_model
        .as_deref()
        .is_some_and(|item| !item.is_empty() && item == model);
    ReceiptEvent::Close {
        stream: info.stream.clone(),
        outcome,
        action_class: action,
        provider: info.provider.clone(),
        adapter: info.adapter.clone(),
        model,
        fallback,
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
        Principal::Guest(sender) => (
            "guest".to_string(),
            Some(chain::sha256_hex(sender.as_bytes())),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

        let writing = Appender::start(
            &config,
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
            fallback_model: None,
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
                fallback_model: None,
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
}
