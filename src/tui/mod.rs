//! The terminal UI (v2, PN-102): a client of the running daemon.
//!
//! `run` attaches to the daemon named in the config, or starts one in this
//! process when none answers, then drives an event-driven render loop: it
//! draws only when a key, a daemon message, a theme change or a running
//! effect says something changed, and never faster than one frame per
//! 16 ms. The loop itself never touches the network: `poller` does, and
//! reports over a channel.

pub mod app;
pub mod bar;
pub mod boot;
pub mod client;
pub mod floats;
pub mod keymap;
pub mod motion;
pub mod pages;
pub mod pane;
pub mod poller;
pub mod prompt;
pub mod text;
pub mod theme;
pub mod transcript;

use std::io::Write as _;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyboardEnhancementFlags, MouseEventKind};
use crossterm::execute;
use tokio::time::MissedTickBehavior;
use tokio_stream::StreamExt as _;

use crate::config::Config;

use app::{Action, App};
use bar::Glyphs;
use client::{ChatEvent, Client, ClientError};
use motion::MotionLevel;
use poller::{Bg, Ctl};
use theme::ThemeWatcher;

/// How long the boot screen waits for a daemon we started before it says so.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);
/// Transcript rows per wheel notch.
const WHEEL_ROWS: i32 = 3;
/// Shortest interval between two frames (62.5 fps ceiling).
const FRAME: Duration = Duration::from_millis(16);
/// Spinner / aurora cadence on the boot screen.
const BOOT_TICK: Duration = Duration::from_millis(80);
/// Spinner / elapsed-time cadence while a reply is in flight.
const TURN_TICK: Duration = Duration::from_millis(100);
/// Omarchy theme file poll cadence.
const THEME_TICK: Duration = Duration::from_secs(2);

/// Run the TUI for the pulse described by `config`, starting with the boot
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
    let (daemon_stop, stop_rx) = tokio::sync::watch::channel(false);
    let attached_at_start = client.probe().await;
    if !attached_at_start {
        tracing::info!("no daemon at {}; starting one in-process", client.base());
        let cfg = config.clone();
        daemon_task = Some(tokio::spawn(async move {
            if let Err(e) = crate::server::start_with_shutdown(cfg, Some(stop_rx)).await {
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
    // Mouse capture is what makes the wheel ours: without it the terminal
    // emulates the wheel as ↑/↓ keys, which the prompt reads as history.
    // Text selection is Shift+drag while the TUI runs, as in any TUI.
    let _ = execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);

    let mut app = App::new(
        &config.pulse.name,
        &config.llm.model,
        &config.pulse.owner_alias,
        ThemeWatcher::from_setting(&config.tui.theme),
        MotionLevel::parse(&config.tui.motion),
        Glyphs::from_setting(&config.tui.nerd_font),
    );
    if !attached_at_start {
        app.boot.status = "starting the daemon".to_string();
    } else if skip_boot {
        app.skip_boot();
    }

    let (ctl, bg, poller_task) = poller::spawn(client.clone(), attached_at_start);
    let result = event_loop(&mut terminal, &mut app, &client, ctl, bg, attached_at_start).await;
    poller_task.abort();

    if keyboard_enhanced {
        let _ = execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
    }
    let _ = execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    let _ = execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();

    // We started the daemon: stop it the way systemd would, so sessions
    // archive and the pidfile is removed. The stop flag feeds the same
    // graceful-shutdown path as SIGTERM.
    if let Some(task) = daemon_task {
        let _ = daemon_stop.send(true);
        match tokio::time::timeout(Duration::from_secs(40), task).await {
            Ok(_) => tracing::info!("in-process daemon stopped"),
            Err(_) => tracing::warn!("in-process daemon did not stop within 40 s"),
        }
    }

    result
}

/// One item from the chat stream. Returns true when the stream is over.
fn on_turn_item(app: &mut App, item: Option<Result<ChatEvent, ClientError>>) -> bool {
    match item {
        Some(Ok(ev)) => {
            app.talk.on_event(ev);
            false
        }
        Some(Err(e)) => {
            tracing::warn!("chat stream error: {e}");
            app.talk.stream_closed();
            true
        }
        None => {
            app.talk.stream_closed();
            true
        }
    }
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    client: &Client,
    ctl: tokio::sync::mpsc::Sender<Ctl>,
    mut bg: tokio::sync::mpsc::Receiver<Bg>,
    mut attached: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut events = EventStream::new();
    let mut boot_tick = tokio::time::interval(BOOT_TICK);
    let mut turn_tick = tokio::time::interval(TURN_TICK);
    let mut theme_tick = tokio::time::interval(THEME_TICK);
    // A ticker that is gated off for a while must not burst when it comes
    // back; skip the missed periods.
    for t in [&mut boot_tick, &mut turn_tick, &mut theme_tick] {
        t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    }
    let attach_deadline = Instant::now() + ATTACH_TIMEOUT;

    // The in-flight chat turn, if any: its event receiver and the task that
    // pumps the SSE stream into it. Dropping the task drops the response body,
    // which is what cancels the turn on the daemon.
    let mut turn_rx: Option<tokio::sync::mpsc::Receiver<Result<ChatEvent, ClientError>>> = None;
    let mut turn_task: Option<tokio::task::JoinHandle<()>> = None;

    let mut dirty = true;
    let mut last_draw = Instant::now() - FRAME;

    if attached {
        app.attached();
    }

    loop {
        // Frame pacer: draw when something changed, never more often than
        // one frame per FRAME. Effects and scroll glides keep asking for the
        // next frame until they settle.
        if dirty && last_draw.elapsed() >= FRAME {
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
            last_draw = Instant::now();
            dirty = app.motion.take_settled()
                || app.motion.is_running()
                || app.talk.transcript.is_animating();
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
                    Some(Ok(Event::Mouse(m))) => {
                        let rows = match m.kind {
                            MouseEventKind::ScrollUp => -WHEEL_ROWS,
                            MouseEventKind::ScrollDown => WHEEL_ROWS,
                            _ => 0,
                        };
                        if rows != 0 {
                            app.on_wheel(rows);
                            dirty = true;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                }
            }
            msg = bg.recv() => {
                match msg {
                    Some(Bg::Attached) => {
                        attached = true;
                        app.attached();
                        app.daemon_back();
                    }
                    Some(Bg::Unreachable { retry_in }) => {
                        attached = false;
                        if app.screen != app::Screen::Boot {
                            app.daemon_lost(retry_in);
                        }
                    }
                    Some(Bg::Bar(update)) => app.apply_bar(&update),
                    Some(Bg::History(msgs)) => {
                        let items = msgs
                            .into_iter()
                            .map(|m| {
                                let who = if m.role == "user" {
                                    transcript::Who::Owner
                                } else {
                                    transcript::Who::Pulse
                                };
                                (who, m.text, m.tools)
                            })
                            .collect();
                        app.talk.load_history(items);
                    }
                    Some(Bg::HistoryBusy) => app
                        .talk
                        .notice("history: a turn holds the session — loading when it ends"),
                    Some(Bg::HistoryUnavailable(why)) => {
                        app.talk.notice(&format!("history unavailable: {why}"));
                    }
                    // Rows feed the Watch page in the next plan; the poller
                    // already refreshed the bar for this one.
                    Some(Bg::Row) => {}
                    None => return Err("connectivity task stopped".into()),
                }
                dirty = true;
            }
            turn = async {
                match turn_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let mut ended = on_turn_item(app, turn);
                // Drain whatever else has already arrived so one frame shows
                // every delta that came in since the last one.
                while !ended {
                    match turn_rx.as_mut().map(|rx| rx.try_recv()) {
                        Some(Ok(item)) => ended = on_turn_item(app, Some(item)),
                        Some(Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)) => {
                            ended = on_turn_item(app, None);
                        }
                        _ => break,
                    }
                }
                if ended {
                    turn_rx = None;
                    turn_task = None;
                }
                dirty = true;
            }
            _ = turn_tick.tick(), if app.talk.turn_active() => {
                app.talk.tick();
                dirty = true;
            }
            _ = boot_tick.tick(), if app.screen == app::Screen::Boot => {
                app.tick();
                if !attached && Instant::now() > attach_deadline {
                    app.boot.status = format!(
                        "no daemon at {} — still trying (q to quit)",
                        client.base()
                    );
                }
                dirty = true;
            }
            _ = theme_tick.tick() => {
                if let Some(previous) = app.theme.poll() {
                    app.theme_changed(previous);
                    dirty = true;
                }
            }
            // Something is dirty but the pacer said "not yet": wake for it.
            _ = tokio::time::sleep_until((last_draw + FRAME).into()), if dirty => {}
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
            // A finished turn is a ledger row; ask for fresh bar facts once
            // the poller sees it (debounced there).
            let _ = ctl.try_send(Ctl::Refresh);
            dirty = true;
        }
    }
}
