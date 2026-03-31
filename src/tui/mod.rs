pub mod app;
pub mod screens;
pub mod tabs;
pub mod theme;
pub mod widgets;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::RwLock;
use tokio_stream::StreamExt;

use crate::config::Config;
use crate::registry::EntityRegistry;
use crate::streaming::StreamingProvider;
use crate::tools::ToolRegistry;

use app::AppContext;
use screens::main_screen::MainScreen;
use screens::splash::SplashScreen;
use screens::welcome::WelcomeScreen;
use screens::wizard::WizardScreen;
use screens::{AppScreen, Screen, ScreenAction};

/// Launch the full TUI application (splash screen → main workspace).
/// Single-entity mode — unchanged from before.
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
        config.cloned(),
        root_dir.map(|p| p.to_path_buf()),
        provider,
        tools,
        system_prompt.map(|s| s.to_string()),
        None,
    );

    let entity_available = config.is_some();
    let entity_name = config.map(|c| c.entity.name.as_str());
    let owner_alias = config.map(|c| c.entity.owner_alias.as_str());

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
                AppScreen::Welcome => {} // not used in single-entity mode
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
                        AppScreen::Welcome => ScreenAction::None,
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
                        ScreenAction::SwitchToEntity(_) => {} // not used in single-entity mode
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
                    AppScreen::Welcome => {}
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
                None,
            ) {
                if cfg.graph.enabled && cfg.graph.auto_ingest {
                    let root = rd.to_path_buf();
                    let provider_clone = ctx.provider.clone();
                    tokio::task::spawn_blocking(move || {
                        let rt = match tokio::runtime::Runtime::new() {
                            Ok(rt) => rt,
                            Err(e) => {
                                tracing::warn!("graph ingest: failed to create runtime: {}", e);
                                return;
                            }
                        };
                        rt.block_on(async {
                            let provider_ref: Option<&dyn pulse_system_types::llm::LmProvider> =
                                provider_clone.as_ref().map(|p| {
                                    p.as_ref() as &dyn pulse_system_types::llm::LmProvider
                                });
                            crate::session::graph_ingest_archive(
                                &root,
                                &archive_path,
                                provider_ref,
                            )
                            .await;
                        });
                    });
                }
            }
        }
    }

    Ok(())
}

/// Launch the TUI in multi-entity mode.
pub async fn run_multi(
    registry: Arc<RwLock<EntityRegistry>>,
    entity_home: PathBuf,
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

    let mut ctx = AppContext::new_multi(Arc::clone(&registry), entity_home.clone());

    // Get initial entity list
    let entities = registry.read().await.list();

    let mut current_screen = AppScreen::Welcome;
    let mut welcome = WelcomeScreen::new(entities);
    let mut main_screen: Option<MainScreen> = None;
    let mut wizard_screen: Option<WizardScreen> = None;

    let mut events = event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        terminal.draw(|f| {
            let area = f.area();
            match current_screen {
                AppScreen::Welcome => welcome.render(f, area, &ctx),
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
                AppScreen::Splash => {} // not used in multi-entity mode
            }
        })?;

        tokio::select! {
            Some(event) = StreamExt::next(&mut events) => {
                if let Ok(Event::Key(key)) = event {
                    let action = match current_screen {
                        AppScreen::Welcome => welcome.handle_key(key, &mut ctx),
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
                        AppScreen::Splash => ScreenAction::None,
                    };

                    match action {
                        ScreenAction::Quit => {
                            if current_screen == AppScreen::Wizard {
                                // ESC from wizard goes back to welcome, not full quit
                                wizard_screen = None;
                                let entities = registry.read().await.list();
                                welcome.update_entities(entities);
                                current_screen = AppScreen::Welcome;
                            } else {
                                break;
                            }
                        }

                        ScreenAction::SwitchToEntity(name) => {
                            // Load entity context
                            let entity_info = registry.read().await.get(&name);
                            if let Some(info) = entity_info {
                                let config = match Config::load_from(&info.dir) {
                                    Ok(c) => c,
                                    Err(e) => {
                                        tracing::error!("Failed to load config for {}: {}", name, e);
                                        continue;
                                    }
                                };
                                if let Err(e) = ctx.load_entity(&config, &info.dir) {
                                    tracing::error!("Failed to load entity {}: {}", name, e);
                                    continue;
                                }
                                // Inject local peers from registry
                                inject_local_peers(&ctx, &registry, &name).await;

                                main_screen = Some(
                                    MainScreen::new(&config.entity.name, &config.entity.owner_alias)
                                        .with_multi_entity(true),
                                );
                                current_screen = AppScreen::Main;
                            }
                        }

                        ScreenAction::SwitchTo(screen) => {
                            match screen {
                                AppScreen::Welcome => {
                                    // Returning from MainScreen — archive session first
                                    archive_current_session(&ctx, &main_screen);
                                    ctx.unload_entity();
                                    main_screen = None;
                                    // Refresh entity list
                                    let entities = registry.read().await.list();
                                    welcome.update_entities(entities);
                                    current_screen = AppScreen::Welcome;
                                }
                                AppScreen::Wizard => {
                                    wizard_screen = Some(WizardScreen::new(&entity_home));
                                    current_screen = AppScreen::Wizard;
                                }
                                AppScreen::Main if current_screen == AppScreen::Wizard => {
                                    // Wizard completed — boot the new entity and auto-enter it
                                    if let Some(ref wiz) = wizard_screen {
                                        if let Some(ref created_dir) = wiz.created_dir {
                                            match boot_and_enter(
                                                &registry,
                                                created_dir,
                                                &mut ctx,
                                                &mut main_screen,
                                            ).await {
                                                Ok(_) => {
                                                    current_screen = AppScreen::Main;
                                                }
                                                Err(e) => {
                                                    tracing::error!("Failed to boot new entity: {}", e);
                                                    let entities = registry.read().await.list();
                                                    welcome.update_entities(entities);
                                                    current_screen = AppScreen::Welcome;
                                                }
                                            }
                                        }
                                    }
                                    wizard_screen = None;
                                }
                                _ => {
                                    current_screen = screen;
                                }
                            }
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
                    AppScreen::Welcome => welcome.handle_tick(&mut ctx),
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
                    AppScreen::Splash => {}
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
    let _ = std::panic::take_hook();

    // Archive current entity session if active
    archive_current_session(&ctx, &main_screen);

    Ok(())
}

/// Archive the current entity's chat session before switching.
/// Also triggers graph ingestion when enabled.
fn archive_current_session(ctx: &AppContext, main_screen: &Option<MainScreen>) {
    if let (Some(rd), Some(cfg), Some(ref main)) = (&ctx.root_dir, &ctx.config, main_screen) {
        if let Some(archive_path) = crate::session::end_session(
            rd,
            &cfg.entity.name,
            &main.chat.conversation,
            "tui-v2",
            "entity-switch",
            None,
        ) {
            if cfg.graph.enabled && cfg.graph.auto_ingest {
                let root = rd.to_path_buf();
                let provider_clone = ctx.provider.clone();
                tokio::task::spawn_blocking(move || {
                    let rt = match tokio::runtime::Runtime::new() {
                        Ok(rt) => rt,
                        Err(e) => {
                            tracing::warn!("graph ingest: failed to create runtime: {}", e);
                            return;
                        }
                    };
                    rt.block_on(async {
                        let provider_ref: Option<&dyn pulse_system_types::llm::LmProvider> =
                            provider_clone
                                .as_ref()
                                .map(|p| p.as_ref() as &dyn pulse_system_types::llm::LmProvider);
                        crate::session::graph_ingest_archive(&root, &archive_path, provider_ref)
                            .await;
                    });
                });
            }
        }
    }
}

/// Boot a newly created entity and enter its TUI.
async fn boot_and_enter(
    registry: &Arc<RwLock<EntityRegistry>>,
    created_dir: &Path,
    ctx: &mut AppContext,
    main_screen: &mut Option<MainScreen>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load_from(created_dir)?;
    let port = registry.write().await.next_port();

    let booted =
        crate::server::boot::boot_entity(config.clone(), created_dir.to_path_buf(), port).await?;

    let entity_name = config.entity.name.clone();
    let owner_alias = config.entity.owner_alias.clone();

    registry
        .write()
        .await
        .register(crate::registry::RunningEntity {
            name: entity_name.clone(),
            dir: created_dir.to_path_buf(),
            config: config.clone(),
            port: booted.actual_port,
            server_handle: booted.server_handle,
            scheduler_handles: booted.scheduler_handles,
            event_bus: booted.event_bus,
            persist_coordinator: booted.persist_coordinator,
        });

    ctx.load_entity(&config, created_dir)?;
    inject_local_peers(ctx, registry, &entity_name).await;
    *main_screen = Some(MainScreen::new(&entity_name, &owner_alias).with_multi_entity(true));

    tracing::info!(
        "Booted and entered entity \"{}\" on :{}",
        entity_name,
        booted.actual_port
    );
    Ok(())
}

/// Inject local entities as peers in the current entity's PeerClient.
async fn inject_local_peers(
    ctx: &AppContext,
    registry: &Arc<RwLock<EntityRegistry>>,
    current_entity: &str,
) {
    if let Some(ref peer_client) = ctx.peer_client {
        let entities = registry.read().await.list();
        let mut client = peer_client.lock().await;
        for entity in entities {
            if entity.name != current_entity {
                client.add_local_peer(entity.name, entity.port);
            }
        }
    }
}

/// Launch directly into chat (skip splash). Used by `pulse-null chat`.
pub async fn run_chat(
    config: &Config,
    root_dir: &Path,
    provider: Arc<dyn StreamingProvider>,
    tools: Arc<ToolRegistry>,
    system_prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
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

    let event_bus = Arc::new(crate::events::EventBus::new(64));

    let mut ctx = AppContext::new(
        Some(config.clone()),
        Some(root_dir.to_path_buf()),
        Some(provider),
        Some(tools),
        Some(system_prompt.to_string()),
        Some(Arc::clone(&event_bus)),
    );

    let mut main = MainScreen::new(&config.entity.name, &config.entity.owner_alias);
    let mut events = event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        terminal.draw(|f| main.render(f, f.area(), &ctx))?;

        tokio::select! {
            Some(event) = StreamExt::next(&mut events) => {
                if let Ok(Event::Key(key)) = event {
                    if let ScreenAction::Quit = main.handle_key(key, &mut ctx) { break }
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
        None,
    ) {
        if config.graph.enabled && config.graph.auto_ingest {
            let root = root_dir.to_path_buf();
            let provider_clone = ctx.provider.clone();
            tokio::task::spawn_blocking(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!("graph ingest: failed to create runtime: {}", e);
                        return;
                    }
                };
                rt.block_on(async {
                    let provider_ref: Option<&dyn pulse_system_types::llm::LmProvider> =
                        provider_clone
                            .as_ref()
                            .map(|p| p.as_ref() as &dyn pulse_system_types::llm::LmProvider);
                    crate::session::graph_ingest_archive(&root, &archive_path, provider_ref).await;
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
            if main.active_tab == tabs::Tab::Comms && mouse.row > 10 {
                let term = crossterm::terminal::size().unwrap_or((80, 24));
                let content_area = Rect::new(0, 11, term.0, term.1.saturating_sub(12));
                main.comms
                    .handle_mouse(mouse.row, mouse.column, content_area);
            }
            if mouse.row >= 8 && mouse.row <= 10 && !main.fullscreen {
                let mut col_start = 2u16;
                let col = mouse.column;
                let mut clicked_tab = None;
                for i in 0..tabs::Tab::COUNT {
                    let tab = tabs::Tab::from_index(i);
                    let label_len = tab.label().len() as u16 + 2;
                    let col_end = col_start + label_len;
                    if col >= col_start && col < col_end {
                        clicked_tab = Some(tab);
                        break;
                    }
                    col_start = col_end + 3;
                }
                if let Some(t) = clicked_tab {
                    main.active_tab = t;
                }
            }
        }
        _ => {}
    }
}
