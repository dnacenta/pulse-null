//! The terminal UI (v2, PN-102): a client of the running daemon.
//!
//! `run` attaches to the daemon named in the config, or starts one in this
//! process when none answers, then drives an event-driven render loop: it
//! draws only when a key, a daemon event, a theme change or a running effect
//! says something changed.

pub mod app;
pub mod bar;
pub mod boot;
pub mod client;
pub mod floats;
pub mod keymap;
pub mod motion;
pub mod pages;
pub mod pane;
pub mod prompt;
pub mod text;
pub mod theme;
pub mod transcript;

use std::io::Write as _;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyboardEnhancementFlags};
use crossterm::execute;
use tokio_stream::StreamExt as _;

use crate::config::Config;

use app::{Action, App};
use bar::{DaemonState, Glyphs};
use client::Client;
use motion::MotionLevel;
use theme::ThemeWatcher;

/// The live `/api/events` stream, boxed so the loop can hold it in an `Option`.
type LedgerStream = std::pin::Pin<
    Box<dyn futures_core::Stream<Item = Result<client::SseEvent, client::ClientError>> + Send>,
>;

/// How long to wait for a daemon we started ourselves.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);
/// Frame cadence while an effect is running.
const FX_TICK: Duration = Duration::from_millis(16);
/// Spinner / aurora cadence on the boot screen.
const BOOT_TICK: Duration = Duration::from_millis(80);
/// Spinner / elapsed-time cadence while a reply is in flight.
const TURN_TICK: Duration = Duration::from_millis(100);
/// Bar refresh cadence (`/health`, `/api/dashboard`, `/api/alerts/peek`).
const BAR_TICK: Duration = Duration::from_secs(5);
/// Omarchy theme file poll cadence.
const THEME_TICK: Duration = Duration::from_secs(2);
/// Reconnect backoff bounds after the daemon stops answering.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(8);

/// The delay after `previous` failed: doubled, capped.
#[must_use]
pub fn next_backoff(previous: Duration) -> Duration {
    (previous * 2).min(BACKOFF_MAX)
}

/// Run the TUI for the entity described by `config`, starting with the boot
/// screen (`pulse-null up`).
pub async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    run_with(config, false).await
}

/// `pulse-null chat`: when a daemon is already up, open straight into Talk.
pub async fn run_chat(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    run_with(config, true).await
}

async fn run_with(config: Config, skip_boot: bool) -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new(
        &config.server.host,
        config.server.port,
        config.security.secret.clone(),
    );

    // Attach or spawn. The daemon owns the provider, tools and sessions;
    // this process only ever talks to it over HTTP.
    let mut daemon_task = None;
    let attached_at_start = client.probe().await;
    if !attached_at_start {
        tracing::info!("no daemon at {}; starting one in-process", client.base());
        let cfg = config.clone();
        daemon_task = Some(tokio::spawn(async move {
            if let Err(e) = crate::server::start(cfg).await {
                tracing::error!("daemon exited with error: {e}");
            }
        }));
    }

    let mut terminal = ratatui::init();
    let keyboard_enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if keyboard_enhanced {
        let _ = execute!(
            std::io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        );
    }
    let _ = execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);

    let mut app = App::new(
        &config.entity.name,
        &config.llm.model,
        &config.entity.owner_alias,
        ThemeWatcher::from_setting(&config.tui.theme),
        MotionLevel::parse(&config.tui.motion),
        Glyphs::from_setting(&config.tui.nerd_font),
    );
    if !attached_at_start {
        app.boot.status = "starting the daemon".to_string();
    } else if skip_boot {
        app.skip_boot();
    }

    let result = event_loop(&mut terminal, &mut app, &client, attached_at_start).await;

    if keyboard_enhanced {
        let _ = execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
    }
    let _ = execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();

    // We started the daemon: shut it down the way systemd would, so sessions
    // archive and the pidfile is removed. The handler tokio installed in
    // `server::start` turns the signal into a graceful shutdown.
    if let Some(task) = daemon_task {
        // SAFETY: raise() is async-signal-safe and only delivers SIGTERM to
        // this process, whose handler is already installed by the daemon task.
        unsafe {
            libc::raise(libc::SIGTERM);
        }
        match tokio::time::timeout(Duration::from_secs(40), task).await {
            Ok(_) => tracing::info!("in-process daemon stopped"),
            Err(_) => tracing::warn!("in-process daemon did not stop within 40 s"),
        }
    }

    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    client: &Client,
    mut attached: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut events = EventStream::new();
    let mut fx_tick = tokio::time::interval(FX_TICK);
    let mut boot_tick = tokio::time::interval(BOOT_TICK);
    let mut bar_tick = tokio::time::interval(BAR_TICK);
    let mut theme_tick = tokio::time::interval(THEME_TICK);
    let attach_deadline = Instant::now() + ATTACH_TIMEOUT;
    // Probing: fast while a daemon we started comes up; exponential backoff
    // once a daemon we had has gone away.
    let mut backoff = Duration::from_millis(250);
    let mut next_probe = tokio::time::Instant::now() + backoff;
    let mut dirty = true;

    // Ledger stream: opened once attached; rows arrive as they happen.
    let mut ledger: Option<LedgerStream> = None;
    let mut last_event_id: Option<u64> = None;
    // The in-flight chat turn, if any: its event receiver and the task that
    // pumps the SSE stream into it. Dropping the task drops the response body,
    // which is what cancels the turn on the daemon.
    let mut turn_rx: Option<
        tokio::sync::mpsc::Receiver<Result<client::ChatEvent, client::ClientError>>,
    > = None;
    let mut turn_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut turn_tick = tokio::time::interval(TURN_TICK);

    if attached {
        app.attached();
        refresh_bar(app, client).await;
        load_history(app, client).await;
    }

    loop {
        if dirty {
            let started = Instant::now();
            let _ = execute!(
                std::io::stdout(),
                crossterm::terminal::BeginSynchronizedUpdate
            );
            terminal.draw(|f| app.render(f))?;
            let _ = execute!(
                std::io::stdout(),
                crossterm::terminal::EndSynchronizedUpdate
            );
            let _ = std::io::stdout().flush();
            app.frame_done(started.elapsed());
            // The frame that retires the last effect is drawn mid-effect;
            // one more pass paints the settled state.
            dirty = app.motion.take_settled();
        }

        if attached && ledger.is_none() {
            match client.events(last_event_id).await {
                Ok(s) => ledger = Some(Box::pin(s)),
                Err(e) => tracing::warn!("ledger stream unavailable: {e}"),
            }
        }

        tokio::select! {
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) => {
                        if app.on_key(key) == Action::Quit {
                            return Ok(());
                        }
                        dirty = true;
                    }
                    Some(Ok(Event::Paste(text))) => {
                        app.on_paste(&text);
                        dirty = true;
                    }
                    Some(Ok(Event::Resize(_, _))) => dirty = true,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                }
            }
            row = async {
                match ledger.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            } => {
                match row {
                    Some(Ok(ev)) => {
                        if let Some(id) = ev.id.as_deref().and_then(|s| s.parse().ok()) {
                            last_event_id = Some(id);
                        }
                        // Rows feed the Watch page in the next plan; today a
                        // new row refreshes the alert count.
                        refresh_bar(app, client).await;
                        dirty = true;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("ledger stream error: {e}");
                        ledger = None;
                        attached = false;
                        backoff = BACKOFF_MIN;
                        next_probe = tokio::time::Instant::now() + backoff;
                        app.daemon_lost(backoff);
                        dirty = true;
                    }
                    None => {
                        // A clean close is the daemon shutting down: treat it
                        // like a loss so the bar and prompt say so at once.
                        tracing::info!("ledger stream closed by the daemon");
                        ledger = None;
                        attached = false;
                        backoff = BACKOFF_MIN;
                        next_probe = tokio::time::Instant::now() + backoff;
                        app.daemon_lost(backoff);
                        dirty = true;
                    }
                }
            }
            turn = async {
                match turn_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match turn {
                    Some(Ok(ev)) => app.talk.on_event(ev),
                    Some(Err(e)) => {
                        tracing::warn!("chat stream error: {e}");
                        app.talk.stream_closed();
                        turn_rx = None;
                        turn_task = None;
                    }
                    None => {
                        app.talk.stream_closed();
                        turn_rx = None;
                        turn_task = None;
                    }
                }
                dirty = true;
            }
            _ = turn_tick.tick(), if app.talk.turn_active() || app.talk.transcript.is_animating() => {
                app.talk.tick();
                dirty = true;
            }
            _ = tokio::time::sleep_until(next_probe), if !attached => {
                if client.probe().await {
                    attached = true;
                    backoff = Duration::from_millis(250);
                    app.attached();
                    app.daemon_back();
                    refresh_bar(app, client).await;
                    load_history(app, client).await;
                    dirty = true;
                } else {
                    if app.screen == app::Screen::Boot {
                        if Instant::now() > attach_deadline {
                            app.boot.status = format!("no daemon at {} — still trying (q to quit)", client.base());
                        }
                    } else {
                        backoff = next_backoff(backoff.max(BACKOFF_MIN));
                        app.daemon_lost(backoff);
                    }
                    next_probe = tokio::time::Instant::now() + backoff;
                    dirty = true;
                }
            }
            _ = boot_tick.tick(), if app.screen == app::Screen::Boot => {
                app.tick();
                dirty = true;
            }
            _ = fx_tick.tick(), if app.motion.is_running() => {
                dirty = true;
            }
            _ = bar_tick.tick(), if attached => {
                refresh_bar(app, client).await;
                if app.bar.daemon == DaemonState::Unreachable {
                    attached = false;
                    ledger = None;
                    backoff = BACKOFF_MIN;
                    next_probe = tokio::time::Instant::now() + backoff;
                    app.daemon_lost(backoff);
                }
                dirty = true;
            }
            _ = theme_tick.tick() => {
                if let Some(previous) = app.theme.poll() {
                    app.theme_changed(previous);
                    dirty = true;
                }
            }
        }

        // The page asked for a send or a cancel; the loop owns the sockets.
        if app.talk.take_cancel() {
            if let Some(task) = turn_task.take() {
                task.abort();
            }
            turn_rx = None;
            app.talk.cancelled();
            dirty = true;
        }
        if let Some(text) = app.talk.take_outbox() {
            if let Some(task) = turn_task.take() {
                task.abort();
            }
            let (tx, rx) = tokio::sync::mpsc::channel(256);
            let c = client.clone();
            turn_task = Some(tokio::spawn(async move {
                match c.chat_stream("tui", &text).await {
                    Ok(stream) => {
                        let mut stream = std::pin::pin!(stream);
                        while let Some(item) = stream.next().await {
                            if tx.send(item).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                    }
                }
            }));
            turn_rx = Some(rx);
            dirty = true;
        }
    }
}

/// Load the owner's conversation on the `tui` channel into Talk.
async fn load_history(app: &mut App, client: &Client) {
    match client.history("tui").await {
        Ok(msgs) => {
            let items = msgs
                .into_iter()
                .map(|m| {
                    let who = if m.role == "user" {
                        transcript::Who::Owner
                    } else {
                        transcript::Who::Entity
                    };
                    (who, m.text, m.tools)
                })
                .collect();
            app.talk.load_history(items);
        }
        Err(e) => tracing::warn!("history unavailable: {e}"),
    }
}

/// Pull the bar's facts from the daemon. Failures leave the last values and
/// mark the daemon unreachable.
async fn refresh_bar(app: &mut App, client: &Client) {
    match client.health().await {
        Ok(h) => {
            app.bar.daemon = DaemonState::Connected;
            app.bar.isolation = h["isolation"].as_bool().unwrap_or(false);
        }
        Err(e) => {
            tracing::debug!("health: {e}");
            app.bar.daemon = DaemonState::Unreachable;
            return;
        }
    }
    if let Ok(d) = client.dashboard().await {
        // The dashboard reports "healthy" before it has enough signal frames to
        // judge; the bar says so instead of borrowing a verdict it cannot back.
        let ch = &d["cognitive_health"];
        let sufficient = ch["sufficient_data"].as_bool().unwrap_or(false);
        app.bar.health = if sufficient {
            ch["status"].as_str().map(str::to_string)
        } else {
            None
        };
    }
    if let Ok(n) = client.alerts_count().await {
        app.bar.alerts = Some(n);
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
