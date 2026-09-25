//! The terminal UI (v2, PN-102): a client of the running daemon.
//!
//! `run_home` opens the pulse menu; picking a row attaches to that daemon,
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
/// Pulse state probe cadence while Home shows.
const HOME_TICK: Duration = Duration::from_secs(2);

/// `pulse-null up`: Home — the logo and the pulse menu. Nothing is
/// started until a row is chosen.
pub async fn run_home() -> Result<(), Box<dyn std::error::Error>> {
    let rows = rescan_home();
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

/// `pulse-null chat`: the TUI for the pulse in `config`, straight into
/// Talk when a daemon is already up.
pub async fn run_chat(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let root = config.root_dir()?;
    let mut started = Vec::new();
    let client = Client::new(
        &config.server.host,
        config.server.port,
        config.security.secret.clone(),
    );
    let state = home::PulseState::from(client.probe_detail(&config.pulse.name).await);
    let session = Session::open(&config, root, state, &mut started)
        .map_err(|why| format!("cannot open {}: {why}", config.pulse.name))?;
    let mut app = App::new(
        "",
        "",
        "",
        ThemeWatcher::from_setting(&config.tui.theme),
        MotionLevel::parse(&config.tui.motion),
        Glyphs::from_setting(&config.tui.nerd_font),
    );
    app.enter_pulse(&config);
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
    // Between wizard prompts the terminal is cooked, so a Ctrl+c there
    // reaches this process as SIGINT. Swallow it: the child wizard dies
    // (it gets the default disposition on exec), the TUI carries on.
    let _sigint = tokio::spawn(async {
        loop {
            let _ = tokio::signal::ctrl_c().await;
        }
    });
    let mut term = enter_terminal();
    let result = event_loop(&mut term, &mut app, session, &mut started).await;
    leave_terminal(&term);
    if !started.is_empty() {
        eprintln!("stopping {} pulse(s) this shell started…", started.len());
    }
    stop_daemons(started).await;
    result
}

/// The terminal in TUI mode: alternate screen, raw mode, the flags we
/// pushed and must pop again.
struct Term {
    terminal: ratatui::DefaultTerminal,
    keyboard_enhanced: bool,
}

fn enter_terminal() -> Term {
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
    Term {
        terminal,
        keyboard_enhanced,
    }
}

fn leave_terminal(term: &Term) {
    if term.keyboard_enhanced {
        let _ = execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
    }
    let _ = execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    let _ = execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
}

/// A daemon this process started for a pulse directory.
struct Daemon {
    dir: std::path::PathBuf,
    stop: tokio::sync::watch::Sender<bool>,
    /// Ends with the daemon: `Err` is why it did not run.
    task: tokio::task::JoinHandle<Result<(), String>>,
}

/// Daemons whose task has ended, with why, removed from `started`.
async fn reap_daemons(started: &mut Vec<Daemon>) -> Vec<(std::path::PathBuf, String)> {
    let mut ended = Vec::new();
    let mut i = 0;
    while i < started.len() {
        if started[i].task.is_finished() {
            let d = started.remove(i);
            let why = match d.task.await {
                Ok(Ok(())) => "it exited".to_string(),
                Ok(Err(e)) => e,
                Err(e) => format!("it panicked: {e}"),
            };
            ended.push((d.dir, why));
        } else {
            i += 1;
        }
    }
    ended
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

/// Everything the loop needs to talk to one pulse: the HTTP client and
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
    /// Attach to the pulse's daemon (`state` is `Up`), or start one in this
    /// process for `root` when nothing listens (`Stopped`; once per
    /// directory — a second pick of the same pulse reuses the daemon
    /// already booting). Nothing is awaited: the probe already happened,
    /// and the poller confirms the attach. A port that answers for another
    /// pulse or not as a daemon is refused here as well as in the menu.
    fn open(
        config: &Config,
        root: std::path::PathBuf,
        state: home::PulseState,
        started: &mut Vec<Daemon>,
    ) -> Result<Self, String> {
        if let Some(why) = crate::discovery::untrusted_reason(&root) {
            return Err(why);
        }
        let attached = match state {
            home::PulseState::Up => true,
            home::PulseState::Stopped | home::PulseState::Starting | home::PulseState::Unknown => {
                false
            }
            home::PulseState::Foreign(who) => return Err(format!("port held by {who}")),
            home::PulseState::Unreachable => return Err("port answers, but not as a daemon".into()),
        };
        let client = Client::new(
            &config.server.host,
            config.server.port,
            config.security.secret.clone(),
        );
        if !attached && !started.iter().any(|d| d.dir == root) {
            tracing::info!("no daemon at {}; starting one in-process", client.base());
            let (stop, stop_rx) = tokio::sync::watch::channel(false);
            let cfg = config.clone();
            let dir = root.clone();
            let task = tokio::spawn(async move {
                crate::server::start_in(cfg, dir, Some(stop_rx))
                    .await
                    .map_err(|e| e.to_string())
            });
            started.push(Daemon {
                dir: root,
                stop,
                task,
            });
        }
        let (ctl, bg, poller) = poller::spawn(client.clone(), attached);
        Ok(Self {
            client,
            ctl,
            bg,
            poller,
            attached,
        })
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

/// Leave TUI mode, run `f` with the terminal cooked (the init wizard
/// prompts through it), then come back. The event stream is recreated by
/// the caller so nothing reads stdin while `f` does.
async fn suspend_for<F: std::future::Future>(term: &mut Term, f: F) -> F::Output {
    leave_terminal(term);
    let out = f.await;
    *term = enter_terminal();
    let _ = term.terminal.clear();
    out
}

/// `pulse-null init --dir <target>` as a child on the cooked terminal. A
/// child, not the in-process wizard: the prompt library answers Ctrl+c by
/// raising SIGINT at its own process, which would take the TUI (and any
/// daemon it started) down with it; in a child it ends only the wizard.
async fn run_wizard(target: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(target).map_err(|e| e.to_string())?;
    // /proc/self/exe keeps working after the binary was replaced in place
    // (a deploy); `current_exe()` would then end in " (deleted)".
    let exe = std::path::Path::new("/proc/self/exe");
    let exe = if exe.exists() {
        exe.to_path_buf()
    } else {
        std::env::current_exe().map_err(|e| e.to_string())?
    };
    let mut dir_arg = std::ffi::OsString::from("--dir=");
    dir_arg.push(target);
    let status = tokio::process::Command::new(exe)
        .arg("init")
        .arg(dir_arg)
        .status()
        .await
        .map_err(|e| format!("could not start the wizard: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(match status.code() {
            Some(code) => format!("exit {code}"),
            None => "cancelled".to_string(),
        })
    }
}

/// Where `Create a new pulse` puts it: the discovered pulse home, else
/// `~/pulse-null`.
/// `(cwd, $HOME)` as the scan and the create target see them.
fn where_we_are() -> (std::path::PathBuf, Option<std::path::PathBuf>) {
    (
        std::env::current_dir().unwrap_or_default(),
        std::env::var_os("HOME").map(std::path::PathBuf::from),
    )
}

/// Where `Create a new pulse` puts it: a directory that already holds this
/// user's pulses (the resolved pulse home when it has pulse children),
/// else `~/pulse-null` — never a bare cwd, which the scan would not find
/// again from anywhere else.
fn create_target(cwd: &std::path::Path, home: Option<&std::path::Path>) -> std::path::PathBuf {
    if let Some(pulse_home) = crate::discovery::resolve_pulse_home(cwd, home) {
        if crate::discovery::has_pulse_children(&pulse_home) {
            return pulse_home;
        }
    }
    home.map_or_else(|| cwd.to_path_buf(), |h| h.join("pulse-null"))
}

fn rescan_home() -> Vec<home::PulseRow> {
    let (cwd, home_dir) = where_we_are();
    home::Home::scan(&cwd, home_dir.as_deref())
}

async fn event_loop(
    term: &mut Term,
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
        tokio::sync::mpsc::channel::<Vec<(std::path::PathBuf, home::PulseState)>>(4);
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
    // Set by the Create row; handled after the select so the event stream
    // can be dropped and rebuilt around the wizard.
    let mut create = false;

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
            term.terminal.draw(|f| app.render(f))?;
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
                                let Some(config) = app.home.rows[i].config().cloned() else {
                                    continue;
                                };
                                let root = app.home.rows[i].dir.clone();
                                let state = app.home.rows[i].state.clone();
                                // A previous pick's poller and turn go; its daemon stays.
                                drop(session.take());
                                if let Some(task) = turn_task.take() {
                                    task.abort();
                                }
                                turn_rx = None;
                                match Session::open(&config, root.clone(), state, started) {
                                    Ok(s) => {
                                        attached = s.attached;
                                        attach_deadline = Instant::now() + ATTACH_TIMEOUT;
                                        daemon_base = Some(s.client.base().to_string());
                                        app.enter_pulse(&config);
                                        app.screen = app::Screen::Home;
                                        if attached {
                                            app.attached();
                                        }
                                        session = Some(s);
                                    }
                                    Err(why) => {
                                        app.home.daemon_ended(&root, &why);
                                    }
                                }
                            }
                            Action::Home => {
                                // The poller and any turn go with the session; the
                                // daemon this process started stays for Exit.
                                drop(session.take());
                                if let Some(task) = turn_task.take() {
                                    task.abort();
                                }
                                turn_rx = None;
                                attached = false;
                                app.start_home(rescan_home());
                            }
                            Action::Create => create = true,
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
            _ = boot_tick.tick(), if app.screen == app::Screen::Boot
                || (app.screen == app::Screen::Home
                    && (app.motion.level() != MotionLevel::Off
                        || app.home.rows.iter().any(|r| r.state == home::PulseState::Starting))) => {
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
                // A daemon we started that already died: say so on its row.
                for (dir, why) in reap_daemons(started).await {
                    tracing::warn!("daemon for {} ended: {why}", dir.display());
                    app.home.daemon_ended(&dir, &why);
                    dirty = true;
                }
                // One task probes every row; the loop never awaits the network.
                let targets: Vec<(std::path::PathBuf, String, Client)> = app
                    .home
                    .rows
                    .iter()
                    .filter_map(|r| r.client().map(|c| (r.dir.clone(), r.name.clone(), c)))
                    .collect();
                let tx = states_tx.clone();
                tokio::spawn(async move {
                    let mut set = tokio::task::JoinSet::new();
                    for (dir, name, c) in targets {
                        set.spawn(async move {
                            (dir, home::PulseState::from(c.probe_detail(&name).await))
                        });
                    }
                    let mut states = Vec::new();
                    while let Some(r) = set.join_next().await {
                        if let Ok(s) = r {
                            states.push(s);
                        }
                    }
                    let _ = tx.try_send(states);
                });
            }
            Some(states) = states_rx.recv() => {
                app.home.apply_states(&states);
                dirty |= app.screen == app::Screen::Home;
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

        if create {
            create = false;
            // dialoguer reads the cooked terminal; our reader must be gone.
            drop(events);
            let (cwd, home_dir) = where_we_are();
            let target = create_target(&cwd, home_dir.as_deref());
            let result = suspend_for(term, run_wizard(&target)).await;
            events = EventStream::new();
            app.start_home(rescan_home());
            app.home.notice = Some(match result {
                Ok(()) => "pulse created — pick it to start".to_string(),
                Err(e) => format!("the wizard did not finish: {e}"),
            });
            dirty = true;
            continue;
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

#[cfg(test)]
mod create_target_tests {
    use super::create_target;

    fn pulse_at(dir: &std::path::Path, name: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("pulse-null.toml"), "").unwrap();
    }

    #[test]
    fn create_goes_where_pulses_already_live_else_home_pulse_null() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        // Nothing anywhere: ~/pulse-null, not the cwd.
        assert_eq!(
            create_target(cwd.path(), Some(home.path())),
            home.path().join("pulse-null")
        );
        // Pulses under ~/pulse-null: there.
        pulse_at(&home.path().join("pulse-null"), "echo");
        assert_eq!(
            create_target(cwd.path(), Some(home.path())),
            home.path().join("pulse-null")
        );
        // Pulses as children of the cwd: the cwd is the pulse home.
        let flat = tempfile::tempdir().unwrap();
        pulse_at(flat.path(), "nova");
        assert_eq!(
            create_target(flat.path(), Some(home.path())),
            flat.path().to_path_buf()
        );
        // Inside a pulse: still ~/pulse-null, never inside the pulse.
        assert_eq!(
            create_target(&flat.path().join("nova/memory"), Some(home.path())),
            home.path().join("pulse-null")
        );
    }
}
