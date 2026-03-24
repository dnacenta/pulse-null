use std::collections::VecDeque;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use ratatui::Frame;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;
use tui_textarea::TextArea;

use pulse_system_types::llm::{Message, MessageContent, Role};

use crate::config::PeerConfig;
use crate::peer::{self, PeerClient};
use crate::streaming::StreamingProvider;
use crate::tui::app::AppContext;
use crate::tui::screens::{EntityState, ScreenAction};
use crate::tui::tabs::entity::push_bordered_section;
use crate::tui::theme::*;
use crate::tui::widgets::pulse::{self, PulseColorTransition};

use super::TabView;

// ─── Types ───

#[derive(Clone, Debug)]
pub enum CommsState {
    Setup,
    Connecting,
    Active {
        #[allow(dead_code)]
        waiting_for: String,
    },
    Finished,
    PeerMgmt,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CommsMode {
    Topic,
    Free,
}

#[derive(Clone, Debug)]
pub struct PeerEntry {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub online: Option<bool>,
    pub latency_ms: Option<u64>,
}

pub struct CommsMessage {
    pub entity: String,
    pub text: String,
    #[allow(dead_code)]
    pub turn: u32,
}

pub enum CommsEvent {
    PeersLoaded(Vec<PeerEntry>),
    MessageReceived {
        entity: String,
        text: String,
        turn: u32,
    },
    PeerActivity(EntityState),
    Error(String),
    Finished,
    TestResult(bool, Option<u64>),
}

#[derive(Clone, Debug, PartialEq)]
enum MgmtView {
    List,
    Form,
    DeleteConfirm,
}

#[derive(Clone, Copy, PartialEq)]
enum FormField {
    Name,
    Host,
    Port,
    Secret,
}

// ─── CommsTab ───

pub struct CommsTab {
    entity_name: String,
    state: CommsState,

    // Setup screen
    peers: Vec<PeerEntry>,
    selected_peer: usize,
    mode: CommsMode,
    topic: String,
    topic_editing: bool,

    // Conversation screen
    transcript: Vec<CommsMessage>,
    scroll: u16,
    auto_scroll: bool,
    turn_count: u32,
    max_turns: u32,
    paused: bool,
    pause_tx: watch::Sender<bool>,

    // Peer pulse (remote entity heartbeat)
    peer_pulse_data: VecDeque<f64>,
    peer_pulse_tick: u64,
    peer_pulse_state: EntityState,
    peer_pulse_color: PulseColorTransition,
    peer_name_active: String,

    // Peer management
    mgmt_view: MgmtView,
    mgmt_selected: usize,
    form_name: TextArea<'static>,
    form_host: TextArea<'static>,
    form_port: TextArea<'static>,
    form_secret: TextArea<'static>,
    form_focus: FormField,
    form_editing: Option<String>,
    form_error: Option<String>,
    form_test: Option<(bool, Option<u64>)>,
    delete_target: Option<String>,

    // Async
    task_handle: Option<JoinHandle<()>>,
    event_tx: mpsc::UnboundedSender<CommsEvent>,
    event_rx: mpsc::UnboundedReceiver<CommsEvent>,
    health_checked: bool,
}

impl CommsTab {
    pub fn new(entity_name: &str) -> Self {
        let term_width = crossterm::terminal::size()
            .map(|(w, _)| w as usize * 2)
            .unwrap_or(240);
        let mut peer_pulse_data = VecDeque::with_capacity(term_width);
        for _ in 0..term_width {
            peer_pulse_data.push_back(0.0);
        }

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (pause_tx, _) = watch::channel(true);

        Self {
            entity_name: entity_name.to_string(),
            state: CommsState::Setup,
            peers: Vec::new(),
            selected_peer: 0,
            mode: CommsMode::Topic,
            topic: String::new(),
            topic_editing: false,
            transcript: Vec::new(),
            scroll: 0,
            auto_scroll: true,
            turn_count: 0,
            max_turns: 20,
            paused: false,
            pause_tx,
            peer_pulse_data,
            peer_pulse_tick: 0,
            peer_pulse_state: EntityState::Idle,
            peer_pulse_color: PulseColorTransition::new(&EntityState::Idle),
            peer_name_active: String::new(),
            mgmt_view: MgmtView::List,
            mgmt_selected: 0,
            form_name: new_form_field("name"),
            form_host: new_form_field("host"),
            form_port: new_form_field("port"),
            form_secret: new_form_field("secret (optional)"),
            form_focus: FormField::Name,
            form_editing: None,
            form_error: None,
            form_test: None,
            delete_target: None,
            task_handle: None,
            event_tx,
            event_rx,
            health_checked: false,
        }
    }

    // ─── Event Handling ───

    fn drain_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: CommsEvent) {
        match event {
            CommsEvent::PeersLoaded(entries) => {
                self.peers = entries;
                self.peers.sort_by(|a, b| a.name.cmp(&b.name));
                if self.selected_peer >= self.peers.len() {
                    self.selected_peer = 0;
                }
            }
            CommsEvent::MessageReceived { entity, text, turn } => {
                self.turn_count = turn;
                // Transition from Connecting to Active on first message
                if matches!(self.state, CommsState::Connecting) {
                    self.state = CommsState::Active {
                        waiting_for: entity.clone(),
                    };
                } else if let CommsState::Active { .. } = &self.state {
                    self.state = CommsState::Active {
                        waiting_for: entity.clone(),
                    };
                }
                self.transcript.push(CommsMessage { entity, text, turn });
            }
            CommsEvent::PeerActivity(new_state) => {
                if new_state != self.peer_pulse_state {
                    self.peer_pulse_color.transition_to(&new_state);
                    self.peer_pulse_state = new_state;
                }
            }
            CommsEvent::Error(msg) => {
                self.transcript.push(CommsMessage {
                    entity: "system".to_string(),
                    text: format!("[error] {}", msg),
                    turn: self.turn_count,
                });
                self.peer_pulse_color.transition_to(&EntityState::Idle);
                self.peer_pulse_state = EntityState::Idle;
                self.task_handle = None;
            }
            CommsEvent::Finished => {
                self.state = CommsState::Finished;
                self.peer_pulse_color.transition_to(&EntityState::Idle);
                self.peer_pulse_state = EntityState::Idle;
                self.task_handle = None;
            }
            CommsEvent::TestResult(online, latency) => {
                self.form_test = Some((online, latency));
            }
        }
    }

    // ─── Peer Loading ───

    fn load_peers_from_config(&mut self, ctx: &AppContext) {
        if let Some(config) = &ctx.config {
            self.peers = config
                .peers
                .iter()
                .map(|(name, pc)| PeerEntry {
                    name: name.clone(),
                    host: pc.host.clone(),
                    port: pc.port,
                    online: None,
                    latency_ms: None,
                })
                .collect();
            self.peers.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }

    fn refresh_health(&self, ctx: &AppContext) {
        let Some(peer_client) = &ctx.peer_client else {
            return;
        };
        let client = Arc::clone(peer_client);
        let tx = self.event_tx.clone();

        tokio::spawn(async move {
            let client = client.lock().await;
            let statuses = client.list_peers().await;
            let entries: Vec<PeerEntry> = statuses
                .into_iter()
                .map(|s| PeerEntry {
                    name: s.name,
                    host: s.host,
                    port: s.port,
                    online: Some(s.online),
                    latency_ms: s.latency_ms,
                })
                .collect();
            let _ = tx.send(CommsEvent::PeersLoaded(entries));
        });
    }

    // ─── Conversation Control ───

    fn start_conversation(&mut self, ctx: &AppContext) {
        let Some(peer) = self.peers.get(self.selected_peer) else {
            return;
        };
        if peer.online != Some(true) {
            return;
        }

        let Some(provider) = &ctx.provider else {
            return;
        };
        let Some(peer_client) = &ctx.peer_client else {
            return;
        };
        let Some(system_prompt) = &ctx.system_prompt else {
            return;
        };

        self.peer_name_active = peer.name.clone();
        self.transcript.clear();
        self.scroll = 0;
        self.auto_scroll = true;
        self.turn_count = 0;
        self.paused = false;
        let _ = self.pause_tx.send(true);
        self.peer_pulse_state = EntityState::Idle;
        self.peer_pulse_color = PulseColorTransition::new(&EntityState::Idle);

        let provider = Arc::clone(provider);
        let peer_client = Arc::clone(peer_client);
        let system_prompt = system_prompt.clone();
        let peer_name = peer.name.clone();
        let local_name = self.entity_name.clone();
        let mode = self.mode.clone();
        let topic = if self.mode == CommsMode::Topic && !self.topic.is_empty() {
            Some(self.topic.clone())
        } else {
            None
        };
        let max_turns = self.max_turns;
        let max_tokens = ctx
            .config
            .as_ref()
            .map(|c| c.llm.max_tokens)
            .unwrap_or(4096);
        let tx = self.event_tx.clone();
        let pause_rx = self.pause_tx.subscribe();

        self.state = CommsState::Connecting;

        let handle = tokio::spawn(async move {
            run_conversation(
                provider,
                system_prompt,
                peer_client,
                peer_name,
                local_name,
                mode,
                topic,
                max_turns,
                max_tokens,
                tx,
                pause_rx,
            )
            .await;
        });
        self.task_handle = Some(handle);
    }

    fn cancel_conversation(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }
        self.state = CommsState::Setup;
        self.peer_pulse_color.transition_to(&EntityState::Idle);
        self.peer_pulse_state = EntityState::Idle;
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        let _ = self.pause_tx.send(!self.paused);
    }

    // ─── Peer Management Helpers ───

    fn open_add_form(&mut self) {
        self.form_editing = None;
        self.form_name = new_form_field("name");
        self.form_host = new_form_field("host");
        self.form_port = new_form_field("port");
        self.form_secret = new_form_field("secret (optional)");
        // Default port
        self.form_port = new_form_field("port");
        self.form_port.insert_str("3100");
        self.form_focus = FormField::Name;
        self.form_error = None;
        self.form_test = None;
        self.mgmt_view = MgmtView::Form;
    }

    fn open_edit_form(&mut self) {
        let Some(peer) = self.peers.get(self.mgmt_selected) else {
            return;
        };
        self.form_editing = Some(peer.name.clone());
        self.form_name = new_form_field("name (read-only)");
        self.form_name.insert_str(&peer.name);
        self.form_host = new_form_field("host");
        self.form_host.insert_str(&peer.host);
        self.form_port = new_form_field("port");
        self.form_port.insert_str(peer.port.to_string());
        self.form_secret = new_form_field("secret (optional)");
        self.form_focus = FormField::Host; // Skip name (read-only in edit)
        self.form_error = None;
        self.form_test = None;
        self.mgmt_view = MgmtView::Form;
    }

    fn save_form(&mut self, ctx: &mut AppContext) -> bool {
        let name = self.form_name.lines().join("").trim().to_string();
        let host = self.form_host.lines().join("").trim().to_string();
        let port_str = self.form_port.lines().join("").trim().to_string();
        let secret = self.form_secret.lines().join("").trim().to_string();

        // Validate
        if name.is_empty() {
            self.form_error = Some("Name cannot be empty".to_string());
            return false;
        }
        if host.is_empty() {
            self.form_error = Some("Host cannot be empty".to_string());
            return false;
        }
        let port: u16 = match port_str.parse() {
            Ok(p) if p > 0 => p,
            _ => {
                self.form_error = Some("Port must be 1-65535".to_string());
                return false;
            }
        };

        let config = PeerConfig {
            host,
            port,
            secret: if secret.is_empty() {
                None
            } else {
                Some(secret)
            },
        };

        let Some(peer_client) = &ctx.peer_client else {
            return false;
        };

        // Use try_lock since we're in sync context
        let mut client = match peer_client.try_lock() {
            Ok(c) => c,
            Err(_) => {
                self.form_error = Some("Peer client busy".to_string());
                return false;
            }
        };

        let result = if self.form_editing.is_some() {
            client.update_peer(&name, config)
        } else {
            client.add_peer(name.clone(), config)
        };

        match result {
            Ok(()) => {
                // Persist to TOML
                if let Some(config_path) = &ctx.config_path {
                    if let Err(e) = peer::save_peers_to_config(config_path, client.peers_map()) {
                        self.form_error = Some(format!("Save failed: {}", e));
                        return false;
                    }
                }
                // Reload peers
                drop(client);
                self.reload_peers_from_client(ctx);
                self.mgmt_view = MgmtView::List;
                true
            }
            Err(e) => {
                self.form_error = Some(format!("{}", e));
                false
            }
        }
    }

    fn delete_peer(&mut self, ctx: &mut AppContext) -> bool {
        let Some(name) = &self.delete_target else {
            return false;
        };
        let name = name.clone();

        let Some(peer_client) = &ctx.peer_client else {
            return false;
        };

        let mut client = match peer_client.try_lock() {
            Ok(c) => c,
            Err(_) => return false,
        };

        if client.remove_peer(&name).is_err() {
            return false;
        }

        if let Some(config_path) = &ctx.config_path {
            let _ = peer::save_peers_to_config(config_path, client.peers_map());
        }

        drop(client);
        self.reload_peers_from_client(ctx);
        self.delete_target = None;
        self.mgmt_view = MgmtView::List;
        true
    }

    fn reload_peers_from_client(&mut self, ctx: &AppContext) {
        let Some(peer_client) = &ctx.peer_client else {
            return;
        };
        let client = match peer_client.try_lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        self.peers = client
            .peers_map()
            .iter()
            .map(|(name, pc)| PeerEntry {
                name: name.clone(),
                host: pc.host.clone(),
                port: pc.port,
                online: None,
                latency_ms: None,
            })
            .collect();
        self.peers.sort_by(|a, b| a.name.cmp(&b.name));
        if self.mgmt_selected >= self.peers.len() {
            self.mgmt_selected = self.peers.len().saturating_sub(1);
        }
    }

    fn test_form_connection(&self) {
        let host = self.form_host.lines().join("").trim().to_string();
        let port_str = self.form_port.lines().join("").trim().to_string();
        let port: u16 = match port_str.parse() {
            Ok(p) if p > 0 => p,
            _ => return,
        };
        if host.is_empty() {
            return;
        }
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let (online, latency) = peer::test_connection(&host, port).await;
            let _ = tx.send(CommsEvent::TestResult(online, latency));
        });
    }

    // ─── Rendering ───

    fn render_setup(&self, frame: &mut Frame, area: Rect, _ctx: &AppContext) {
        let outer = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" comms ", Style::default().fg(NORD7)));
        let outer_inner = outer.inner(area);
        frame.render_widget(outer, area);

        let cols = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(outer_inner);

        let inner_w = cols[0].width.saturating_sub(2) as usize;
        let mut lines: Vec<Line> = Vec::new();

        // ── Connect To ──
        let mut peer_lines: Vec<Line> = Vec::new();
        if self.peers.is_empty() {
            peer_lines.push(Line::styled(
                "  No peers configured",
                Style::default().fg(COLOR_DIM),
            ));
            peer_lines.push(Line::styled(
                "  Press p to add peers",
                Style::default().fg(COLOR_DIM),
            ));
        } else {
            for (i, p) in self.peers.iter().enumerate() {
                let sel = i == self.selected_peer;
                let marker = if sel { "\u{25b8}" } else { " " };
                let dot = match p.online {
                    Some(true) => Span::styled("\u{25cf}", Style::default().fg(NORD14)),
                    Some(false) => Span::styled("\u{25cb}", Style::default().fg(NORD11)),
                    None => Span::styled("?", Style::default().fg(COLOR_DIM)),
                };
                peer_lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {}", marker),
                        Style::default().fg(if sel { NORD8 } else { COLOR_DIM }),
                    ),
                    dot,
                    Span::styled(
                        format!(" {:12} :{}", p.name, p.port),
                        Style::default().fg(if sel { COLOR_TEXT_BRIGHT } else { COLOR_TEXT }),
                    ),
                ]));
            }
        }
        push_bordered_section(&mut lines, "connect to", NORD7, &peer_lines, inner_w);
        lines.push(Line::from(""));

        // ── Mode ──
        let tm = if self.mode == CommsMode::Topic {
            "\u{25b8}"
        } else {
            " "
        };
        let fm = if self.mode == CommsMode::Free {
            "\u{25b8}"
        } else {
            " "
        };
        let mode_lines = vec![
            Line::styled(
                format!("  {} Topic", tm),
                Style::default().fg(if self.mode == CommsMode::Topic {
                    COLOR_TEXT_BRIGHT
                } else {
                    COLOR_DIM
                }),
            ),
            Line::styled(
                format!("  {} Free", fm),
                Style::default().fg(if self.mode == CommsMode::Free {
                    COLOR_TEXT_BRIGHT
                } else {
                    COLOR_DIM
                }),
            ),
        ];
        push_bordered_section(&mut lines, "mode", NORD8, &mode_lines, inner_w);

        // ── Topic (if topic mode) ──
        if self.mode == CommsMode::Topic {
            lines.push(Line::from(""));
            let display = if self.topic.is_empty() {
                "...".to_string()
            } else {
                self.topic.clone()
            };
            let title_color = if self.topic_editing { NORD8 } else { NORD13 };
            let text_color = if self.topic.is_empty() {
                COLOR_DIM
            } else {
                COLOR_TEXT
            };
            let topic_lines = vec![Line::styled(
                format!("  {}", display),
                Style::default().fg(text_color),
            )];
            let suffix = if self.topic_editing { " (editing)" } else { "" };
            push_bordered_section(
                &mut lines,
                &format!("topic{}", suffix),
                title_color,
                &topic_lines,
                inner_w,
            );
        }

        lines.push(Line::from(""));

        // ── Start hint ──
        let can_start = !self.peers.is_empty()
            && self
                .peers
                .get(self.selected_peer)
                .is_some_and(|p| p.online == Some(true));
        let start_style = if can_start {
            Style::default().fg(NORD14).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(COLOR_DIM)
        };
        lines.push(Line::styled("  Enter to start session", start_style));

        let left = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(left, cols[0]);

        // ── Right side: entity names ──
        let right = cols[1];
        if right.height > 4 && right.width > 10 {
            let center_y = right.y + right.height / 2;
            let local = &self.entity_name;
            let remote = self
                .peers
                .get(self.selected_peer)
                .map(|p| p.name.as_str())
                .unwrap_or("\u{2014}");

            let conn_line = Line::from(vec![
                Span::styled(
                    local.to_string(),
                    Style::default().fg(NORD8).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  \u{2194}  ", Style::default().fg(COLOR_DIM)),
                Span::styled(
                    remote.to_string(),
                    Style::default().fg(NORD15).add_modifier(Modifier::BOLD),
                ),
            ]);
            let text_len: usize = conn_line.spans.iter().map(|s| s.content.len()).sum();
            let pad_x = right.width.saturating_sub(text_len as u16) / 2;

            let label_area = Rect::new(right.x + pad_x, center_y, right.width - pad_x, 1);
            frame.render_widget(Paragraph::new(conn_line), label_area);
        }
    }

    fn render_conversation(&self, frame: &mut Frame, area: Rect) {
        // Layout: transcript (fill) + peer pulse (5 lines)
        let chunks = Layout::vertical([Constraint::Min(6), Constraint::Length(5)]).split(area);

        // ── Transcript ──
        let mode_label = match &self.mode {
            CommsMode::Topic => {
                if self.topic.is_empty() {
                    "topic".to_string()
                } else {
                    self.topic.clone()
                }
            }
            CommsMode::Free => "free".to_string(),
        };
        let status = if matches!(self.state, CommsState::Finished) {
            "complete"
        } else if self.paused {
            "paused"
        } else {
            &mode_label
        };

        let title = format!(
            " comms \u{2014} {} \u{2194} {} \u{2014} turn {}/{} \u{2014} {} ",
            self.entity_name, self.peer_name_active, self.turn_count, self.max_turns, status,
        );

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(title, Style::default().fg(NORD7)));
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);

        let inner_w = inner.width.saturating_sub(2) as usize;
        let mut lines: Vec<Line> = Vec::new();

        for msg in &self.transcript {
            let is_local = msg.entity == self.entity_name;
            let is_system = msg.entity == "system";
            let title_color = if is_system {
                NORD11
            } else if is_local {
                NORD8
            } else {
                NORD15
            };
            let content: Vec<Line> = msg
                .text
                .lines()
                .map(|l| {
                    Line::styled(
                        format!("  {}", l),
                        Style::default().fg(if is_system { NORD11 } else { COLOR_TEXT }),
                    )
                })
                .collect();
            push_bordered_section(&mut lines, &msg.entity, title_color, &content, inner_w);
            lines.push(Line::from(""));
        }

        // Completion banner
        if matches!(self.state, CommsState::Finished) {
            let banner = "conversation complete";
            let pad = inner_w.saturating_sub(banner.len() + 4) / 2;
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(pad)),
                Span::styled(
                    format!("\u{2500}\u{2500} {} \u{2500}\u{2500}", banner),
                    Style::default().fg(NORD14),
                ),
            ]));
        }

        // Connecting/waiting indicator
        if matches!(self.state, CommsState::Connecting) {
            lines.push(Line::styled(
                "  connecting...",
                Style::default().fg(COLOR_DIM),
            ));
        }

        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let content_height = paragraph.line_count(inner.width) as u16;
        let view_height = inner.height;
        let max_scroll = content_height.saturating_sub(view_height);
        let scroll = if self.auto_scroll {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        let paragraph = paragraph.scroll((scroll, 0));
        frame.render_widget(paragraph, inner);

        // ── Peer Pulse ──
        pulse::draw(
            frame,
            chunks[1],
            &self.peer_pulse_data,
            &self.peer_pulse_state,
            &self.peer_name_active,
            &self.peer_pulse_color,
        );
    }

    fn render_mgmt_list(&self, frame: &mut Frame, area: Rect) {
        let outer = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(
                " peer management ",
                Style::default().fg(NORD7),
            ));
        let outer_inner = outer.inner(area);
        frame.render_widget(outer, area);

        let inner_w = outer_inner.width.saturating_sub(2) as usize;
        let mut lines: Vec<Line> = Vec::new();

        // ── Phone Book ──
        let mut table_lines: Vec<Line> = Vec::new();
        // Header
        table_lines.push(Line::from(vec![Span::styled(
            format!(
                "  {:3} {:14} {:20} {:6} {}",
                "", "Name", "Host", "Port", "Status"
            ),
            Style::default().fg(COLOR_DIM).add_modifier(Modifier::BOLD),
        )]));
        table_lines.push(Line::styled(
            format!("  {}", "\u{2500}".repeat(inner_w.saturating_sub(4))),
            Style::default().fg(COLOR_BORDER),
        ));

        if self.peers.is_empty() {
            table_lines.push(Line::styled(
                "  No peers configured. Press a to add.",
                Style::default().fg(COLOR_DIM),
            ));
        } else {
            for (i, p) in self.peers.iter().enumerate() {
                let sel = i == self.mgmt_selected;
                let marker = if sel { " \u{25b8}" } else { "  " };
                let status = match p.online {
                    Some(true) => {
                        let lat = p
                            .latency_ms
                            .map(|ms| format!(" ({}ms)", ms))
                            .unwrap_or_default();
                        format!("\u{25cf} online{}", lat)
                    }
                    Some(false) => "\u{25cb} offline".to_string(),
                    None => "? unknown".to_string(),
                };
                let status_color = match p.online {
                    Some(true) => NORD14,
                    Some(false) => NORD11,
                    None => COLOR_DIM,
                };
                table_lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} {:14} {:20} {:6}", marker, p.name, p.host, p.port),
                        Style::default().fg(if sel { COLOR_TEXT_BRIGHT } else { COLOR_TEXT }),
                    ),
                    Span::styled(status, Style::default().fg(status_color)),
                ]));
            }
        }
        push_bordered_section(&mut lines, "phone book", NORD7, &table_lines, inner_w);

        // ── Details ──
        if let Some(peer) = self.peers.get(self.mgmt_selected) {
            lines.push(Line::from(""));
            let secret_display = if peer.online.is_some() {
                "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}".to_string()
            } else {
                "\u{2014}".to_string()
            };
            let status_text = match peer.online {
                Some(true) => {
                    let lat = peer
                        .latency_ms
                        .map(|ms| format!(" ({}ms)", ms))
                        .unwrap_or_default();
                    format!("\u{25cf} online{}", lat)
                }
                Some(false) => "\u{25cb} offline".to_string(),
                None => "? not checked".to_string(),
            };
            let detail_lines = vec![
                Line::from(vec![
                    Span::styled("  Name:     ", Style::default().fg(COLOR_DIM)),
                    Span::styled(
                        peer.name.clone(),
                        Style::default().fg(NORD7).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Host:     ", Style::default().fg(COLOR_DIM)),
                    Span::styled(peer.host.clone(), Style::default().fg(COLOR_TEXT)),
                ]),
                Line::from(vec![
                    Span::styled("  Port:     ", Style::default().fg(COLOR_DIM)),
                    Span::styled(peer.port.to_string(), Style::default().fg(COLOR_TEXT)),
                ]),
                Line::from(vec![
                    Span::styled("  Secret:   ", Style::default().fg(COLOR_DIM)),
                    Span::styled(secret_display, Style::default().fg(COLOR_DIM)),
                ]),
                Line::from(vec![
                    Span::styled("  Status:   ", Style::default().fg(COLOR_DIM)),
                    Span::styled(
                        status_text,
                        Style::default().fg(match peer.online {
                            Some(true) => NORD14,
                            Some(false) => NORD11,
                            None => COLOR_DIM,
                        }),
                    ),
                ]),
            ];
            push_bordered_section(&mut lines, "details", NORD9, &detail_lines, inner_w);
        }

        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, outer_inner);
    }

    fn render_mgmt_form(&self, frame: &mut Frame, area: Rect) {
        let outer = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(
                if self.form_editing.is_some() {
                    " peer management \u{2014} edit peer "
                } else {
                    " peer management \u{2014} add peer "
                },
                Style::default().fg(NORD7),
            ));
        let outer_inner = outer.inner(area);
        frame.render_widget(outer, area);

        // Center the form
        let form_width = 50u16.min(outer_inner.width.saturating_sub(4));
        let form_x = outer_inner.x + (outer_inner.width.saturating_sub(form_width)) / 2;

        // Calculate how many rows we need: 4 fields * 3 lines each + test + error + buttons
        let field_height = 3u16;
        let total_height = field_height * 4 + 4; // 4 fields + extras
        let form_y = outer_inner.y + outer_inner.height.saturating_sub(total_height).min(2);

        let mut y = form_y;

        // Name field
        let name_area = Rect::new(form_x, y, form_width, field_height);
        let name_style = if self.form_focus == FormField::Name {
            Style::default().fg(NORD8)
        } else {
            Style::default().fg(COLOR_BORDER)
        };
        let mut name_ta = self.form_name.clone();
        name_ta.set_block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(name_style)
                .title(Span::styled(
                    " name ",
                    Style::default().fg(if self.form_focus == FormField::Name {
                        NORD8
                    } else {
                        COLOR_DIM
                    }),
                )),
        );
        frame.render_widget(&name_ta, name_area);
        y += field_height;

        // Host field
        let host_area = Rect::new(form_x, y, form_width, field_height);
        let host_style = if self.form_focus == FormField::Host {
            Style::default().fg(NORD8)
        } else {
            Style::default().fg(COLOR_BORDER)
        };
        let mut host_ta = self.form_host.clone();
        host_ta.set_block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(host_style)
                .title(Span::styled(
                    " host ",
                    Style::default().fg(if self.form_focus == FormField::Host {
                        NORD8
                    } else {
                        COLOR_DIM
                    }),
                )),
        );
        frame.render_widget(&host_ta, host_area);
        y += field_height;

        // Port field
        let port_area = Rect::new(form_x, y, form_width, field_height);
        let port_style = if self.form_focus == FormField::Port {
            Style::default().fg(NORD8)
        } else {
            Style::default().fg(COLOR_BORDER)
        };
        let mut port_ta = self.form_port.clone();
        port_ta.set_block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(port_style)
                .title(Span::styled(
                    " port ",
                    Style::default().fg(if self.form_focus == FormField::Port {
                        NORD8
                    } else {
                        COLOR_DIM
                    }),
                )),
        );
        frame.render_widget(&port_ta, port_area);
        y += field_height;

        // Secret field
        let secret_area = Rect::new(form_x, y, form_width, field_height);
        let secret_style = if self.form_focus == FormField::Secret {
            Style::default().fg(NORD8)
        } else {
            Style::default().fg(COLOR_BORDER)
        };
        let mut secret_ta = self.form_secret.clone();
        secret_ta.set_block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(secret_style)
                .title(Span::styled(
                    " secret (optional) ",
                    Style::default().fg(if self.form_focus == FormField::Secret {
                        NORD8
                    } else {
                        COLOR_DIM
                    }),
                )),
        );
        frame.render_widget(&secret_ta, secret_area);
        y += field_height;

        // Test connection result
        if let Some((online, latency)) = &self.form_test {
            let test_text = if *online {
                let lat = latency.map(|ms| format!(" ({}ms)", ms)).unwrap_or_default();
                format!("\u{25cf} online{}", lat)
            } else {
                "\u{25cb} unreachable".to_string()
            };
            let test_color = if *online { NORD14 } else { NORD11 };
            let test_area = Rect::new(form_x, y, form_width, 1);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("  {}", test_text),
                    Style::default().fg(test_color),
                )),
                test_area,
            );
            y += 1;
        }

        // Error message
        if let Some(err) = &self.form_error {
            let err_area = Rect::new(form_x, y, form_width, 1);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("  {}", err),
                    Style::default().fg(NORD11),
                )),
                err_area,
            );
        }
    }

    fn render_delete_confirm(&self, frame: &mut Frame, area: Rect) {
        // Render the list behind
        self.render_mgmt_list(frame, area);

        // Popup overlay
        let width = 44u16.min(area.width.saturating_sub(4));
        let height = 9u16.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let popup = Rect::new(x, y, width, height);

        frame.render_widget(Clear, popup);

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(NORD11))
            .title(Span::styled(
                " delete peer ",
                Style::default().fg(NORD11).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let name = self.delete_target.as_deref().unwrap_or("?");
        let peer_info = self
            .peers
            .iter()
            .find(|p| p.name == name)
            .map(|p| format!("{}:{}", p.host, p.port))
            .unwrap_or_default();

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Remove ", Style::default().fg(COLOR_TEXT)),
                Span::styled(
                    format!("\"{}\"", name),
                    Style::default()
                        .fg(COLOR_TEXT_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" from phone book?", Style::default().fg(COLOR_TEXT)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Host:  ", Style::default().fg(COLOR_DIM)),
                Span::styled(peer_info, Style::default().fg(COLOR_TEXT)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Enter ", Style::default().fg(NORD0).bg(NORD11)),
                Span::styled(" Delete   ", Style::default().fg(COLOR_DIM)),
                Span::styled(" Esc ", Style::default().fg(NORD0).bg(NORD3)),
                Span::styled(" Cancel", Style::default().fg(COLOR_DIM)),
            ]),
        ];

        frame.render_widget(Paragraph::new(lines), inner);
    }

    // ─── Key Handling ───

    fn handle_key_setup(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction {
        // Topic editing mode
        if self.topic_editing {
            return self.handle_key_topic(key);
        }

        match key.code {
            KeyCode::Up => {
                if !self.peers.is_empty() {
                    if self.selected_peer == 0 {
                        self.selected_peer = self.peers.len() - 1;
                    } else {
                        self.selected_peer -= 1;
                    }
                }
            }
            KeyCode::Down => {
                if !self.peers.is_empty() {
                    self.selected_peer = (self.selected_peer + 1) % self.peers.len();
                }
            }
            KeyCode::Left | KeyCode::Right => {
                self.mode = match self.mode {
                    CommsMode::Topic => CommsMode::Free,
                    CommsMode::Free => CommsMode::Topic,
                };
            }
            KeyCode::Enter => {
                self.start_conversation(ctx);
            }
            KeyCode::Char('e') | KeyCode::Char('t') => {
                if self.mode == CommsMode::Topic {
                    self.topic_editing = true;
                }
            }
            KeyCode::Char('r') => {
                self.refresh_health(ctx);
            }
            KeyCode::Char('p') => {
                self.state = CommsState::PeerMgmt;
                self.mgmt_view = MgmtView::List;
                self.mgmt_selected = 0;
                // Trigger health refresh for mgmt screen
                self.refresh_health(ctx);
            }
            _ => {}
        }
        ScreenAction::None
    }

    fn handle_key_topic(&mut self, key: KeyEvent) -> ScreenAction {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.topic_editing = false;
            }
            KeyCode::Backspace => {
                self.topic.pop();
            }
            KeyCode::Char(c) => {
                self.topic.push(c);
            }
            _ => {}
        }
        ScreenAction::None
    }

    fn handle_key_conversation(&mut self, key: KeyEvent) -> ScreenAction {
        match key.code {
            KeyCode::Char(' ') => {
                if !matches!(self.state, CommsState::Finished) {
                    self.toggle_pause();
                }
            }
            KeyCode::Esc => {
                self.cancel_conversation();
            }
            KeyCode::Enter => {
                if matches!(self.state, CommsState::Finished) {
                    // Return to setup for a new conversation
                    self.state = CommsState::Setup;
                }
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll = self.scroll.saturating_add(3);
                self.auto_scroll = false;
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll = self.scroll.saturating_sub(3);
                if self.scroll == 0 {
                    self.auto_scroll = true;
                }
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10);
                self.auto_scroll = false;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                if self.scroll == 0 {
                    self.auto_scroll = true;
                }
            }
            _ => {}
        }
        ScreenAction::None
    }

    fn handle_key_mgmt(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction {
        match self.mgmt_view {
            MgmtView::List => self.handle_key_mgmt_list(key, ctx),
            MgmtView::Form => self.handle_key_mgmt_form(key, ctx),
            MgmtView::DeleteConfirm => self.handle_key_delete(key, ctx),
        }
    }

    fn handle_key_mgmt_list(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction {
        match key.code {
            KeyCode::Esc => {
                self.state = CommsState::Setup;
                // Reload peers in case they changed
                self.reload_peers_from_client(ctx);
            }
            KeyCode::Up => {
                if !self.peers.is_empty() {
                    if self.mgmt_selected == 0 {
                        self.mgmt_selected = self.peers.len() - 1;
                    } else {
                        self.mgmt_selected -= 1;
                    }
                }
            }
            KeyCode::Down => {
                if !self.peers.is_empty() {
                    self.mgmt_selected = (self.mgmt_selected + 1) % self.peers.len();
                }
            }
            KeyCode::Char('a') => {
                self.open_add_form();
            }
            KeyCode::Char('e') => {
                if !self.peers.is_empty() {
                    self.open_edit_form();
                }
            }
            KeyCode::Char('d') => {
                if let Some(peer) = self.peers.get(self.mgmt_selected) {
                    self.delete_target = Some(peer.name.clone());
                    self.mgmt_view = MgmtView::DeleteConfirm;
                }
            }
            KeyCode::Char('r') => {
                self.refresh_health(ctx);
            }
            _ => {}
        }
        ScreenAction::None
    }

    fn handle_key_mgmt_form(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction {
        match key.code {
            KeyCode::Esc => {
                self.mgmt_view = MgmtView::List;
            }
            KeyCode::Tab => {
                self.form_focus = match self.form_focus {
                    FormField::Name => FormField::Host,
                    FormField::Host => FormField::Port,
                    FormField::Port => FormField::Secret,
                    FormField::Secret => FormField::Name,
                };
            }
            KeyCode::BackTab => {
                self.form_focus = match self.form_focus {
                    FormField::Name => FormField::Secret,
                    FormField::Host => FormField::Name,
                    FormField::Port => FormField::Host,
                    FormField::Secret => FormField::Port,
                };
            }
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.test_form_connection();
            }
            KeyCode::Enter => {
                self.save_form(ctx);
            }
            _ => {
                // Delegate to focused field
                let is_edit = self.form_editing.is_some();
                match self.form_focus {
                    FormField::Name => {
                        if !is_edit {
                            self.form_name.input(key);
                        }
                    }
                    FormField::Host => {
                        self.form_host.input(key);
                    }
                    FormField::Port => {
                        self.form_port.input(key);
                    }
                    FormField::Secret => {
                        self.form_secret.input(key);
                    }
                }
            }
        }
        ScreenAction::None
    }

    fn handle_key_delete(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction {
        match key.code {
            KeyCode::Enter => {
                self.delete_peer(ctx);
            }
            KeyCode::Esc => {
                self.delete_target = None;
                self.mgmt_view = MgmtView::List;
            }
            _ => {}
        }
        ScreenAction::None
    }

    // ─── Tick ───

    fn tick_pulse(&mut self) {
        self.peer_pulse_tick += 1;
        let point = pulse::generate_pulse(&self.peer_pulse_state, self.peer_pulse_tick);

        let term_width = crossterm::terminal::size()
            .map(|(w, _)| w as usize * 2)
            .unwrap_or(240);
        while self.peer_pulse_data.len() < term_width {
            self.peer_pulse_data.push_front(0.0);
        }
        while self.peer_pulse_data.len() > term_width {
            self.peer_pulse_data.pop_front();
        }

        self.peer_pulse_data.pop_front();
        self.peer_pulse_data.push_back(point);
        self.peer_pulse_color.tick();
    }
}

// ─── TabView ───

impl TabView for CommsTab {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &AppContext) {
        match &self.state {
            CommsState::Setup => self.render_setup(frame, area, ctx),
            CommsState::Connecting | CommsState::Active { .. } | CommsState::Finished => {
                self.render_conversation(frame, area);
            }
            CommsState::PeerMgmt => match self.mgmt_view {
                MgmtView::List => self.render_mgmt_list(frame, area),
                MgmtView::Form => self.render_mgmt_form(frame, area),
                MgmtView::DeleteConfirm => self.render_delete_confirm(frame, area),
            },
        }
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction {
        match &self.state {
            CommsState::Setup => self.handle_key_setup(key, ctx),
            CommsState::Connecting | CommsState::Active { .. } | CommsState::Finished => {
                self.handle_key_conversation(key)
            }
            CommsState::PeerMgmt => self.handle_key_mgmt(key, ctx),
        }
    }

    fn handle_tick(&mut self, ctx: &mut AppContext) {
        // Load peers on first tick
        if !self.health_checked {
            self.load_peers_from_config(ctx);
            self.refresh_health(ctx);
            self.health_checked = true;
        }

        // Drain async events
        self.drain_events();

        // Tick peer pulse during conversations
        if matches!(
            self.state,
            CommsState::Active { .. } | CommsState::Connecting | CommsState::Finished
        ) {
            self.tick_pulse();
        }
    }
}

// ─── Async Conversation Loop ───

#[allow(clippy::too_many_arguments)]
async fn run_conversation(
    provider: Arc<dyn StreamingProvider>,
    system_prompt: String,
    peer_client: Arc<Mutex<PeerClient>>,
    peer_name: String,
    local_name: String,
    mode: CommsMode,
    topic: Option<String>,
    max_turns: u32,
    max_tokens: u32,
    tx: mpsc::UnboundedSender<CommsEvent>,
    mut pause_rx: watch::Receiver<bool>,
) {
    // Build opener prompt
    let opener = match mode {
        CommsMode::Topic => format!(
            "You are starting a conversation with {}. The topic is: {}. \
             Introduce yourself briefly and discuss the topic. \
             Keep your responses conversational \u{2014} 2-4 sentences.",
            peer_name,
            topic.unwrap_or_default(),
        ),
        CommsMode::Free => format!(
            "You are starting a conversation with {}. \
             Talk about whatever interests you. \
             Keep your responses conversational \u{2014} 2-4 sentences.",
            peer_name,
        ),
    };

    let mut conversation: Vec<Message> = Vec::new();
    let mut turn = 0u32;

    // First: local entity (A) responds to opener
    conversation.push(Message {
        role: Role::User,
        content: MessageContent::Text(opener),
    });

    let a_response = provider
        .invoke(&system_prompt, &conversation, max_tokens, None)
        .await;

    let a_text = match a_response {
        Ok(resp) => resp.text(),
        Err(e) => {
            let _ = tx.send(CommsEvent::Error(e.to_string()));
            return;
        }
    };

    turn += 1;
    conversation.push(Message {
        role: Role::Assistant,
        content: MessageContent::Text(a_text.clone()),
    });
    let _ = tx.send(CommsEvent::MessageReceived {
        entity: local_name.clone(),
        text: a_text.clone(),
        turn,
    });

    let mut last_message = a_text;

    // Main conversation loop
    while turn < max_turns {
        // Check pause
        if !*pause_rx.borrow() {
            if pause_rx.changed().await.is_err() {
                break;
            }
            if !*pause_rx.borrow() {
                break;
            }
        }

        // Send A's message to B (peer) via HTTP
        let _ = tx.send(CommsEvent::PeerActivity(EntityState::Thinking));

        let peer_response = {
            let client = peer_client.lock().await;
            client
                .send_message(&peer_name, &last_message, &local_name, "comms")
                .await
        };

        match peer_response {
            Ok(b_resp) => {
                let _ = tx.send(CommsEvent::PeerActivity(EntityState::Streaming));
                turn += 1;
                let _ = tx.send(CommsEvent::MessageReceived {
                    entity: peer_name.clone(),
                    text: b_resp.response.clone(),
                    turn,
                });

                // Brief visual delay for streaming effect
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                let _ = tx.send(CommsEvent::PeerActivity(EntityState::Idle));

                if turn >= max_turns {
                    break;
                }

                // Check pause again
                if !*pause_rx.borrow() {
                    if pause_rx.changed().await.is_err() {
                        break;
                    }
                    if !*pause_rx.borrow() {
                        break;
                    }
                }

                // Send B's response to A (local provider)
                conversation.push(Message {
                    role: Role::User,
                    content: MessageContent::Text(format!(
                        "[{} says]: {}",
                        peer_name, b_resp.response
                    )),
                });

                let a_response = provider
                    .invoke(&system_prompt, &conversation, max_tokens, None)
                    .await;

                match a_response {
                    Ok(resp) => {
                        let text = resp.text();
                        turn += 1;
                        conversation.push(Message {
                            role: Role::Assistant,
                            content: MessageContent::Text(text.clone()),
                        });
                        let _ = tx.send(CommsEvent::MessageReceived {
                            entity: local_name.clone(),
                            text: text.clone(),
                            turn,
                        });
                        last_message = text;
                    }
                    Err(e) => {
                        let _ = tx.send(CommsEvent::Error(e.to_string()));
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(CommsEvent::Error(format!("{}", e)));
                return;
            }
        }
    }

    let _ = tx.send(CommsEvent::PeerActivity(EntityState::Idle));
    let _ = tx.send(CommsEvent::Finished);
}

// ─── Helpers ───

fn new_form_field(label: &str) -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_placeholder_text("...");
    ta.set_placeholder_style(Style::default().fg(COLOR_DIM));
    ta.set_cursor_line_style(Style::default());
    ta.set_style(Style::default().fg(COLOR_TEXT));
    ta.set_block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(
                format!(" {} ", label),
                Style::default().fg(COLOR_DIM),
            )),
    );
    ta
}
