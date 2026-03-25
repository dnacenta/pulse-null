pub mod app;
pub mod screens;
pub mod tabs;
pub mod theme;
pub mod widgets;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio_stream::StreamExt;

use crate::config::Config;
use crate::streaming::StreamingProvider;
use crate::tools::ToolRegistry;

use app::AppContext;
use screens::main_screen::MainScreen;
use screens::splash::SplashScreen;
use screens::wizard::WizardScreen;
use screens::{AppScreen, Screen, ScreenAction};

/// Launch the full TUI application (splash screen → main workspace).
pub async fn run(
    config: Option<&Config>,
    root_dir: Option<&Path>,
    provider: Option<Arc<dyn StreamingProvider>>,
    tools: Option<Arc<ToolRegistry>>,
    system_prompt: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Terminal setup
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        event::EnableBracketedPaste,
        event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Panic hook for terminal cleanup
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            event::DisableBracketedPaste,
            event::DisableMouseCapture
        );
        original_hook(info);
    }));

    // Build context
    let mut ctx = AppContext::new(
        config.cloned(),
        root_dir.map(|p| p.to_path_buf()),
        provider,
        tools,
        system_prompt.map(|s| s.to_string()),
    );

    let entity_available = config.is_some();
    let entity_name = config.map(|c| c.entity.name.as_str());
    let owner_alias = config.map(|c| c.entity.owner_alias.as_str());

    // Screens
    let mut current_screen = AppScreen::Splash;
    let mut splash = SplashScreen::new(entity_available, entity_name);
    let mut main_screen: Option<MainScreen> = None;
    let mut wizard_screen: Option<WizardScreen> = None;

    let mut events = event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        terminal.draw(|f| {
            let area = f.area();
            match current_screen {
                AppScreen::Splash => splash.render(f, area, &ctx),
                AppScreen::Wizard => {
                    if let Some(ref wiz) = wizard_screen {
                        wiz.render(f, area, &ctx);
                    }
                }
                AppScreen::Main => {
                    if let Some(ref main) = main_screen {
                        main.render(f, area, &ctx);
                    }
                }
            }
        })?;

        tokio::select! {
            Some(event) = StreamExt::next(&mut events) => {
                if let Ok(Event::Key(key)) = event {
                    let action = match current_screen {
                        AppScreen::Splash => splash.handle_key(key, &mut ctx),
                        AppScreen::Wizard => {
                            if let Some(ref mut wiz) = wizard_screen {
                                wiz.handle_key(key, &mut ctx)
                            } else {
                                ScreenAction::None
                            }
                        }
                        AppScreen::Main => {
                            if let Some(ref mut main) = main_screen {
                                main.handle_key(key, &mut ctx)
                            } else {
                                ScreenAction::None
                            }
                        }
                    };

                    match action {
                        ScreenAction::Quit => break,
                        ScreenAction::SwitchTo(screen) => {
                            if screen == AppScreen::Wizard && wizard_screen.is_none() {
                                let target = root_dir.unwrap_or(Path::new("."));
                                wizard_screen = Some(WizardScreen::new(target));
                            }
                            if screen == AppScreen::Main && main_screen.is_none() {
                                let name = entity_name.unwrap_or("entity");
                                let alias = owner_alias.unwrap_or("you");
                                main_screen = Some(MainScreen::new(name, alias));
                            }
                            current_screen = screen;
                        }
                        ScreenAction::None => {}
                    }
                } else if let Ok(Event::Paste(text)) = event {
                    if current_screen == AppScreen::Main {
                        if let Some(ref mut main) = main_screen {
                            if main.active_tab == crate::tui::tabs::Tab::Chat {
                                main.chat.insert_paste_text(&text);
                            }
                        }
                    }
                } else if let Ok(Event::Mouse(mouse)) = event {
                    if current_screen == AppScreen::Main {
                        if let Some(ref mut main) = main_screen {
                            handle_mouse(mouse, main);
                        }
                    }
                }
            }
            _ = tick.tick() => {
                match current_screen {
                    AppScreen::Splash => splash.handle_tick(&mut ctx),
                    AppScreen::Wizard => {
                        if let Some(ref mut wiz) = wizard_screen {
                            wiz.handle_tick(&mut ctx);
                        }
                    }
                    AppScreen::Main => {
                        if let Some(ref mut main) = main_screen {
                            main.handle_tick(&mut ctx);
                        }
                    }
                }
            }
        }
    }

    // Cleanup terminal
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        event::DisableBracketedPaste,
        event::DisableMouseCapture
    )?;

    // Restore panic hook
    let _ = std::panic::take_hook();

    // Archive session
    if let (Some(rd), Some(cfg)) = (root_dir, config) {
        if let Some(ref main) = main_screen {
            if let Some(archive_path) = crate::session::end_session(
                rd,
                &cfg.entity.name,
                &main.chat.conversation,
                "tui-v2",
                "session-end",
            ) {
                if cfg.graph.enabled && cfg.graph.auto_ingest {
                    let root = rd.to_path_buf();
                    tokio::task::spawn_blocking(move || {
                        let rt = match tokio::runtime::Runtime::new() {
                            Ok(rt) => rt,
                            Err(e) => {
                                tracing::warn!("graph ingest: failed to create runtime: {}", e);
                                return;
                            }
                        };
                        rt.block_on(async {
                            crate::session::graph_ingest_archive(&root, &archive_path, None).await;
                        });
                    });
                }
            }
        }
    }

    Ok(())
}

/// Launch directly into chat (skip splash). Used by `pulse-null chat`.
pub async fn run_chat(
    config: &Config,
    root_dir: &Path,
    provider: Arc<dyn StreamingProvider>,
    tools: Arc<ToolRegistry>,
    system_prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Terminal setup
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        event::EnableBracketedPaste,
        event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            event::DisableBracketedPaste,
            event::DisableMouseCapture
        );
        original_hook(info);
    }));

    let mut ctx = AppContext::new(
        Some(config.clone()),
        Some(root_dir.to_path_buf()),
        Some(provider),
        Some(tools),
        Some(system_prompt.to_string()),
    );

    let mut main = MainScreen::new(&config.entity.name, &config.entity.owner_alias);
    let mut events = event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        terminal.draw(|f| main.render(f, f.area(), &ctx))?;

        tokio::select! {
            Some(event) = StreamExt::next(&mut events) => {
                if let Ok(Event::Key(key)) = event {
                    match main.handle_key(key, &mut ctx) {
                        ScreenAction::Quit => break,
                        _ => {}
                    }
                } else if let Ok(Event::Paste(text)) = event {
                    if main.active_tab == crate::tui::tabs::Tab::Chat {
                        main.chat.insert_paste_text(&text);
                    }
                } else if let Ok(Event::Mouse(mouse)) = event {
                    handle_mouse(mouse, &mut main);
                }
            }
            _ = tick.tick() => {
                main.handle_tick(&mut ctx);
            }
        }
    }

    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        event::DisableBracketedPaste,
        event::DisableMouseCapture
    )?;
    let _ = std::panic::take_hook();

    if let Some(archive_path) = crate::session::end_session(
        root_dir,
        &config.entity.name,
        &main.chat.conversation,
        "tui-v2",
        "session-end",
    ) {
        if config.graph.enabled && config.graph.auto_ingest {
            let root = root_dir.to_path_buf();
            tokio::task::spawn_blocking(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!("graph ingest: failed to create runtime: {}", e);
                        return;
                    }
                };
                rt.block_on(async {
                    crate::session::graph_ingest_archive(&root, &archive_path, None).await;
                });
            });
        }
    }

    Ok(())
}

/// Handle mouse events for the main screen.
fn handle_mouse(mouse: crossterm::event::MouseEvent, main: &mut MainScreen) {
    match mouse.kind {
        MouseEventKind::ScrollUp => match main.active_tab {
            tabs::Tab::Chat => main.chat.scroll_down(3),
            tabs::Tab::Files => main.files.scroll_down(3),
            tabs::Tab::Evolution => main.evolution.scroll_down(3),
            tabs::Tab::Comms => main.comms.scroll_down(3),
            _ => {}
        },
        MouseEventKind::ScrollDown => match main.active_tab {
            tabs::Tab::Chat => main.chat.scroll_up(3),
            tabs::Tab::Files => main.files.scroll_up(3),
            tabs::Tab::Evolution => main.evolution.scroll_up(3),
            tabs::Tab::Comms => main.comms.scroll_up(3),
            _ => {}
        },
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            // Tab-specific click handling
            if main.active_tab == tabs::Tab::Comms && mouse.row > 10 {
                let term = crossterm::terminal::size().unwrap_or((80, 24));
                let content_area = Rect::new(0, 11, term.0, term.1.saturating_sub(12));
                main.comms
                    .handle_mouse(mouse.row, mouse.column, content_area);
            }
            // Tab bar click detection: header is 8 rows (0-7), tab bar is rows 8-10
            if mouse.row >= 8 && mouse.row <= 10 && !main.fullscreen {
                // Calculate tab positions dynamically based on label widths + separators
                // Format: " Label │ Label │ ..."  — each label is " {name} " + " │ "
                let mut col_start = 2u16; // initial " " padding + border
                let col = mouse.column;
                let mut clicked_tab = None;
                for i in 0..tabs::Tab::COUNT {
                    let tab = tabs::Tab::from_index(i);
                    let label_len = tab.label().len() as u16 + 2; // " Label "
                    let col_end = col_start + label_len;
                    if col >= col_start && col < col_end {
                        clicked_tab = Some(tab);
                        break;
                    }
                    col_start = col_end + 3; // " │ " separator
                }
                if let Some(t) = clicked_tab {
                    main.active_tab = t;
                }
            }
        }
        _ => {}
    }
}
