//! The terminal UI (v2, PN-102): a client of the running daemon.
//!
//! `run_home` opens the entity menu; picking a row attaches to that daemon,
//! or starts one in this process when none answers, then drives an event-driven render loop: it
//! draws only when a key, a daemon message, a theme change or a running
//! effect says something changed, and never faster than one frame per
//! 16 ms. The loop itself never touches the network: `poller` does, and
//! reports over a channel.

pub mod app;
pub mod bar;
pub mod boot;
pub mod client;
pub mod floats;
pub mod home;
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
/// Entity state probe cadence while Home shows.
const HOME_TICK: Duration = Duration::from_secs(2);

/// `pulse-null up`: Home — the logo and the entity menu. Nothing is
/// started until a row is chosen.
pub async fn run_home() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let home_dir = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let rows = home::Home::scan(&cwd, home_dir.as_deref());
    let tui = crate::config::TuiConfig::default();
    let mut app = App::new(
        "pulse-null",
        "",
        "",
        ThemeWatcher::from_setting(&tui.theme),
        MotionLevel::parse(&tui.motion),
        Glyphs::from_setting(&tui.nerd_font),
    );
    app.start_home(rows);
    run_app(app, None).await
}

/// `pulse-null chat`: the TUI for the entity in `config`, straight into
/// Talk when a daemon is already up.
pub async fn run_chat(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let root = config.root_dir()?;
    let mut started = Vec::new();
    let session = Session::open(&config, root, &mut started).await;
    let mut app = App::new(
        "",
        "",
        "",
        ThemeWatcher::from_setting(&config.tui.theme),
        MotionLevel::parse(&config.tui.motion),
        Glyphs::from_setting(&config.tui.nerd_font),
    );
    app.enter_entity(&config);
    if session.attached {
        app.skip_boot();
    } else {
        app.screen = app::Screen::Boot;
        app.boot.status = "starting the daemon".to_string();
    }
    run_app_with(app, Some(session), started).await
}

async fn run_app(app: App, session: Option<Session>) -> Result<(), Box<dyn std::error::Error>> {
    run_app_with(app, session, Vec::new()).await
}

/// Terminal setup, the loop, terminal teardown, then stop every daemon this
/// process started (the stop flag feeds the same graceful-shutdown path as
/// SIGTERM, so sessions archive and pidfiles are removed).
async fn run_app_with(
    mut app: App,
    session: Option<Session>,
    mut started: Vec<Daemon>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut terminal, keyboard_enhanced) = enter_terminal();
    let result = event_loop(&mut terminal, &mut app, session, &mut started).await;
    leave_terminal(keyboard_enhanced);
    stop_daemons(started).await;
    result
}

fn enter_terminal() -> (ratatui::DefaultTerminal, bool) {
    let terminal = ratatui::init();
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
    (terminal, keyboard_enhanced)
}

fn leave_terminal(keyboard_enhanced: bool) {
    if keyboard_enhanced {
        let _ = execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
    }
    let _ = execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    let _ = execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
}

/// A daemon this process started for an entity directory.
struct Daemon {
    dir: std::path::PathBuf,
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

/// Stop every started daemon, joined, each bounded to 40 s.
async fn stop_daemons(started: Vec<Daemon>) {
    let mut set = tokio::task::JoinSet::new();
    for d in started {
        set.spawn(async move {
            let _ = d.stop.send(true);
            match tokio::time::timeout(Duration::from_secs(40), d.task).await {
                Ok(_) => tracing::info!("in-process daemon for {} stopped", d.dir.display()),
                Err(_) => tracing::warn!(
                    "in-process daemon for {} did not stop within 40 s",
                    d.dir.display()
                ),
            }
        });
    }
    while set.join_next().await.is_some() {}
}

/// Everything the loop needs to talk to one entity: the HTTP client and
/// the poller task that owns connectivity. Created when a row is chosen.
struct Session {
    client: Client,
    ctl: tokio::sync::mpsc::Sender<Ctl>,
    bg: tokio::sync::mpsc::Receiver<Bg>,
    poller: tokio::task::JoinHandle<()>,
    /// Whether `/health` answered when the session opened.
    attached: bool,
}

impl Session {
    /// Attach to the entity's daemon, or start one in this process for
    /// `root` when none answers (once per directory: a second pick of the
    /// same stopped entity reuses the daemon already booting).
    async fn open(config: &Config, root: std::path::PathBuf, started: &mut Vec<Daemon>) -> Self {
        let client = Client::new(
            &config.server.host,
            config.server.port,
            config.security.secret.clone(),
        );
        let attached = client.probe().await;
        if !attached && !started.iter().any(|d| d.dir == root) {
            tracing::info!("no daemon at {}; starting one in-process", client.base());
            let (stop, stop_rx) = tokio::sync::watch::channel(false);
            let cfg = config.clone();
            let dir = root.clone();
            let task = tokio::spawn(async move {
                if let Err(e) = crate::server::start_in(cfg, dir, Some(stop_rx)).await {
                    tracing::error!("daemon exited with error: {e}");
                }
            });
            started.push(Daemon {
                dir: root,
                stop,
                task,
            });
        }
        let (ctl, bg, poller) = poller::spawn(client.clone(), attached);
        Self {
            client,
            ctl,
            bg,
            poller,
            attached,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.poller.abort();
    }
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
    mut session: Option<Session>,
    started: &mut Vec<Daemon>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut attached = session.as_ref().is_some_and(|s| s.attached);
    let mut events = EventStream::new();
    let mut boot_tick = tokio::time::interval(BOOT_TICK);
    let mut turn_tick = tokio::time::interval(TURN_TICK);
    let mut theme_tick = tokio::time::interval(THEME_TICK);
    let mut home_tick = tokio::time::interval(HOME_TICK);
    // Probe results for Home's rows, from a task per tick.
    let (states_tx, mut states_rx) =
        tokio::sync::mpsc::channel::<Vec<(std::path::PathBuf, home::EntityState)>>(4);
    // A ticker that is gated off for a while must not burst when it comes
    // back; skip the missed periods.
    for t in [
        &mut boot_tick,
        &mut turn_tick,
        &mut theme_tick,
        &mut home_tick,
    ] {
        t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    }
    let mut attach_deadline = Instant::now() + ATTACH_TIMEOUT;
    // The daemon address, for the boot status; set when a session opens.
    let mut daemon_base: Option<String> = session.as_ref().map(|s| s.client.base().to_string());

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
                        match app.on_key(key) {
                            Action::Quit => return Ok(()),
                            Action::Open(i) => {
                                let Some(config) = app.home.rows[i].config.clone() else {
                                    continue;
                                };
                                let root = app.home.rows[i].dir.clone();
                                // A previous pick's poller and turn go; its daemon stays.
                                drop(session.take());
                                if let Some(task) = turn_task.take() {
                                    task.abort();
                                }
                                turn_rx = None;
                                let s = Session::open(&config, root, started).await;
                                attached = s.attached;
                                attach_deadline = Instant::now() + ATTACH_TIMEOUT;
                                daemon_base = Some(s.client.base().to_string());
                                app.enter_entity(&config);
                                app.screen = app::Screen::Home;
                                if attached {
                                    app.attached();
                                }
                                session = Some(s);
                            }
                            Action::Create => {
                                app.home.notice = Some(
                                    "create: not yet — run `pulse-null init` in a terminal".to_string(),
                                );
                            }
                            Action::None => {}
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
            msg = async {
                match session.as_mut() {
                    Some(s) => s.bg.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match msg {
                    Some(Bg::Attached) => {
                        attached = true;
                        app.attached();
                        app.daemon_back();
                    }
                    Some(Bg::Unreachable { retry_in }) => {
                        attached = false;
                        match app.screen {
                            app::Screen::Talk => app.daemon_lost(retry_in),
                            app::Screen::Home => {
                                app.home.notice = Some(format!(
                                    "no daemon answered at {} — retrying",
                                    daemon_base.as_deref().unwrap_or("?")
                                ));
                            }
                            app::Screen::Boot => {}
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
                                    transcript::Who::Entity
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
            _ = boot_tick.tick(), if matches!(app.screen, app::Screen::Boot | app::Screen::Home) => {
                app.tick();
                if app.screen == app::Screen::Boot && !attached && Instant::now() > attach_deadline {
                    app.boot.status = format!(
                        "no daemon at {} — still trying (q to quit)",
                        daemon_base.as_deref().unwrap_or("?")
                    );
                }
                dirty = true;
            }
            _ = home_tick.tick(), if app.screen == app::Screen::Home => {
                // One task probes every row; the loop never awaits the network.
                let targets: Vec<(std::path::PathBuf, Client)> = app
                    .home
                    .rows
                    .iter()
                    .filter_map(|r| r.client().map(|c| (r.dir.clone(), c)))
                    .collect();
                let tx = states_tx.clone();
                tokio::spawn(async move {
                    let mut set = tokio::task::JoinSet::new();
                    for (dir, c) in targets {
                        set.spawn(async move { (dir, home::EntityState::from(c.probe_detail().await)) });
                    }
                    let mut states = Vec::new();
                    while let Some(Ok(s)) = set.join_next().await {
                        states.push(s);
                    }
                    let _ = tx.try_send(states);
                });
            }
            Some(states) = states_rx.recv() => {
                app.home.apply_states(&states);
                dirty = app.screen == app::Screen::Home;
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
            let Some(s) = session.as_ref() else {
                continue;
            };
            if let Some(task) = turn_task.take() {
                task.abort();
            }
            let (tx, rx) = tokio::sync::mpsc::channel(256);
            let c = s.client.clone();
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
            let _ = s.ctl.try_send(Ctl::Refresh);
            dirty = true;
        }
    }
}
