//! The Discord text loop, in-process (PN-110).
//!
//! discord-echo 0.1.3 reads the gateway well, but its own forwarder posts
//! every message to `/chat` over HTTP without a credential. This module keeps
//! the crate's gateway and REST client and replaces only the forwarder: each
//! message runs as a turn through [`PluginTurns`], as the plugin's own
//! principal, and the reply is posted back to the channel it came from.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use discord_echo::client::DiscordClient;
use discord_echo::config::Config;
use discord_echo::dedup::DedupState;
use discord_echo::types::IncomingMessage;
use tokio::sync::{mpsc, watch, Mutex, Notify};
use tokio::task::{JoinHandle, JoinSet};

use crate::server::turns::{PluginTurn, PluginTurns, SlotWait, TurnError, DRAIN_WINDOW};
use pulse_system_types::SetupPrompt;

/// Replies that start with one of these are the pulse choosing to stay
/// quiet; they are never posted. Same markers as discord-echo 0.1.3.
const SILENT_MARKERS: &[&str] = &["[SILENT]", "[NO_RESPONSE]", "No response requested"];

/// Config keys the in-process forwarder no longer reads.
const IGNORED_KEYS: &[&str] = &["chat_endpoint", "chat_secret"];

/// How long `stop` waits for the gateway before aborting it.
const GATEWAY_GRACE: Duration = Duration::from_secs(5);

/// Gateway → forwarder queue depth, as in discord-echo.
const QUEUE_DEPTH: usize = 64;

/// Messages one Discord channel may have queued behind its turn in flight.
/// Past this, new messages on the channel are dropped and said so in the
/// log (audit R2-8): a spammer cannot queue hours of paid turns.
pub(crate) const LANE_DEPTH: usize = 16;

/// A queued message older than this when its turn comes is dropped: the
/// conversation has moved on, and an answer this late is noise (audit R2-8).
const MAX_QUEUED_AGE: Duration = Duration::from_secs(5 * 60);

/// Whether `reply` is the pulse choosing not to answer.
#[must_use]
fn is_silent(reply: &str) -> bool {
    let trimmed = reply.trim();
    SILENT_MARKERS
        .iter()
        .any(|marker| trimmed.starts_with(marker))
}

/// Warn about loopback-era keys still present in a Discord plugin's config.
/// Names only, never values: `chat_secret` is a credential.
pub(crate) fn warn_ignored_keys(plugin: &str, raw: &toml::Value) {
    for key in IGNORED_KEYS {
        if raw.get(key).is_some() {
            tracing::warn!(
                "{plugin}: `{key}` is ignored — messages run in-process now; remove it from the config"
            );
        }
    }
}

/// The prompts both Discord adapters ask during setup.
pub(crate) fn setup_prompts() -> Vec<SetupPrompt> {
    vec![
        SetupPrompt {
            key: "bot_token".to_string(),
            question: "Discord bot token:".to_string(),
            default: None,
            required: true,
            secret: true,
        },
        SetupPrompt {
            key: "guild_id".to_string(),
            question: "Discord server (guild) ID:".to_string(),
            default: None,
            required: true,
            secret: false,
        },
    ]
}

/// Discord refused a reply, with what its client said.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("discord refused the reply: {0}")]
pub(crate) struct PostError(pub(crate) String);

/// Where replies go. The real one is Discord's REST API; tests use a fake.
pub(crate) trait ReplyPoster: Send + Sync {
    fn post<'a>(
        &'a self,
        channel_id: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PostError>> + Send + 'a>>;
}

impl ReplyPoster for DiscordClient {
    fn post<'a>(
        &'a self,
        channel_id: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PostError>> + Send + 'a>> {
        Box::pin(async move {
            self.send_message_by_id(channel_id, text)
                .await
                .map_err(|e| PostError(e.to_string()))
        })
    }
}

/// What became of one incoming message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Handled {
    /// Older than the dedup staleness window (a replay after reconnect).
    Stale,
    /// Already seen, or already answered.
    Duplicate,
    /// The pulse answered and the reply was posted.
    Posted,
    /// The pulse chose silence, or said nothing.
    Silent,
    /// The turn did not run or failed.
    TurnFailed(TurnError),
    /// The turn ran but Discord refused the reply.
    PostFailed(PostError),
}

/// Runs each incoming message as a turn and posts the reply.
///
/// Messages are checked (stale, duplicate) the moment they arrive, then
/// queued per Discord channel: one channel's replies keep their order, and
/// different channels run concurrently, up to the plugin's pool.
pub(crate) struct Forwarder<P: ReplyPoster> {
    turns: PluginTurns,
    poster: Arc<P>,
    dedup: Arc<Mutex<DedupState>>,
    /// See [`MAX_QUEUED_AGE`]; tests shorten it.
    max_queued_age: Duration,
}

impl<P: ReplyPoster> Forwarder<P> {
    /// The pool bounds how many turns run at once; a message queued behind
    /// a full pool waits for a slot rather than being dropped.
    pub(crate) fn new(turns: PluginTurns, poster: Arc<P>, dedup: Arc<Mutex<DedupState>>) -> Self {
        Self {
            turns: turns.waiting_for_a_slot(SlotWait::Forever),
            poster,
            dedup,
            max_queued_age: MAX_QUEUED_AGE,
        }
    }

    /// This forwarder with a different queued-message age limit, for tests.
    #[cfg(test)]
    pub(crate) fn with_max_queued_age(mut self, age: Duration) -> Self {
        self.max_queued_age = age;
        self
    }

    /// Whether `msg`, about to be answered, waited past the age limit.
    fn too_old(&self, msg: &IncomingMessage) -> bool {
        let age = chrono::Utc::now().signed_duration_since(msg.timestamp);
        age.to_std().is_ok_and(|age| age > self.max_queued_age)
    }

    /// Handle one message end to end: [`admit`](Self::admit), then
    /// [`answer`](Self::answer).
    #[cfg(test)]
    pub(crate) async fn handle(&self, msg: IncomingMessage) -> Handled {
        match self.admit(&msg).await {
            Some(refused) => refused,
            None => self.answer(msg).await,
        }
    }

    /// The receive-time checks. `Some` names why the message is dropped;
    /// `None` admits it, and records it as seen.
    pub(crate) async fn admit(&self, msg: &IncomingMessage) -> Option<Handled> {
        let mut dedup = self.dedup.lock().await;
        if dedup.is_stale(&msg.timestamp) {
            tracing::debug!("discord: ignoring stale message {}", msg.message_id);
            return Some(Handled::Stale);
        }
        if dedup.check_and_record_seen(&msg.message_id) {
            tracing::debug!("discord: skipping duplicate message {}", msg.message_id);
            return Some(Handled::Duplicate);
        }
        None
    }

    /// Run an admitted message as a turn and post the reply.
    pub(crate) async fn answer(&self, msg: IncomingMessage) -> Handled {
        let channel_label = msg.channel_name.as_deref().unwrap_or("discord");
        tracing::info!(
            "discord: message from {} in #{} runs in-process on channel {:?}",
            msg.author_name,
            channel_label,
            self.turns.channel()
        );
        // The Discord user id is the sender: it is what `[owner] discord_id`
        // matches, so attribution is exactly what it was over HTTP.
        let turn = PluginTurn {
            message: msg.content,
            sender: Some(msg.author_id),
        };
        let reply = match self.reply_to(&msg.message_id, turn).await {
            Ok(reply) => reply,
            Err(failed) => return failed,
        };
        if reply.trim().is_empty() || is_silent(&reply) {
            tracing::debug!("discord: silent reply in #{channel_label}");
            return Handled::Silent;
        }
        let delivery = Delivery {
            message_id: &msg.message_id,
            channel_id: &msg.channel_id,
            channel_label,
        };
        self.post_once(delivery, &reply).await
    }

    /// The pulse's reply to one message, or why there is none.
    async fn reply_to(&self, message_id: &str, turn: PluginTurn) -> Result<String, Handled> {
        match self.turns.run(turn, None).await {
            Ok(reply) => Ok(reply.response),
            // Expected while the pulse stops: not worth a warning each.
            Err(TurnError::ShuttingDown) => {
                tracing::debug!("discord: message {message_id} not answered, shutting down");
                Err(Handled::TurnFailed(TurnError::ShuttingDown))
            }
            Err(e) => {
                tracing::warn!("discord: turn for message {message_id} failed: {e}");
                Err(Handled::TurnFailed(e))
            }
        }
    }

    /// Post the reply unless the message was already answered, and record
    /// that it was.
    async fn post_once(&self, delivery: Delivery<'_>, reply: &str) -> Handled {
        // Second line of defence: a seen-id evicted from the window must
        // still not get a second reply.
        if self.dedup.lock().await.has_responded(delivery.message_id) {
            return Handled::Duplicate;
        }
        match self.poster.post(delivery.channel_id, reply).await {
            Ok(()) => {
                self.dedup
                    .lock()
                    .await
                    .record_responded(delivery.message_id);
                Handled::Posted
            }
            Err(e) => {
                tracing::error!(
                    "discord: failed to reply in #{}: {e}",
                    delivery.channel_label
                );
                Handled::PostFailed(e)
            }
        }
    }
}

/// Where a reply goes, and the message it answers.
struct Delivery<'a> {
    message_id: &'a str,
    channel_id: &'a str,
    /// The channel's name for the log.
    channel_label: &'a str,
}

impl<P: ReplyPoster + 'static> Forwarder<P> {
    /// Take messages off `rx` until the gateway goes away or `stop` is set,
    /// admitting each the moment it arrives and queueing it on its
    /// channel's lane. Then let the lanes finish the turns they have in
    /// flight, for up to `grace`, starting no new one.
    pub(crate) async fn run(
        self: Arc<Self>,
        mut rx: mpsc::Receiver<IncomingMessage>,
        mut stop: watch::Receiver<bool>,
        grace: Duration,
    ) {
        let mut lanes = Lanes::default();
        loop {
            let msg = tokio::select! {
                biased;
                _ = stop.wait_for(|stopped| *stopped) => break,
                msg = rx.recv() => match msg {
                    Some(msg) => msg,
                    None => break,
                },
            };
            if self.admit(&msg).await.is_some() {
                continue;
            }
            lanes.queue(&self, msg, &stop);
        }
        // Close the gateway's queue now, so a gateway still sending sees it
        // gone and returns, rather than being aborted at its grace.
        drop(rx);
        lanes.finish(grace).await;
    }

    /// One channel's messages, in order. Once `stop` is set no further
    /// message starts a turn; the one in flight runs to the end. A message
    /// that waited past the age limit is dropped when its turn comes.
    async fn lane(
        self: Arc<Self>,
        mut queue: mpsc::Receiver<IncomingMessage>,
        stop: watch::Receiver<bool>,
    ) {
        while let Some(msg) = queue.recv().await {
            // The forwarder's own stop, or the whole pulse refusing turns
            // (shutdown started before this plugin was stopped).
            if *stop.borrow() || self.turns.is_shutting_down() {
                tracing::info!(
                    "discord: stopping; message {} left unanswered",
                    msg.message_id
                );
                return;
            }
            if self.too_old(&msg) {
                tracing::info!(
                    "discord: message {} waited past {:?} in the queue; dropped unanswered",
                    msg.message_id,
                    self.max_queued_age
                );
                continue;
            }
            self.answer(msg).await;
        }
    }
}

/// One queue and one worker per Discord channel.
#[derive(Default)]
struct Lanes {
    senders: HashMap<String, mpsc::Sender<IncomingMessage>>,
    workers: JoinSet<()>,
}

impl Lanes {
    /// Queue an admitted message on its channel's lane, starting the lane
    /// when there is none (or its worker is gone).
    fn queue<P: ReplyPoster + 'static>(
        &mut self,
        forwarder: &Arc<Forwarder<P>>,
        msg: IncomingMessage,
        stop: &watch::Receiver<bool>,
    ) {
        let channel = msg.channel_id.clone();
        let Some(msg) = self.send_on_existing_lane(&channel, msg) else {
            return;
        };
        let (lane, queue) = mpsc::channel(LANE_DEPTH);
        self.workers
            .spawn(Arc::clone(forwarder).lane(queue, stop.clone()));
        // A fresh lane is empty and its receiver alive: this cannot fail.
        let _ = lane.try_send(msg);
        self.senders.insert(channel, lane);
    }

    /// Hand `msg` to the channel's running lane. A full lane drops it, said
    /// in the log; the message comes back when a new lane must take it (no
    /// lane yet, or its worker panicked).
    fn send_on_existing_lane(
        &self,
        channel: &str,
        msg: IncomingMessage,
    ) -> Option<IncomingMessage> {
        let Some(lane) = self.senders.get(channel) else {
            return Some(msg);
        };
        match lane.try_send(msg) {
            Ok(()) => None,
            Err(mpsc::error::TrySendError::Full(msg)) => {
                tracing::warn!(
                    "discord: {LANE_DEPTH} messages already queued in channel {channel}; \
                     message {} from {} dropped unanswered",
                    msg.message_id,
                    msg.author_id
                );
                None
            }
            Err(mpsc::error::TrySendError::Closed(msg)) => Some(msg),
        }
    }

    /// Let the lanes finish the turns they have in flight, for up to
    /// `grace`, starting no new one; then abandon the rest.
    async fn finish(self, grace: Duration) {
        let Lanes {
            senders,
            mut workers,
        } = self;
        drop(senders);
        let finished = tokio::time::timeout(grace, async {
            while workers.join_next().await.is_some() {}
        })
        .await;
        if finished.is_err() {
            tracing::warn!(
                "discord: turns still in flight after {grace:?}; abandoning them unanswered"
            );
            workers.abort_all();
        }
    }
}

/// A running gateway plus its in-process forwarder.
pub(crate) struct DiscordLink {
    gateway_shutdown: Arc<Notify>,
    stop: watch::Sender<bool>,
    gateway: Option<JoinHandle<()>>,
    forwarder: JoinHandle<()>,
    /// How long the forwarder's in-flight turns get once stopped.
    grace: Duration,
}

impl DiscordLink {
    /// Connect the gateway and start forwarding its messages as turns.
    /// `dedup` outlives the link, so a restart does not replay messages.
    pub(crate) fn start(
        config: Arc<Config>,
        client: Arc<DiscordClient>,
        turns: PluginTurns,
        dedup: Arc<Mutex<DedupState>>,
    ) -> Self {
        let gateway_shutdown = Arc::new(Notify::new());
        let (tx, rx) = mpsc::channel::<IncomingMessage>(QUEUE_DEPTH);
        let gateway = tokio::spawn(discord_echo::gateway::run_gateway(
            config,
            tx,
            Arc::clone(&gateway_shutdown),
        ));
        let mut link = Self::forwarding(rx, Forwarder::new(turns, client, dedup), DRAIN_WINDOW);
        link.gateway_shutdown = gateway_shutdown;
        link.gateway = Some(gateway);
        tracing::info!("discord: gateway started, messages run in-process");
        link
    }

    /// The forwarder half alone, fed by `rx` — what [`start`](Self::start)
    /// builds on, and what tests drive without a gateway.
    pub(crate) fn forwarding<P: ReplyPoster + 'static>(
        rx: mpsc::Receiver<IncomingMessage>,
        forwarder: Forwarder<P>,
        grace: Duration,
    ) -> Self {
        let (stop, stopped) = watch::channel(false);
        let forwarder = tokio::spawn(Arc::new(forwarder).run(rx, stopped, grace));
        Self {
            gateway_shutdown: Arc::new(Notify::new()),
            stop,
            gateway: None,
            forwarder,
            grace,
        }
    }

    /// Stop taking messages, let the turns in flight finish and post (for
    /// up to the drain window), then return. Queued messages that have not
    /// started are left unanswered. The stop is level-triggered: a stop
    /// that lands while a turn runs is still seen when it ends.
    pub(crate) async fn stop(self) {
        self.stop.send_replace(true);
        // `notify_waiters` wakes the gateway's current waiters; the stored
        // permit catches a loop that was between two waits.
        self.gateway_shutdown.notify_waiters();
        self.gateway_shutdown.notify_one();
        if let Some(mut gateway) = self.gateway {
            if tokio::time::timeout(GATEWAY_GRACE, &mut gateway)
                .await
                .is_err()
            {
                tracing::warn!("discord: gateway did not stop within {GATEWAY_GRACE:?}; aborting");
                gateway.abort();
            }
        }
        // The forwarder bounds its own wait by `grace`; this is a backstop.
        let mut forwarder = self.forwarder;
        if tokio::time::timeout(self.grace + GATEWAY_GRACE, &mut forwarder)
            .await
            .is_err()
        {
            tracing::warn!("discord: forwarder did not stop; aborting");
            forwarder.abort();
        }
        tracing::info!("discord: stopped");
    }
}

/// Why the Discord link did not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum StartError {
    /// `start` was called on a link that is already running.
    #[error("already running")]
    AlreadyRunning,
}

/// What both Discord adapters run: the crate's config and REST client, the
/// plugin's turn handle, and the link while started.
pub(crate) struct DiscordText {
    config: Arc<Config>,
    client: Arc<DiscordClient>,
    turns: PluginTurns,
    dedup: Arc<Mutex<DedupState>>,
    link: Option<DiscordLink>,
}

impl DiscordText {
    /// Bind `plugin` to its configured `chat_channel_name`. Fails on a
    /// reserved channel, so no config can route Discord users to the owner
    /// by channel.
    pub(crate) fn new(
        plugin: &'static str,
        config: Config,
        raw: &toml::Value,
        ctx: &super::PluginContext,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        warn_ignored_keys(plugin, raw);
        let turns = ctx.turns.for_plugin(plugin, &config.chat_channel_name)?;
        let client = DiscordClient::new(config.bot_token.clone(), config.channels.clone());
        Ok(Self {
            config: Arc::new(config),
            client,
            turns,
            dedup: Arc::new(Mutex::new(DedupState::new())),
            link: None,
        })
    }

    /// The REST client, for the post tool.
    pub(crate) fn client(&self) -> Arc<DiscordClient> {
        Arc::clone(&self.client)
    }

    pub(crate) fn start(&mut self) -> Result<(), StartError> {
        if self.link.is_some() {
            return Err(StartError::AlreadyRunning);
        }
        self.link = Some(DiscordLink::start(
            Arc::clone(&self.config),
            Arc::clone(&self.client),
            self.turns.clone(),
            Arc::clone(&self.dedup),
        ));
        Ok(())
    }

    pub(crate) async fn stop(&mut self) {
        if let Some(link) = self.link.take() {
            link.stop().await;
        }
    }

    pub(crate) fn health(&self) -> super::PluginHealth {
        match self.link {
            Some(_) => super::PluginHealth::Healthy,
            None => super::PluginHealth::Down("not started".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_markers_only_count_at_the_start() {
        assert!(is_silent("[SILENT]"));
        assert!(is_silent("  [NO_RESPONSE] nothing to add"));
        assert!(is_silent("No response requested."));
        assert!(!is_silent("Hello"));
        assert!(!is_silent(""));
        assert!(!is_silent("I think [SILENT] is interesting"));
    }

    #[test]
    fn setup_prompts_ask_for_the_token_and_guild() {
        let keys: Vec<_> = setup_prompts().into_iter().map(|p| p.key).collect();
        assert_eq!(keys, ["bot_token", "guild_id"]);
    }
}
