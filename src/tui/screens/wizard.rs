use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::Frame;
use tui_textarea::TextArea;

use super::{Screen, ScreenAction};
use crate::tui::app::AppContext;
use crate::tui::theme::*;

// ─── Wizard Steps ───

#[derive(Clone, Debug, PartialEq)]
enum WizardStep {
    Identity,
    Values,
    Provider,
    Server,
    Plugins,
    Review,
    Creating,
    Done,
}

impl WizardStep {
    fn index(&self) -> usize {
        match self {
            WizardStep::Identity => 0,
            WizardStep::Values => 1,
            WizardStep::Provider => 2,
            WizardStep::Server => 3,
            WizardStep::Plugins => 4,
            WizardStep::Review => 5,
            WizardStep::Creating => 6,
            WizardStep::Done => 7,
        }
    }

    fn label(&self) -> &str {
        match self {
            WizardStep::Identity => "Identity",
            WizardStep::Values => "Values & Traits",
            WizardStep::Provider => "LLM Provider",
            WizardStep::Server => "Server",
            WizardStep::Plugins => "Plugins",
            WizardStep::Review => "Review",
            WizardStep::Creating => "Creating",
            WizardStep::Done => "Done",
        }
    }

    const TOTAL: usize = 6; // steps the user interacts with (not Creating/Done)
}

// ─── Wizard State ───

pub struct WizardScreen {
    step: WizardStep,
    target_dir: PathBuf,

    // Identity
    entity_name: TextArea<'static>,
    owner_name: TextArea<'static>,
    owner_alias: TextArea<'static>,
    identity_field: usize, // 0=entity, 1=owner, 2=alias

    // Values
    values: TextArea<'static>,
    traits: TextArea<'static>,
    values_field: usize, // 0=values, 1=traits

    // Provider
    provider_idx: usize, // 0=claude-code, 1=claude, 2=ollama
    model: TextArea<'static>,
    api_key: TextArea<'static>,
    provider_field: usize, // 0=provider select, 1=model/api_key

    // Server
    port: TextArea<'static>,
    timezone_idx: usize,
    server_field: usize, // 0=port, 1=timezone

    // Plugins
    available_plugins: Vec<(String, String, bool)>, // (name, description, selected)
    plugin_cursor: usize,

    // Status
    error_msg: Option<String>,
    created_dir: Option<PathBuf>,
}

impl WizardScreen {
    pub fn new(target_dir: &Path) -> Self {
        let mut entity_name = TextArea::default();
        style_input(&mut entity_name, "Entity name");
        let mut owner_name = TextArea::default();
        style_input(&mut owner_name, "Your name");
        let mut owner_alias = TextArea::default();
        style_input(&mut owner_alias, "How should the entity address you?");

        let mut values = TextArea::default();
        style_input(&mut values, "Core values (one per line)");
        let mut traits = TextArea::default();
        style_input(&mut traits, "Personality traits (one per line)");

        let mut model = TextArea::default();
        style_input(&mut model, "Model");
        model.insert_str("opus");
        let mut api_key = TextArea::default();
        style_input(&mut api_key, "API key");

        let mut port = TextArea::default();
        style_input(&mut port, "Port");
        port.insert_str("3100");

        // Load available plugins
        let available_plugins: Vec<(String, String, bool)> =
            crate::plugins::registry::known_plugins()
                .into_iter()
                .filter(|p| p.available)
                .map(|p| (p.name, p.description, false))
                .collect();

        Self {
            step: WizardStep::Identity,
            target_dir: target_dir.to_path_buf(),
            entity_name,
            owner_name,
            owner_alias,
            identity_field: 0,
            values,
            traits,
            values_field: 0,
            provider_idx: 0,
            model,
            api_key,
            provider_field: 0,
            port,
            timezone_idx: 0,
            server_field: 0,
            available_plugins,
            plugin_cursor: 0,
            error_msg: None,
            created_dir: None,
        }
    }

    fn active_textarea(&mut self) -> Option<&mut TextArea<'static>> {
        match self.step {
            WizardStep::Identity => match self.identity_field {
                0 => Some(&mut self.entity_name),
                1 => Some(&mut self.owner_name),
                2 => Some(&mut self.owner_alias),
                _ => None,
            },
            WizardStep::Values => match self.values_field {
                0 => Some(&mut self.values),
                1 => Some(&mut self.traits),
                _ => None,
            },
            WizardStep::Provider => {
                if self.provider_field == 1 {
                    if self.provider_idx == 1 {
                        Some(&mut self.api_key)
                    } else {
                        Some(&mut self.model)
                    }
                } else {
                    None
                }
            }
            WizardStep::Server => {
                if self.server_field == 0 {
                    Some(&mut self.port)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn next_step(&mut self) {
        self.error_msg = None;
        match self.step {
            WizardStep::Identity => {
                let name = get_text(&self.entity_name);
                let owner = get_text(&self.owner_name);
                if name.is_empty() {
                    self.error_msg = Some("Entity name is required.".into());
                    return;
                }
                if owner.is_empty() {
                    self.error_msg = Some("Your name is required.".into());
                    return;
                }
                // Default alias to owner name
                if get_text(&self.owner_alias).is_empty() {
                    self.owner_alias.select_all();
                    self.owner_alias.cut();
                    self.owner_alias.insert_str(&owner);
                }
                self.step = WizardStep::Values;
            }
            WizardStep::Values => self.step = WizardStep::Provider,
            WizardStep::Provider => {
                // Set default model based on provider
                if self.provider_idx == 2 {
                    let m = get_text(&self.model);
                    if m.is_empty() || m == "opus" || m == "sonnet" || m == "haiku" {
                        self.model.select_all();
                        self.model.cut();
                        self.model.insert_str("llama3.2:latest");
                    }
                }
                self.step = WizardStep::Server;
            }
            WizardStep::Server => self.step = WizardStep::Plugins,
            WizardStep::Plugins => self.step = WizardStep::Review,
            WizardStep::Review => {
                self.step = WizardStep::Creating;
                self.create_entity();
            }
            WizardStep::Creating | WizardStep::Done => {}
        }
    }

    fn prev_step(&mut self) {
        self.error_msg = None;
        match self.step {
            WizardStep::Identity => {}
            WizardStep::Values => self.step = WizardStep::Identity,
            WizardStep::Provider => self.step = WizardStep::Values,
            WizardStep::Server => self.step = WizardStep::Provider,
            WizardStep::Plugins => self.step = WizardStep::Server,
            WizardStep::Review => self.step = WizardStep::Plugins,
            WizardStep::Creating | WizardStep::Done => {}
        }
    }

    fn create_entity(&mut self) {
        use crate::init::templates;

        let entity_name = get_text(&self.entity_name);
        let owner_name = get_text(&self.owner_name);
        let owner_alias = get_text(&self.owner_alias);

        let values: Vec<String> = get_text(&self.values)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let traits: Vec<String> = get_text(&self.traits)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        let provider = match self.provider_idx {
            0 => "claude-code",
            1 => "claude",
            2 => "ollama",
            _ => "claude-code",
        };

        let model = get_text(&self.model);
        let api_key = if self.provider_idx == 1 {
            let k = get_text(&self.api_key);
            if k.is_empty() {
                None
            } else {
                Some(k)
            }
        } else {
            None
        };

        let port: u16 = get_text(&self.port).parse().unwrap_or(3100);
        let timezone = TIMEZONES[self.timezone_idx.min(TIMEZONES.len() - 1)].to_string();

        let selected_plugins: Vec<(String, Vec<(String, String)>)> = self
            .available_plugins
            .iter()
            .filter(|(_, _, selected)| *selected)
            .map(|(name, _, _)| (name.clone(), Vec::new()))
            .collect();

        let entity_dir = self.target_dir.join(entity_name.to_lowercase());

        // Create directory structure
        let dirs = [
            "",
            "memory",
            "memory/logs",
            "journal",
            "monitoring",
            "archives",
            "archives/reflections",
            "archives/learning",
            "archives/curiosity",
            "archives/thoughts",
            "archives/praxis",
            "archives/conversations",
            "plugins",
            "logs",
        ];
        for d in &dirs {
            if let Err(e) = std::fs::create_dir_all(entity_dir.join(d)) {
                self.error_msg = Some(format!("Failed to create directories: {}", e));
                self.step = WizardStep::Review;
                return;
            }
        }

        let identity = templates::Identity {
            entity_name: entity_name.clone(),
            owner_name: owner_name.clone(),
            owner_alias: owner_alias.clone(),
            values,
            traits,
            morals: Vec::new(),
        };

        let config = templates::ConfigData {
            entity_name: entity_name.clone(),
            owner_name,
            owner_alias,
            provider: provider.to_string(),
            api_key,
            model,
            port,
            timezone,
            plugins: selected_plugins,
            rules_dir: None,
        };

        let files = vec![
            ("pulse-null.toml", templates::render_config(&config)),
            ("SELF.md", templates::render_self_md(&identity)),
            ("CLAUDE.md", templates::render_claude_md(&identity)),
            ("memory/MEMORY.md", templates::render_memory_md(&identity)),
            ("memory/EPHEMERAL.md", String::new()),
            ("memory/ARCHIVE.md", "# Archive Index\n".to_string()),
            (
                "journal/LEARNING.md",
                format!(
                    "# {} — Learning\n\nResearch journal. Raw notes.\n",
                    entity_name
                ),
            ),
            (
                "journal/THOUGHTS.md",
                format!("# {} — Thoughts\n\nIncubation space.\n", entity_name),
            ),
            (
                "journal/REFLECTIONS.md",
                format!(
                    "# {} — Reflections\n\nCrystallized observations.\n",
                    entity_name
                ),
            ),
            (
                "journal/CURIOSITY.md",
                format!(
                    "# {} — Curiosity\n\n## Open Questions\n\n## Themes\n\n## Explored\n",
                    entity_name
                ),
            ),
            (
                "journal/PRAXIS.md",
                format!("# {} — Praxis\n\nBehavioral policies.\n", entity_name),
            ),
            (
                "journal/LOGBOOK.md",
                format!("# {} — Logbook\n\nSession records.\n", entity_name),
            ),
            ("schedule.json", templates::render_schedule_json()),
            ("pipeline-state.json", "{}".to_string()),
            ("monitoring/signals.json", "[]".to_string()),
        ];

        for (path, content) in &files {
            if let Err(e) = std::fs::write(entity_dir.join(path), content) {
                self.error_msg = Some(format!("Failed to write {}: {}", path, e));
                self.step = WizardStep::Review;
                return;
            }
        }

        self.created_dir = Some(entity_dir);
        self.step = WizardStep::Done;
    }

    fn draw_progress(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![Span::raw("  ")];
        for i in 0..WizardStep::TOTAL {
            let step = match i {
                0 => WizardStep::Identity,
                1 => WizardStep::Values,
                2 => WizardStep::Provider,
                3 => WizardStep::Server,
                4 => WizardStep::Plugins,
                5 => WizardStep::Review,
                _ => WizardStep::Identity,
            };

            let current = self.step.index();
            let style = if i == current {
                Style::default().fg(NORD8).add_modifier(Modifier::BOLD)
            } else if i < current {
                Style::default().fg(NORD14)
            } else {
                Style::default().fg(COLOR_DIM)
            };

            let prefix = if i < current { "✓ " } else { "" };
            spans.push(Span::styled(format!("{}{}", prefix, step.label()), style));

            if i < WizardStep::TOTAL - 1 {
                spans.push(Span::styled(" → ", Style::default().fg(COLOR_DIM)));
            }
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

impl Screen for WizardScreen {
    fn render(&self, frame: &mut Frame, area: Rect, _ctx: &AppContext) {
        let chunks = Layout::vertical([
            Constraint::Length(3), // title
            Constraint::Length(2), // progress bar
            Constraint::Min(10),   // content
            Constraint::Length(2), // navigation hint + errors
        ])
        .split(area);

        // Title
        let title_block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(NORD8))
            .title(Span::styled(
                " pulse-null — entity wizard ",
                Style::default().fg(NORD8).add_modifier(Modifier::BOLD),
            ));
        frame.render_widget(title_block, chunks[0]);

        // Progress
        self.draw_progress(frame, chunks[1]);

        // Content
        let content_block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(
                format!(" {} ", self.step.label()),
                Style::default().fg(NORD7),
            ));
        let content_inner = content_block.inner(chunks[2]);
        frame.render_widget(content_block, chunks[2]);

        match self.step {
            WizardStep::Identity => self.draw_identity(frame, content_inner),
            WizardStep::Values => self.draw_values(frame, content_inner),
            WizardStep::Provider => self.draw_provider(frame, content_inner),
            WizardStep::Server => self.draw_server(frame, content_inner),
            WizardStep::Plugins => self.draw_plugins(frame, content_inner),
            WizardStep::Review => self.draw_review(frame, content_inner),
            WizardStep::Creating => self.draw_creating(frame, content_inner),
            WizardStep::Done => self.draw_done(frame, content_inner),
        }

        // Nav hints + error
        let mut hint_spans = vec![Span::styled("  ", Style::default())];
        match self.step {
            WizardStep::Done => {
                hint_spans.push(Span::styled(
                    "Enter: Start  |  Esc: Quit",
                    Style::default().fg(COLOR_DIM),
                ));
            }
            WizardStep::Creating => {}
            WizardStep::Identity => {
                hint_spans.push(Span::styled(
                    "Tab: Next field  |  Enter: Next step  |  Esc: Quit",
                    Style::default().fg(COLOR_DIM),
                ));
            }
            _ => {
                hint_spans.push(Span::styled(
                    "Tab: Next field  |  Enter: Next step  |  Backspace/Esc: Back",
                    Style::default().fg(COLOR_DIM),
                ));
            }
        }

        if let Some(ref err) = self.error_msg {
            hint_spans.push(Span::raw("  "));
            hint_spans.push(Span::styled(err.as_str(), Style::default().fg(COLOR_ERROR)));
        }

        frame.render_widget(Paragraph::new(Line::from(hint_spans)), chunks[3]);
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        // Global: Esc quits from identity or done
        if key.code == KeyCode::Esc {
            match self.step {
                WizardStep::Identity => return ScreenAction::Quit,
                WizardStep::Done => return ScreenAction::Quit,
                _ => {
                    self.prev_step();
                    return ScreenAction::None;
                }
            }
        }

        match self.step {
            WizardStep::Done => {
                if key.code == KeyCode::Enter && self.created_dir.is_some() {
                    return ScreenAction::SwitchTo(super::AppScreen::Main);
                }
                return ScreenAction::None;
            }
            WizardStep::Creating => return ScreenAction::None,
            _ => {}
        }

        // Enter: advance to next step
        if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
            // If in a selection field, Enter advances
            match self.step {
                WizardStep::Provider if self.provider_field == 0 => {
                    self.provider_field = 1;
                    // Set default model for selected provider
                    let default_model = match self.provider_idx {
                        0 => "opus",
                        1 => "claude-sonnet-4-20250514",
                        2 => "llama3.2:latest",
                        _ => "opus",
                    };
                    self.model.select_all();
                    self.model.cut();
                    self.model.insert_str(default_model);
                    return ScreenAction::None;
                }
                WizardStep::Plugins => {
                    self.next_step();
                    return ScreenAction::None;
                }
                _ => {
                    self.next_step();
                    return ScreenAction::None;
                }
            }
        }

        // Tab: cycle between fields within a step
        if key.code == KeyCode::Tab {
            match self.step {
                WizardStep::Identity => {
                    self.identity_field = (self.identity_field + 1) % 3;
                }
                WizardStep::Values => {
                    self.values_field = (self.values_field + 1) % 2;
                }
                WizardStep::Provider => {
                    if self.provider_field == 0 {
                        self.provider_field = 1;
                        let default_model = match self.provider_idx {
                            0 => "opus",
                            1 => "claude-sonnet-4-20250514",
                            2 => "llama3.2:latest",
                            _ => "opus",
                        };
                        self.model.select_all();
                        self.model.cut();
                        self.model.insert_str(default_model);
                    } else {
                        self.provider_field = 0;
                    }
                }
                WizardStep::Server => {
                    self.server_field = (self.server_field + 1) % 2;
                }
                _ => {}
            }
            return ScreenAction::None;
        }

        // Step-specific key handling
        match self.step {
            WizardStep::Provider if self.provider_field == 0 => {
                match key.code {
                    KeyCode::Up => {
                        self.provider_idx = self.provider_idx.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if self.provider_idx < 2 {
                            self.provider_idx += 1;
                        }
                    }
                    _ => {}
                }
                return ScreenAction::None;
            }
            WizardStep::Server if self.server_field == 1 => {
                match key.code {
                    KeyCode::Up => {
                        self.timezone_idx = self.timezone_idx.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if self.timezone_idx < TIMEZONES.len() - 1 {
                            self.timezone_idx += 1;
                        }
                    }
                    _ => {}
                }
                return ScreenAction::None;
            }
            WizardStep::Plugins => {
                match key.code {
                    KeyCode::Up => {
                        self.plugin_cursor = self.plugin_cursor.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if !self.available_plugins.is_empty()
                            && self.plugin_cursor < self.available_plugins.len() - 1
                        {
                            self.plugin_cursor += 1;
                        }
                    }
                    KeyCode::Char(' ') => {
                        if let Some(p) = self.available_plugins.get_mut(self.plugin_cursor) {
                            p.2 = !p.2;
                        }
                    }
                    _ => {}
                }
                return ScreenAction::None;
            }
            _ => {}
        }

        // Delegate to active textarea
        if let Some(ta) = self.active_textarea() {
            ta.input(key);
        }

        ScreenAction::None
    }

    fn handle_tick(&mut self, _ctx: &mut AppContext) {}
}

// ─── Draw Helpers ───

impl WizardScreen {
    fn draw_identity(&self, frame: &mut Frame, area: Rect) {
        let rows = Layout::vertical([
            Constraint::Length(1), // label
            Constraint::Length(3), // entity name
            Constraint::Length(1), // label
            Constraint::Length(3), // owner name
            Constraint::Length(1), // label
            Constraint::Length(3), // owner alias
            Constraint::Min(0),
        ])
        .split(area);

        let labels = [
            "Entity name:",
            "Your name:",
            "How should the entity address you?",
        ];
        let fields: [&TextArea; 3] = [&self.entity_name, &self.owner_name, &self.owner_alias];

        for (i, (label, field)) in labels.iter().zip(fields.iter()).enumerate() {
            let style = if i == self.identity_field {
                Style::default().fg(NORD8)
            } else {
                Style::default().fg(COLOR_DIM)
            };
            frame.render_widget(
                Paragraph::new(Line::styled(format!("  {}", label), style)),
                rows[i * 2],
            );

            let field_area = Rect {
                x: rows[i * 2 + 1].x + 2,
                width: rows[i * 2 + 1].width.saturating_sub(4),
                ..rows[i * 2 + 1]
            };
            frame.render_widget(*field, field_area);
        }
    }

    fn draw_values(&self, frame: &mut Frame, area: Rect) {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Percentage(45),
            Constraint::Length(1),
            Constraint::Percentage(45),
            Constraint::Min(0),
        ])
        .split(area);

        let labels = [
            "Core values (one per line):",
            "Personality traits (one per line):",
        ];
        let fields: [&TextArea; 2] = [&self.values, &self.traits];

        for (i, (label, field)) in labels.iter().zip(fields.iter()).enumerate() {
            let style = if i == self.values_field {
                Style::default().fg(NORD8)
            } else {
                Style::default().fg(COLOR_DIM)
            };
            frame.render_widget(
                Paragraph::new(Line::styled(format!("  {}", label), style)),
                rows[i * 2],
            );
            let field_area = Rect {
                x: rows[i * 2 + 1].x + 2,
                width: rows[i * 2 + 1].width.saturating_sub(4),
                ..rows[i * 2 + 1]
            };
            frame.render_widget(*field, field_area);
        }
    }

    fn draw_provider(&self, frame: &mut Frame, area: Rect) {
        let rows = Layout::vertical([
            Constraint::Length(1), // label
            Constraint::Length(5), // selection
            Constraint::Length(1), // spacer
            Constraint::Length(1), // model label
            Constraint::Length(3), // model input
            Constraint::Min(0),
        ])
        .split(area);

        let label_style = if self.provider_field == 0 {
            Style::default().fg(NORD8)
        } else {
            Style::default().fg(COLOR_DIM)
        };
        frame.render_widget(
            Paragraph::new(Line::styled("  Select LLM provider:", label_style)),
            rows[0],
        );

        let providers = [
            "Claude Code (uses claude CLI — no API key)",
            "Claude API (requires Anthropic API key)",
            "Ollama (local, requires Ollama running)",
        ];

        let lines: Vec<Line> = providers
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let marker = if i == self.provider_idx { "▸ " } else { "  " };
                let style = if i == self.provider_idx {
                    Style::default().fg(NORD8).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(COLOR_TEXT)
                };
                Line::styled(format!("    {}{}", marker, p), style)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rows[1]);

        // Model / API key input
        let model_label = match self.provider_idx {
            1 => "API Key / Model:",
            _ => "Model:",
        };
        let model_style = if self.provider_field == 1 {
            Style::default().fg(NORD8)
        } else {
            Style::default().fg(COLOR_DIM)
        };
        frame.render_widget(
            Paragraph::new(Line::styled(format!("  {}", model_label), model_style)),
            rows[3],
        );

        let input_area = Rect {
            x: rows[4].x + 2,
            width: rows[4].width.saturating_sub(4),
            ..rows[4]
        };
        if self.provider_idx == 1 && self.provider_field == 1 {
            frame.render_widget(&self.api_key, input_area);
        } else {
            frame.render_widget(&self.model, input_area);
        }
    }

    fn draw_server(&self, frame: &mut Frame, area: Rect) {
        let rows = Layout::vertical([
            Constraint::Length(1), // port label
            Constraint::Length(3), // port input
            Constraint::Length(1), // tz label
            Constraint::Length(8), // tz selection (visible window)
            Constraint::Min(0),
        ])
        .split(area);

        let port_style = if self.server_field == 0 {
            Style::default().fg(NORD8)
        } else {
            Style::default().fg(COLOR_DIM)
        };
        frame.render_widget(
            Paragraph::new(Line::styled("  Server port:", port_style)),
            rows[0],
        );
        let port_area = Rect {
            x: rows[1].x + 2,
            width: 20.min(rows[1].width.saturating_sub(4)),
            ..rows[1]
        };
        frame.render_widget(&self.port, port_area);

        let tz_style = if self.server_field == 1 {
            Style::default().fg(NORD8)
        } else {
            Style::default().fg(COLOR_DIM)
        };
        frame.render_widget(
            Paragraph::new(Line::styled("  Timezone:", tz_style)),
            rows[2],
        );

        // Show timezone list with scrolling window
        let visible = rows[3].height as usize;
        let start = self.timezone_idx.saturating_sub(visible / 2);
        let end = (start + visible).min(TIMEZONES.len());

        let lines: Vec<Line> = TIMEZONES[start..end]
            .iter()
            .enumerate()
            .map(|(i, tz)| {
                let actual_idx = start + i;
                let marker = if actual_idx == self.timezone_idx {
                    "▸ "
                } else {
                    "  "
                };
                let style = if actual_idx == self.timezone_idx {
                    Style::default().fg(NORD8).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(COLOR_TEXT)
                };
                Line::styled(format!("    {}{}", marker, tz), style)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rows[3]);
    }

    fn draw_plugins(&self, frame: &mut Frame, area: Rect) {
        if self.available_plugins.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "  No plugins available.",
                    Style::default().fg(COLOR_DIM),
                )),
                area,
            );
            return;
        }

        let mut lines = vec![Line::styled(
            "  Space to toggle, Enter to continue:",
            Style::default().fg(COLOR_DIM),
        )];
        lines.push(Line::from(""));

        for (i, (name, desc, selected)) in self.available_plugins.iter().enumerate() {
            let marker = if i == self.plugin_cursor {
                "▸ "
            } else {
                "  "
            };
            let check = if *selected { "[✓]" } else { "[ ]" };
            let style = if i == self.plugin_cursor {
                Style::default().fg(NORD8)
            } else {
                Style::default().fg(COLOR_TEXT)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("    {}{} ", marker, check), style),
                Span::styled(name.as_str(), style.add_modifier(Modifier::BOLD)),
                Span::styled(format!(" — {}", desc), Style::default().fg(COLOR_DIM)),
            ]));
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_review(&self, frame: &mut Frame, area: Rect) {
        let entity_name = get_text(&self.entity_name);
        let owner_name = get_text(&self.owner_name);
        let owner_alias = get_text(&self.owner_alias);
        let model = get_text(&self.model);
        let port = get_text(&self.port);

        let provider = match self.provider_idx {
            0 => "claude-code",
            1 => "claude",
            2 => "ollama",
            _ => "claude-code",
        };
        let timezone = TIMEZONES[self.timezone_idx.min(TIMEZONES.len() - 1)];

        let selected_plugins: Vec<&str> = self
            .available_plugins
            .iter()
            .filter(|(_, _, s)| *s)
            .map(|(n, _, _)| n.as_str())
            .collect();
        let plugins_str = if selected_plugins.is_empty() {
            "none".to_string()
        } else {
            selected_plugins.join(", ")
        };

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("    Entity:    ", Style::default().fg(COLOR_DIM)),
                Span::styled(
                    entity_name,
                    Style::default().fg(NORD7).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("    Owner:     ", Style::default().fg(COLOR_DIM)),
                Span::styled(owner_name, Style::default().fg(COLOR_TEXT)),
            ]),
            Line::from(vec![
                Span::styled("    Alias:     ", Style::default().fg(COLOR_DIM)),
                Span::styled(owner_alias, Style::default().fg(COLOR_TEXT)),
            ]),
            Line::from(vec![
                Span::styled("    Provider:  ", Style::default().fg(COLOR_DIM)),
                Span::styled(provider, Style::default().fg(COLOR_TEXT)),
            ]),
            Line::from(vec![
                Span::styled("    Model:     ", Style::default().fg(COLOR_DIM)),
                Span::styled(model, Style::default().fg(COLOR_TEXT)),
            ]),
            Line::from(vec![
                Span::styled("    Port:      ", Style::default().fg(COLOR_DIM)),
                Span::styled(port, Style::default().fg(COLOR_TEXT)),
            ]),
            Line::from(vec![
                Span::styled("    Timezone:  ", Style::default().fg(COLOR_DIM)),
                Span::styled(timezone, Style::default().fg(COLOR_TEXT)),
            ]),
            Line::from(vec![
                Span::styled("    Plugins:   ", Style::default().fg(COLOR_DIM)),
                Span::styled(plugins_str, Style::default().fg(COLOR_TEXT)),
            ]),
            Line::from(""),
            Line::styled(
                "    Press Enter to create entity, Esc to go back.",
                Style::default().fg(COLOR_DIM),
            ),
        ];

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn draw_creating(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "  Creating entity...",
                Style::default().fg(NORD8),
            )),
            area,
        );
    }

    fn draw_done(&self, frame: &mut Frame, area: Rect) {
        let entity_name = get_text(&self.entity_name);
        let dir_str = self
            .created_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_default();

        let lines = vec![
            Line::from(""),
            Line::styled(
                format!("  Entity \"{}\" created successfully!", entity_name),
                Style::default().fg(NORD14).add_modifier(Modifier::BOLD),
            ),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Directory: ", Style::default().fg(COLOR_DIM)),
                Span::styled(dir_str, Style::default().fg(COLOR_TEXT)),
            ]),
            Line::from(""),
            Line::styled(
                "  Press Enter to start, or Esc to quit.",
                Style::default().fg(COLOR_DIM),
            ),
        ];

        frame.render_widget(Paragraph::new(lines), area);
    }
}

// ─── Helpers ───

fn style_input(ta: &mut TextArea<'static>, _placeholder: &str) {
    ta.set_block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER)),
    );
    ta.set_cursor_line_style(Style::default());
    ta.set_style(Style::default().fg(COLOR_TEXT));
}

fn get_text(ta: &TextArea) -> String {
    ta.lines().join("\n").trim().to_string()
}

const TIMEZONES: &[&str] = &[
    "UTC",
    "US/Eastern",
    "US/Central",
    "US/Pacific",
    "Europe/London",
    "Europe/Madrid",
    "Europe/Berlin",
    "Europe/Paris",
    "Asia/Tokyo",
    "Asia/Shanghai",
    "Australia/Sydney",
];
