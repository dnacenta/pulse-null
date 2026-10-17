//! The Discord text loop, in-process (PN-110).
//!
//! discord-echo 0.1.3 reads the gateway well, but its own forwarder posts
//! every message to `/chat` over HTTP without a credential. This module keeps
//! the crate's gateway and REST client and replaces only the forwarder: each
//! message runs as a turn through [`PluginTurns`], as the plugin's own
//! principal, and the reply is posted back to the channel it came from.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use discord_echo::client::DiscordClient;
use discord_echo::config::Config;
use discord_echo::dedup::DedupState;
use discord_echo::types::IncomingMessage;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::server::turns::{PluginTurn, PluginTurns};
use pulse_system_types::SetupPrompt;

/// Replies that start with one of these are the pulse choosing to stay
/// quiet; they are never posted. Same markers as discord-echo 0.1.3.
const SILENT_MARKERS: &[&str] = &["[SILENT]", "[NO_RESPONSE]", "No response requested"];

/// Config keys the in-process forwarder no longer reads.
const IGNORED_KEYS: &[&str] = &["chat_endpoint", "chat_secret"];

/// How long `stop` waits for each task before aborting it.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Gateway → forwarder queue depth, as in discord-echo.
const QUEUE_DEPTH: usize = 64;

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

/// Where replies go. The real one is Discord's REST API; tests use a fake.
pub(crate) trait ReplyPoster: Send + Sync {
    fn post<'a>(
        &'a self,
        channel_id: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

impl ReplyPoster for DiscordClient {
    fn post<'a>(
        &'a self,
        channel_id: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.send_message_by_id(channel_id, text)
                .await
                .map_err(|e| e.to_string())
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
    TurnFailed(String),
    /// The turn ran but Discord refused the reply.
    PostFailed(String),
}

/// Runs each incoming message as a turn and posts the reply.
pub(crate) struct Forwarder<P: ReplyPoster> {
    turns: PluginTurns,
    poster: Arc<P>,
    dedup: Arc<Mutex<DedupState>>,
}

impl<P: ReplyPoster> Forwarder<P> {
    pub(crate) fn new(turns: PluginTurns, poster: Arc<P>, dedup: Arc<Mutex<DedupState>>) -> Self {
        Self {
            turns,
            poster,
            dedup,
        }
    }

    /// Handle one message end to end.
    pub(crate) async fn handle(&self, msg: IncomingMessage) -> Handled {
        {
            let mut dedup = self.dedup.lock().await;
            if dedup.is_stale(&msg.timestamp) {
                tracing::debug!("discord: ignoring stale message {}", msg.message_id);
                return Handled::Stale;
            }
            if dedup.check_and_record_seen(&msg.message_id) {
                tracing::debug!("discord: skipping duplicate message {}", msg.message_id);
                return Handled::Duplicate;
            }
        }

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
        let reply = match self.turns.run(turn, None).await {
            Ok(reply) => reply.response,
            Err(e) => {
                tracing::warn!("discord: turn for message {} failed: {e}", msg.message_id);
                return Handled::TurnFailed(e.to_string());
            }
        };

        if reply.trim().is_empty() || is_silent(&reply) {
            tracing::debug!("discord: silent reply in #{channel_label}");
            return Handled::Silent;
        }

        // Second line of defence: a seen-id evicted from the window must
        // still not get a second reply.
        if self.dedup.lock().await.has_responded(&msg.message_id) {
            return Handled::Duplicate;
        }
        match self.poster.post(&msg.channel_id, &reply).await {
            Ok(()) => {
                self.dedup.lock().await.record_responded(&msg.message_id);
                Handled::Posted
            }
            Err(e) => {
                tracing::error!("discord: failed to reply in #{channel_label}: {e}");
                Handled::PostFailed(e)
            }
        }
    }

    /// Handle messages until the gateway goes away or `shutdown` fires.
    async fn run(self, mut rx: mpsc::Receiver<IncomingMessage>, shutdown: Arc<Notify>) {
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => {
                        self.handle(msg).await;
                    }
                    None => return,
                },
                () = shutdown.notified() => return,
            }
        }
    }
}

/// A running gateway plus its in-process forwarder.
pub(crate) struct DiscordLink {
    shutdown: Arc<Notify>,
    gateway: JoinHandle<()>,
    forwarder: JoinHandle<()>,
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
        let shutdown = Arc::new(Notify::new());
        let (tx, rx) = mpsc::channel::<IncomingMessage>(QUEUE_DEPTH);

        let gateway = tokio::spawn(discord_echo::gateway::run_gateway(
            config,
            tx,
            Arc::clone(&shutdown),
        ));
        let forwarder = Forwarder::new(turns, client, dedup);
        let forwarder = tokio::spawn(forwarder.run(rx, Arc::clone(&shutdown)));
        tracing::info!("discord: gateway started, messages run in-process");
        Self {
            shutdown,
            gateway,
            forwarder,
        }
    }

    /// Signal both tasks, give each a grace period, then abort what is left.
    /// An aborted turn is rolled back by the turn pipeline.
    pub(crate) async fn stop(self) {
        self.shutdown.notify_waiters();
        for (name, mut task) in [("gateway", self.gateway), ("forwarder", self.forwarder)] {
            if tokio::time::timeout(STOP_GRACE, &mut task).await.is_err() {
                tracing::warn!("discord: {name} did not stop within {STOP_GRACE:?}; aborting");
                task.abort();
            }
        }
        tracing::info!("discord: stopped");
    }
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

    pub(crate) fn start(&mut self) -> Result<(), String> {
        if self.link.is_some() {
            return Err("already running".to_string());
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
