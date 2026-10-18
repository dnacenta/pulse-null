//! In-process conversation turns for plugins (PN-110).
//!
//! Plugins used to loop back over HTTP `/chat` without a credential, which
//! kept `[security] secret` unset and admitted every loopback request as the
//! owner. They now run turns here, typed and in-process, as their own narrow
//! principal ([`AuthIdentity::Plugin`]): no plugin holds a credential, and
//! the HTTP auth layer stays strict.
//!
//! Plugins start before [`AppState`] exists (AWARENESS.md depends on their
//! start results), so the runner is created first, handed to every plugin,
//! and bound to the state once it is built. A turn that arrives before the
//! bind waits for it; one that arrives after shutdown starts is refused, and
//! shutdown drains the turns already in flight.
//!
//! The plugin-facing half (`for_plugin`, [`PluginTurns`] and what it uses)
//! has callers only when a plugin feature is compiled in; those items alone
//! carry `allow(dead_code)` in a build without one. The gate lints each
//! plugin feature on its own and together.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use axum::http::StatusCode;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use super::auth::AuthIdentity;
use super::guest_budget::{GuestKind, GuestLedger, GuestRefusal};
use super::handlers::chat::{run_turn, ChatRequest, ChatResponse};
use super::rate_limit::RateLimiter;
use super::AppState;
use crate::tool_loop::TurnSink;

/// How long a turn waits for the runner to be bound before giving up.
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Concurrent turns per plugin. The pool belongs to the plugin's name, so
/// every handle the plugin takes shares it; a Discord burst never eats a
/// voice call's budget, and neither touches the HTTP stream pool the TUI
/// uses. What a turn does when the pool is full is the handle's
/// [`SlotWait`].
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
pub const PLUGIN_POOL_SIZE: usize = 4;

/// Turns a plugin may start in a burst before its rate limit applies — the
/// same bucket `/chat` has over HTTP. In-process turns skip the HTTP layer,
/// so without it a plugin fed from outside (a phone line, a Discord guild)
/// could start paid turns without bound.
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
pub const PLUGIN_RATE_BURST: u32 = 10;

/// A plugin's sustained turn rate once its burst is spent.
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
pub const PLUGIN_RATE_PER_SECOND: f64 = 2.0;

/// The longest a [`SlotWait::Forever`] turn sleeps before it looks at the
/// rate bucket again.
#[cfg_attr(not(feature = "discord-text"), allow(dead_code))]
const MAX_RATE_WAIT: Duration = Duration::from_secs(5);

/// How long shutdown waits for the plugin turns already in flight. A plugin
/// that stops before the drain (the Discord forwarder) waits this long for
/// its own in-flight turns before it gives up on them.
pub const DRAIN_WINDOW: Duration = Duration::from_secs(30);

/// What a plugin turn does when its plugin's pool is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
pub enum SlotWait {
    /// Fail at once with [`TurnError::Busy`].
    Never,
    /// Wait up to this long for a slot, then [`TurnError::Busy`]. Voice.
    #[cfg_attr(not(feature = "voice"), allow(dead_code))]
    Up(Duration),
    /// Wait as long as it takes. For a caller that bounds its own
    /// concurrency and must not drop what it queued: the Discord forwarder.
    #[cfg_attr(not(feature = "discord-text"), allow(dead_code))]
    Forever,
}

/// The channel every voice transport speaks on. Callers on it are guests.
pub const VOICE_CHANNEL: &str = "voice";

/// Channels no plugin may be bound to: the owner's own channels, the peer
/// channel whose senders resolve to sibling pulses, and the HTTP clients'
/// channels (`chat`, the `/chat` default, and the legacy `tui-chat`), whose
/// recent activity — the owner's own lines — must not reach a plugin's
/// guests as channel context (audit #9).
pub const RESERVED_CHANNELS: &[&str] =
    &["tui", "system", "reflection", "comms", "chat", "tui-chat"];

#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
const MAX_CHANNEL_LEN: usize = 64;

/// Whether `channel` is reserved (case-insensitive, so `"TUI"` is too).
#[must_use]
pub fn is_reserved_channel(channel: &str) -> bool {
    RESERVED_CHANNELS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(channel))
}

/// Why a plugin could not be bound to a channel.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
pub enum BindError {
    /// Empty, too long, or with path characters in it.
    #[error("plugin {plugin}: invalid channel name {channel:?}")]
    InvalidChannel {
        plugin: &'static str,
        channel: String,
    },
    /// One of [`RESERVED_CHANNELS`].
    #[error(
        "plugin {plugin}: channel {channel:?} is reserved ({}); pick another",
        RESERVED_CHANNELS.join(", ")
    )]
    ReservedChannel {
        plugin: &'static str,
        channel: String,
    },
}

/// One plugin's turn budget: its pool and its rate bucket. Shared by every
/// handle the plugin takes (cloning shares).
#[derive(Clone)]
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
struct Budget {
    pool: Arc<Semaphore>,
    rate: Arc<RateLimiter>,
    /// What the plugin's guests have cost so far (`[plugin_turns]`).
    guests: Arc<Mutex<GuestLedger>>,
}

#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
impl Budget {
    fn new() -> Self {
        Self {
            pool: Arc::new(Semaphore::new(PLUGIN_POOL_SIZE)),
            rate: Arc::new(RateLimiter::new(PLUGIN_RATE_BURST, PLUGIN_RATE_PER_SECOND)),
            guests: Arc::new(Mutex::new(GuestLedger::default())),
        }
    }
}

/// Why a plugin turn did not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
pub enum TurnError {
    /// The runner was not bound within [`READY_TIMEOUT`].
    #[error("the pulse is not ready to take turns yet")]
    NotReady,
    /// Shutdown has started; no new turns run.
    #[error("the pulse is shutting down")]
    ShuttingDown,
    /// The plugin already has [`PLUGIN_POOL_SIZE`] turns in flight, and no
    /// slot freed up within its [`SlotWait`].
    #[error("too many turns in flight for this plugin")]
    Busy,
    /// The plugin is starting turns faster than its rate limit
    /// ([`PLUGIN_RATE_BURST`], then [`PLUGIN_RATE_PER_SECOND`]).
    #[error("this plugin is over its turn rate limit")]
    RateLimited,
    /// The plugin ran its `[plugin_turns] daily_turns` guest turns today.
    #[error("this plugin has run its guest turns for today")]
    DailyBudgetSpent,
    /// This voice call took its `[plugin_turns] voice_turns_per_call`.
    #[error("this call has used all its turns")]
    CallTurnsSpent,
    /// This guest sender is over its `[plugin_turns] guest_burst` bucket.
    #[error("this sender is posting too fast")]
    SenderRateLimited,
    /// The turn itself refused or failed, with the status `/chat` would
    /// have answered.
    #[error("turn refused ({0}): {1}")]
    Refused(StatusCode, String),
}

struct Shared {
    state: OnceLock<Weak<AppState>>,
    closed: AtomicBool,
    /// Woken on bind and on close.
    changed: Notify,
    in_flight: AtomicUsize,
    /// Woken when the last in-flight turn finishes.
    drained: Notify,
    /// Each plugin's pool and rate bucket, by plugin name.
    #[cfg_attr(
        not(any(feature = "voice", feature = "discord-text")),
        allow(dead_code)
    )]
    budgets: Mutex<HashMap<&'static str, Budget>>,
    #[cfg_attr(
        not(any(feature = "voice", feature = "discord-text")),
        allow(dead_code)
    )]
    ready_timeout: Duration,
}

/// Late-bound access to the turn pipeline, shared by the daemon and every
/// plugin. Cloning is cheap; all clones share one binding.
#[derive(Clone)]
pub struct TurnRunner {
    shared: Arc<Shared>,
}

impl Default for TurnRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnRunner {
    /// An unbound runner. Turns wait up to [`READY_TIMEOUT`] for [`bind`].
    ///
    /// [`bind`]: TurnRunner::bind
    #[must_use]
    pub fn new() -> Self {
        Self::with_ready_timeout(READY_TIMEOUT)
    }

    fn with_ready_timeout(ready_timeout: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: OnceLock::new(),
                closed: AtomicBool::new(false),
                changed: Notify::new(),
                in_flight: AtomicUsize::new(0),
                drained: Notify::new(),
                budgets: Mutex::new(HashMap::new()),
                ready_timeout,
            }),
        }
    }

    /// Bind the runner to the daemon's state. Holds it weakly, so the state
    /// owning the plugins that own this runner is not a cycle. A second bind
    /// is ignored.
    pub(crate) fn bind(&self, state: &Arc<AppState>) {
        if self.shared.state.set(Arc::downgrade(state)).is_err() {
            tracing::warn!("turn runner bound twice; keeping the first binding");
        }
        self.shared.changed.notify_waiters();
    }

    /// Refuse every turn from now on, and wake the ones waiting for the
    /// bind so they fail instead of waiting. The turns already in flight
    /// keep running; [`drain`] waits for them.
    ///
    /// [`drain`]: TurnRunner::drain
    pub(crate) fn refuse_new(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.changed.notify_waiters();
    }

    /// Wait up to `within` for the turns in flight to finish. Returns
    /// whether they all did.
    pub(crate) async fn drain(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            let drained = self.shared.drained.notified();
            tokio::pin!(drained);
            drained.as_mut().enable();
            if self.shared.in_flight.load(Ordering::SeqCst) == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, drained).await.is_err() {
                return self.shared.in_flight.load(Ordering::SeqCst) == 0;
            }
        }
    }

    /// Refuse new turns on every runner first, then drain them all within
    /// one shared `within`. Returns whether every in-flight turn finished.
    /// For a process that hosts several pulses: none of them starts a turn
    /// while another is still draining.
    pub(crate) async fn quiesce_all(runners: &[TurnRunner], within: Duration) -> bool {
        for runner in runners {
            runner.refuse_new();
        }
        let deadline = Instant::now() + within;
        let mut drained = true;
        for runner in runners {
            let left = deadline.saturating_duration_since(Instant::now());
            drained &= runner.drain(left).await;
        }
        drained
    }

    /// [`refuse_new`], then [`drain`] for up to `within`.
    ///
    /// [`refuse_new`]: TurnRunner::refuse_new
    /// [`drain`]: TurnRunner::drain
    #[cfg(test)]
    pub(crate) async fn close(&self, within: Duration) -> bool {
        self.refuse_new();
        self.drain(within).await
    }
}

/// The plugin-facing half of the runner.
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
impl TurnRunner {
    /// A plugin's handle for running turns on `channel`, as
    /// `AuthIdentity::Plugin { name, channel }`. Refuses a reserved, empty
    /// or malformed channel, so a misconfigured plugin fails at init rather
    /// than speaking as the owner. Every handle for the same `name` shares
    /// one pool and one rate bucket.
    pub fn for_plugin(&self, name: &'static str, channel: &str) -> Result<PluginTurns, BindError> {
        if channel.is_empty()
            || channel.len() > MAX_CHANNEL_LEN
            || channel.contains("..")
            || channel.contains('/')
        {
            return Err(BindError::InvalidChannel {
                plugin: name,
                channel: channel.to_string(),
            });
        }
        if is_reserved_channel(channel) {
            return Err(BindError::ReservedChannel {
                plugin: name,
                channel: channel.to_string(),
            });
        }
        Ok(self.handle(name, channel))
    }

    /// A handle for `name` on `channel`, with the plugin's shared budget.
    fn handle(&self, name: &'static str, channel: &str) -> PluginTurns {
        let budget = self
            .shared
            .budgets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name)
            .or_insert_with(Budget::new)
            .clone();
        PluginTurns {
            runner: self.clone(),
            name,
            channel: channel.to_string(),
            budget,
            slot_wait: SlotWait::Never,
        }
    }

    fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// Count a turn as in flight, unless shutdown has started. The count is
    /// taken before the closed check so [`drain`] either sees this turn or
    /// this turn sees [`refuse_new`].
    ///
    /// [`drain`]: TurnRunner::drain
    /// [`refuse_new`]: TurnRunner::refuse_new
    fn enter(&self) -> Result<Flight, TurnError> {
        self.shared.in_flight.fetch_add(1, Ordering::SeqCst);
        let flight = Flight(Arc::clone(&self.shared));
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(TurnError::ShuttingDown);
        }
        Ok(flight)
    }

    /// The bound state, waiting for the bind if it has not happened yet.
    async fn state(&self) -> Result<Arc<AppState>, TurnError> {
        let deadline = Instant::now() + self.shared.ready_timeout;
        loop {
            let changed = self.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.shared.closed.load(Ordering::SeqCst) {
                return Err(TurnError::ShuttingDown);
            }
            if let Some(weak) = self.shared.state.get() {
                return weak.upgrade().ok_or(TurnError::ShuttingDown);
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return Err(TurnError::NotReady);
            }
        }
    }
}

/// One in-flight turn; dropping it releases the count.
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
struct Flight(Arc<Shared>);

impl Drop for Flight {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.drained.notify_waiters();
        }
    }
}

/// A message a plugin wants answered.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
pub struct PluginTurn {
    pub message: String,
    /// The transport's id for whoever spoke (a Discord user id, a fixed
    /// voice identity). Resolved against `[owner]` like any external sender.
    pub sender: Option<String>,
}

/// One plugin's way into the turn pipeline: its principal, its channel, and
/// the plugin's shared pool and rate bucket.
#[derive(Clone)]
#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
pub struct PluginTurns {
    runner: TurnRunner,
    name: &'static str,
    channel: String,
    budget: Budget,
    slot_wait: SlotWait,
}

#[cfg_attr(
    not(any(feature = "voice", feature = "discord-text")),
    allow(dead_code)
)]
impl PluginTurns {
    /// This handle's turns wait for a pool slot as `wait` says.
    #[must_use]
    pub fn waiting_for_a_slot(mut self, wait: SlotWait) -> Self {
        self.slot_wait = wait;
        self
    }

    /// The channel this plugin's turns run on.
    #[must_use]
    #[cfg_attr(not(feature = "discord-text"), allow(dead_code))]
    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// Run `turn` end to end, exactly as `/chat` would, as this plugin's
    /// principal. `sink` receives turn events as they happen, if given.
    pub async fn run(
        &self,
        turn: PluginTurn,
        sink: Option<TurnSink>,
    ) -> Result<ChatResponse, TurnError> {
        if self.runner.is_closed() {
            return Err(TurnError::ShuttingDown);
        }
        self.rate_token().await?;
        // Slot first, then the flight: a turn still waiting for a slot is
        // not in flight, and one that gets its slot after shutdown started
        // is refused here instead of starting.
        let _permit = self.slot().await?;
        let _flight = self.runner.enter()?;
        let state = self.runner.state().await?;
        let request = ChatRequest {
            message: turn.message,
            channel: self.channel.clone(),
            sender: turn.sender,
        };
        self.admit_guest(&state, &request)?;
        let who = AuthIdentity::Plugin {
            name: self.name,
            channel: self.channel.clone(),
        };
        run_turn(state, request, who, sink)
            .await
            .map_err(|(status, message)| TurnError::Refused(status, message))
    }

    /// The `[plugin_turns]` caps, for a guest's turn; the owner's are never
    /// counted. A request that would not resolve is left for the turn
    /// pipeline to refuse.
    fn admit_guest(&self, state: &AppState, request: &ChatRequest) -> Result<(), TurnError> {
        let Ok(key) = crate::server::handlers::chat::resolve_plugin_key(
            self.name,
            &self.channel,
            request,
            &state.config,
        ) else {
            return Ok(());
        };
        if !key.is_guest() {
            return Ok(());
        }
        let kind = if self.channel.eq_ignore_ascii_case(VOICE_CHANNEL) {
            GuestKind::Call
        } else {
            GuestKind::Sender
        };
        let admitted = self
            .budget
            .guests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .admit(
                key.as_str(),
                kind,
                &state.config.plugin_turns,
                Instant::now().into_std(),
                chrono::Utc::now().date_naive(),
            );
        admitted.map_err(|refusal| {
            tracing::warn!("plugin {}: turn for {key} refused: {refusal:?}", self.name);
            match refusal {
                GuestRefusal::DailyBudgetSpent => TurnError::DailyBudgetSpent,
                GuestRefusal::CallTurnsSpent => TurnError::CallTurnsSpent,
                GuestRefusal::SenderRateLimited => TurnError::SenderRateLimited,
            }
        })
    }

    /// A token from the plugin's rate bucket. With [`SlotWait::Forever`] the
    /// caller has queued work it must not drop, so it waits for one (and
    /// gives up only when shutdown starts) instead of failing (audit R2-9);
    /// any other handle fails at once with [`TurnError::RateLimited`].
    async fn rate_token(&self) -> Result<(), TurnError> {
        loop {
            let changed = self.runner.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.runner.is_closed() {
                return Err(TurnError::ShuttingDown);
            }
            let wait = match self.budget.rate.try_acquire_or_wait().await {
                Ok(()) => return Ok(()),
                Err(wait) => wait,
            };
            if self.slot_wait != SlotWait::Forever {
                tracing::warn!("plugin {}: turn refused, over its rate limit", self.name);
                return Err(TurnError::RateLimited);
            }
            let wait = wait.unwrap_or(MAX_RATE_WAIT).min(MAX_RATE_WAIT);
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = changed => {}
            }
        }
    }

    /// A slot in the plugin's pool, waiting as [`SlotWait`] allows.
    async fn slot(&self) -> Result<OwnedSemaphorePermit, TurnError> {
        let pool = Arc::clone(&self.budget.pool);
        let acquired = match self.slot_wait {
            SlotWait::Never => return pool.try_acquire_owned().map_err(|_| TurnError::Busy),
            SlotWait::Up(limit) => tokio::time::timeout(limit, pool.acquire_owned())
                .await
                .map_err(|_| TurnError::Busy)?,
            SlotWait::Forever => pool.acquire_owned().await,
        };
        // The pool is never closed; a closed one would mean shutdown.
        acquired.map_err(|_| TurnError::ShuttingDown)
    }
}

#[cfg(test)]
impl TurnRunner {
    /// How many turns are in flight (entered, not yet finished).
    pub(crate) fn in_flight(&self) -> usize {
        self.shared.in_flight.load(Ordering::SeqCst)
    }

    /// Wait until `n` turns are in flight — a turn counts once it is past
    /// its slot and waiting for the bind or running.
    pub(crate) async fn wait_in_flight(&self, n: usize) {
        while self.in_flight() < n {
            tokio::task::yield_now().await;
        }
    }

    /// An unbound runner with a short readiness wait, for tests.
    pub(crate) fn with_test_timeout(ready_timeout: Duration) -> Self {
        Self::with_ready_timeout(ready_timeout)
    }

    /// The principal a plugin's turns would carry, bypassing `for_plugin`'s
    /// checks — lets a test prove `run_turn` refuses on its own.
    pub(crate) fn unchecked_plugin(&self, name: &'static str, channel: &str) -> PluginTurns {
        self.handle(name, channel)
    }
}

#[cfg(test)]
impl PluginTurns {
    /// This handle with a rate bucket of its own, for tests.
    pub(crate) fn with_rate_limit(mut self, burst: u32, per_second: f64) -> Self {
        self.budget.rate = Arc::new(RateLimiter::new(burst, per_second));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_channels_are_refused_case_insensitively() {
        let runner = TurnRunner::new();
        for channel in [
            "tui",
            "system",
            "reflection",
            "comms",
            "chat",
            "tui-chat",
            "TUI",
            "Comms",
            "Chat",
        ] {
            assert!(
                runner.for_plugin("test", channel).is_err(),
                "{channel:?} must be refused"
            );
        }
        for channel in ["", "a/b", "..", &"x".repeat(65)] {
            assert!(
                matches!(
                    runner.for_plugin("test", channel),
                    Err(BindError::InvalidChannel { .. })
                ),
                "{channel:?}"
            );
        }
        assert_eq!(
            runner.for_plugin("test", "tui").err(),
            Some(BindError::ReservedChannel {
                plugin: "test",
                channel: "tui".to_string()
            })
        );
        let turns = runner
            .for_plugin("test", "discord")
            .expect("discord is fine");
        assert_eq!(turns.channel(), "discord");
    }

    #[tokio::test]
    async fn an_unbound_runner_gives_up_after_the_readiness_wait() {
        let runner = TurnRunner::with_test_timeout(Duration::from_millis(50));
        let turns = runner.for_plugin("test", "discord").unwrap();
        let outcome = turns
            .run(
                PluginTurn {
                    message: "hi".into(),
                    sender: None,
                },
                None,
            )
            .await;
        assert_eq!(outcome.err(), Some(TurnError::NotReady));
    }

    #[tokio::test]
    async fn close_with_nothing_in_flight_returns_at_once() {
        let runner = TurnRunner::new();
        assert!(runner.close(Duration::from_millis(10)).await);
        let turns = runner.for_plugin("test", "discord").unwrap();
        let outcome = turns
            .run(
                PluginTurn {
                    message: "hi".into(),
                    sender: None,
                },
                None,
            )
            .await;
        assert_eq!(outcome.err(), Some(TurnError::ShuttingDown));
    }

    #[tokio::test]
    async fn quiesce_all_refuses_everywhere_then_drains_everything() {
        let (a, b) = (TurnRunner::new(), TurnRunner::new());
        let in_flight_on_b = b.enter().unwrap();

        let runners = [a.clone(), b.clone()];
        let quiescing = tokio::spawn(async move {
            TurnRunner::quiesce_all(&runners, Duration::from_secs(30)).await
        });
        while !a.is_closed() || !b.is_closed() {
            tokio::task::yield_now().await;
        }
        assert!(matches!(a.enter(), Err(TurnError::ShuttingDown)));
        assert!(matches!(b.enter(), Err(TurnError::ShuttingDown)));
        assert!(!quiescing.is_finished(), "b's turn is still in flight");

        drop(in_flight_on_b);
        assert!(quiescing.await.unwrap());
    }

    #[tokio::test]
    async fn close_wakes_a_turn_waiting_for_the_bind() {
        let runner = TurnRunner::new();
        let turns = runner.for_plugin("test", "discord").unwrap();
        let waiting = tokio::spawn(async move {
            turns
                .run(
                    PluginTurn {
                        message: "hi".into(),
                        sender: None,
                    },
                    None,
                )
                .await
        });
        // The turn is in flight and parked on the bind before the close.
        runner.wait_in_flight(1).await;
        assert!(
            !waiting.is_finished(),
            "the turn must be waiting for the bind"
        );
        assert!(runner.close(Duration::from_secs(5)).await);
        let outcome = waiting.await.unwrap();
        assert_eq!(outcome.err(), Some(TurnError::ShuttingDown));
    }
}
