//! The one task that talks to the daemon on the TUI's behalf.
//!
//! The render loop never awaits the network. This task owns connectivity:
//! it probes until the daemon answers, keeps the ledger event stream open,
//! refreshes the bar on a cadence (three GETs joined into one round trip),
//! fetches history after every attach (in its own task, since a turn can
//! hold the session for minutes), and reports everything over a channel.
//! When the daemon goes away it backs off 0.5→8 s and says so.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use super::client::{Client, HistoryMessage};

/// What the poller tells the loop.
#[derive(Debug)]
pub enum Bg {
    /// The daemon answered `/health`; the ledger stream is being opened.
    Attached,
    /// The daemon did not answer; the next probe is in `retry_in`.
    Unreachable { retry_in: Duration },
    /// Fresh bar facts.
    Bar(BarUpdate),
    /// The `tui` channel's conversation, after an attach.
    History(Vec<HistoryMessage>),
    /// History could not be read yet: a turn holds the session. Sent once
    /// per attach; `History` follows when the turn ends.
    HistoryBusy,
    /// History gave up (an error, or the session never freed).
    HistoryUnavailable(String),
    /// A live ledger row arrived (the row itself lands with the Watch page).
    Row,
}

/// Bar facts from `/health`, `/api/dashboard`, `/api/alerts/peek`.
#[derive(Debug, Clone, Default)]
pub struct BarUpdate {
    pub isolation: bool,
    /// Cognitive status when the dashboard has enough data; `None` otherwise.
    pub health: Option<crate::wire::CognitiveStatus>,
    pub alerts: Option<usize>,
}

/// What the loop tells the poller.
#[derive(Debug)]
pub enum Ctl {
    /// Refresh the bar now (debounced to once per second).
    Refresh,
}

/// Probe cadence while a daemon we started ourselves is coming up.
const FAST_PROBE: Duration = Duration::from_millis(250);
/// How long fast probing lasts before a daemon that never came up is
/// treated like one that went away (backoff, `Unreachable` reports).
const FAST_PROBE_FOR: Duration = Duration::from_secs(10);
/// Reconnect backoff bounds after a daemon we had goes away.
pub const BACKOFF_MIN: Duration = Duration::from_millis(500);
pub const BACKOFF_MAX: Duration = Duration::from_secs(8);
/// Bar refresh cadence while attached.
const BAR_EVERY: Duration = Duration::from_secs(5);
/// A ledger row asks for a refresh at most this often.
const ROW_REFRESH_DEBOUNCE: Duration = Duration::from_secs(1);
/// History fetch: the daemon answers 503 while a turn holds the session,
/// which a streamed reply can do for as long as the provider's idle bound
/// (15 min by default). Retry that long, then say so.
const HISTORY_RETRY: Duration = Duration::from_secs(2);
const HISTORY_MAX_WAIT: Duration = Duration::from_secs(15 * 60);

/// The delay after `previous` failed: doubled, capped.
#[must_use]
pub fn next_backoff(previous: Duration) -> Duration {
    (previous * 2).min(BACKOFF_MAX)
}

/// Start the poller. `attached` says whether `/health` already answered.
pub fn spawn(
    client: Client,
    attached: bool,
) -> (
    mpsc::Sender<Ctl>,
    mpsc::Receiver<Bg>,
    tokio::task::JoinHandle<()>,
) {
    let (ctl_tx, ctl_rx) = mpsc::channel(8);
    let (bg_tx, bg_rx) = mpsc::channel(256);
    let handle = tokio::spawn(run(client, attached, ctl_rx, bg_tx));
    (ctl_tx, bg_rx, handle)
}

async fn run(
    client: Client,
    attached_at_start: bool,
    mut ctl: mpsc::Receiver<Ctl>,
    tx: mpsc::Sender<Bg>,
) {
    // Fast probing until the first attach (a daemon we spawned is booting);
    // exponential backoff after a daemon we had goes away. A daemon that
    // already answered goes through the same attach path (probe now, bar,
    // history) so `pulse-null chat` loads the conversation like a relaunch.
    let mut attached = false;
    let mut fast = !attached_at_start;
    let fast_since = Instant::now();
    let mut backoff = if attached_at_start {
        Duration::ZERO
    } else {
        FAST_PROBE
    };
    let mut last_id: Option<u64> = None;
    // Aborted with the poller: a `Session` drop aborts this task, and the
    // guard takes the history fetch down with it.
    let mut history_task = AbortOnDrop(None);

    loop {
        if !attached {
            tokio::time::sleep(backoff).await;
            if client.probe().await {
                attached = true;
                fast = false;
                backoff = FAST_PROBE;
                if tx.send(Bg::Attached).await.is_err() {
                    return;
                }
                if let Some(update) = refresh(&client).await {
                    let _ = tx.send(Bg::Bar(update)).await;
                }
                // Its own task: the session may be busy for minutes, and the
                // ledger stream and the bar must not wait behind it.
                history_task.replace(tokio::spawn(history(client.clone(), tx.clone())));
            } else {
                if fast && fast_since.elapsed() < FAST_PROBE_FOR {
                    continue;
                }
                fast = false;
                // First report is BACKOFF_MIN itself, then doubling to the cap.
                backoff = if backoff < BACKOFF_MIN {
                    BACKOFF_MIN
                } else {
                    next_backoff(backoff)
                };
                if tx
                    .send(Bg::Unreachable { retry_in: backoff })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            continue;
        }

        // Attached: hold the ledger stream open and refresh the bar on a
        // cadence until either says the daemon is gone.
        let stream = match client.events(last_id).await {
            Ok(s) => s,
            Err(super::client::ClientError::Status { status: 503, .. }) => {
                // The daemon is up but its event pool is full: wait, stay
                // attached, try again. Not a loss of the daemon.
                tracing::debug!("event stream pool full; retrying");
                tokio::time::sleep(BACKOFF_MIN).await;
                continue;
            }
            Err(e) => {
                tracing::warn!("ledger stream unavailable: {e}");
                attached = false;
                history_task.replace_none();
                backoff = BACKOFF_MIN;
                let _ = tx.send(Bg::Unreachable { retry_in: backoff }).await;
                continue;
            }
        };
        let mut stream = std::pin::pin!(stream);
        let mut ticker = tokio::time::interval(BAR_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick is immediate; the attach already refreshed
        let mut last_refresh = Instant::now();

        loop {
            tokio::select! {
                row = stream.next() => match row {
                    Some(Ok(ev)) => {
                        if let Some(id) = ev.id.as_deref().and_then(|s| s.parse().ok()) {
                            last_id = Some(id);
                        }
                        if tx.send(Bg::Row).await.is_err() {
                            return;
                        }
                        if last_refresh.elapsed() >= ROW_REFRESH_DEBOUNCE {
                            last_refresh = Instant::now();
                            if let Some(update) = refresh(&client).await {
                                let _ = tx.send(Bg::Bar(update)).await;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!("ledger stream error: {e}");
                        break;
                    }
                    None => {
                        tracing::info!("ledger stream closed by the daemon");
                        break;
                    }
                },
                _ = ticker.tick() => {
                    last_refresh = Instant::now();
                    match refresh(&client).await {
                        Some(update) => {
                            if tx.send(Bg::Bar(update)).await.is_err() {
                                return;
                            }
                        }
                        None => break,
                    }
                }
                c = ctl.recv() => match c {
                    Some(Ctl::Refresh) => {
                        if last_refresh.elapsed() >= ROW_REFRESH_DEBOUNCE {
                            last_refresh = Instant::now();
                            if let Some(update) = refresh(&client).await {
                                let _ = tx.send(Bg::Bar(update)).await;
                            }
                        }
                    }
                    None => return,
                },
            }
        }

        // Anything that broke the inner loop means the daemon is gone.
        attached = false;
        history_task.replace_none();
        backoff = BACKOFF_MIN;
        if tx
            .send(Bg::Unreachable { retry_in: backoff })
            .await
            .is_err()
        {
            return;
        }
    }
}

/// One round trip for the bar. `None` means `/health` failed: the daemon is
/// gone. Dashboard and alert failures are tolerated (their fields stay
/// `None`).
async fn refresh(client: &Client) -> Option<BarUpdate> {
    let (health, dashboard, alerts) =
        tokio::join!(client.health(), client.dashboard(), client.alerts_count());
    let health = match health {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!("health: {e}");
            return None;
        }
    };
    // The dashboard reports "healthy" before it has enough signal frames to
    // judge; the bar says so instead of borrowing a verdict.
    let cognitive = dashboard
        .ok()
        .and_then(|d| d.cognitive_health)
        .filter(|ch| ch.sufficient_data)
        .map(|ch| ch.status);
    Some(BarUpdate {
        isolation: health.isolation,
        health: cognitive,
        alerts: alerts.ok(),
    })
}

/// A task handle that aborts its task when dropped or replaced.
struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl AbortOnDrop {
    fn replace(&mut self, task: tokio::task::JoinHandle<()>) {
        self.replace_none();
        self.0 = Some(task);
    }

    fn replace_none(&mut self) {
        if let Some(t) = self.0.take() {
            t.abort();
        }
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.replace_none();
    }
}

/// The `tui` conversation, retried while the daemon reports the session
/// busy. Says so once, then delivers the history when the turn ends, or
/// gives up after [`HISTORY_MAX_WAIT`].
async fn history(client: Client, tx: mpsc::Sender<Bg>) {
    let started = Instant::now();
    let mut said_busy = false;
    loop {
        match client.history("tui").await {
            Ok(h) => {
                let _ = tx.send(Bg::History(h)).await;
                return;
            }
            Err(super::client::ClientError::Status { status: 503, .. }) => {
                if !said_busy {
                    said_busy = true;
                    if tx.send(Bg::HistoryBusy).await.is_err() {
                        return;
                    }
                }
                if started.elapsed() >= HISTORY_MAX_WAIT {
                    let _ = tx
                        .send(Bg::HistoryUnavailable(
                            "the session stayed busy; relaunch to load it".to_string(),
                        ))
                        .await;
                    return;
                }
                tokio::time::sleep(HISTORY_RETRY).await;
            }
            Err(e) => {
                tracing::warn!("history unavailable: {e}");
                let _ = tx.send(Bg::HistoryUnavailable(e.to_string())).await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut d = BACKOFF_MIN;
        let mut seen = vec![d];
        for _ in 0..6 {
            d = next_backoff(d);
            seen.push(d);
        }
        assert_eq!(
            seen.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
            vec![500, 1000, 2000, 4000, 8000, 8000, 8000]
        );
    }
}
