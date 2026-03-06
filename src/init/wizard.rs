use std::path::Path;
use std::sync::Arc;

use console::style;
use dialoguer::{Input, MultiSelect, Password, Select};

use super::templates;
use echo_system_types::llm::{LlmResponse, LlmResult, LmProvider, Message, StopReason};

pub async fn run(target_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    println!();
    println!("  {}", style("Welcome to pulse-null.").bold());
    println!("  Let's create your entity.");
    println!();

    // Entity name
    let entity_name: String = Input::new()
        .with_prompt("  What should your entity be called?")
        .interact_text()?;

    // Owner name
    let owner_name: String = Input::new()
        .with_prompt("  What's your name?")
        .interact_text()?;

    // Owner alias
    let owner_alias: String = Input::new()
        .with_prompt("  How should the entity address you?")
        .default(owner_name.clone())
        .interact_text()?;

    println!();
    println!("  {}", style("Let's set up your entity's identity.").bold());
    println!();

    // Core values
    println!("  Core values — what principles should guide your entity?");
    println!("  (Enter one per line, empty line to finish)");
    let values = read_multiline("  > ")?;

    // Personality traits
    println!("  Personality traits — how should your entity communicate?");
    println!("  (Enter one per line, empty line to finish)");
    let traits = read_multiline("  > ")?;

    // Moral framework
    println!("  Moral framework (optional) — any ethical grounding?");
    println!("  (Enter one per line, empty line to finish)");
    let morals = read_multiline("  > ")?;

    println!();

    // LLM provider
    let providers = vec![
        "Claude API (requires Anthropic API key)",
        "OpenAI API (requires API key)",
        "Ollama (local, requires Ollama running)",
    ];
    let provider_idx = Select::new()
        .with_prompt("  LLM provider")
        .items(&providers)
        .default(0)
        .interact()?;

    let (provider_name, api_key) = match provider_idx {
        0 => {
            let key: String = Input::new()
                .with_prompt("  Anthropic API key")
                .interact_text()?;
            ("claude".to_string(), Some(key))
        }
        1 => {
            let key: String = Input::new()
                .with_prompt("  OpenAI API key")
                .interact_text()?;
            ("openai".to_string(), Some(key))
        }
        2 => ("ollama".to_string(), None),
        _ => unreachable!(),
    };

    // Server port
    let port: u16 = Input::new()
        .with_prompt("  Server port")
        .default(3100)
        .interact_text()?;

    println!();
    println!("  {}", style("Scheduler configuration.").bold());
    println!();

    // Timezone
    let common_timezones = vec![
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
    let tz_idx = Select::new()
        .with_prompt("  Timezone for scheduled tasks")
        .items(&common_timezones)
        .default(0)
        .interact()?;

    let timezone = if tz_idx < common_timezones.len() {
        common_timezones[tz_idx].to_string()
    } else {
        // Fallback (shouldn't happen with Select)
        "UTC".to_string()
    };

    // Plugin selection
    let plugin_configs = collect_plugin_configs(target_dir).await?;

    println!();
    println!(
        "  Creating entity \"{}\"...",
        style(&entity_name).cyan().bold()
    );

    // Create directory structure
    let entity_dir = target_dir.join(entity_name.to_lowercase());
    create_directory_structure(&entity_dir)?;

    // Generate files
    let identity = templates::Identity {
        entity_name: entity_name.clone(),
        owner_name: owner_name.clone(),
        owner_alias: owner_alias.clone(),
        values: values.clone(),
        traits: traits.clone(),
        morals: morals.clone(),
    };

    let config = templates::ConfigData {
        entity_name: entity_name.clone(),
        owner_name: owner_name.clone(),
        owner_alias: owner_alias.clone(),
        provider: provider_name,
        api_key,
        port,
        timezone: timezone.clone(),
        plugins: plugin_configs,
    };

    // Write all files
    let files = vec![
        ("pulse-null.toml", templates::render_config(&config)),
        ("SELF.md", templates::render_self_md(&identity)),
        ("CLAUDE.md", templates::render_claude_md(&identity)),
        (
            "memory/MEMORY.md",
            templates::render_memory_md(&identity),
        ),
        ("memory/EPHEMERAL.md", String::new()),
        ("memory/ARCHIVE.md", "# Archive Index\n".to_string()),
        (
            "journal/LEARNING.md",
            format!("# {} — Learning\n\nResearch journal. Raw notes from reading, papers, concepts encountered.\n", entity_name),
        ),
        (
            "journal/THOUGHTS.md",
            format!("# {} — Thoughts\n\nIncubation space. Half-formed ideas that need multiple passes.\n", entity_name),
        ),
        (
            "journal/REFLECTIONS.md",
            format!("# {} — Reflections\n\nCrystallized observations, patterns, and lessons learned.\n", entity_name),
        ),
        (
            "journal/CURIOSITY.md",
            format!("# {} — Curiosity\n\n## Open Questions\n\n## Themes\n\n## Explored\n", entity_name),
        ),
        (
            "journal/PRAXIS.md",
            format!("# {} — Praxis\n\nActive behavioral policies derived from reflection.\n", entity_name),
        ),
        (
            "journal/LOGBOOK.md",
            format!("# {} — Logbook\n\nSession tracking and daily records.\n", entity_name),
        ),
        (
            "schedule.json",
            templates::render_schedule_json(),
        ),
        (
            "pipeline-state.json",
            "{}".to_string(),
        ),
        (
            "monitoring/signals.json",
            "[]".to_string(),
        ),
    ];

    for (path, content) in &files {
        let full_path = entity_dir.join(path);
        std::fs::write(&full_path, content)?;
        let display_path = path.rsplit('/').next().unwrap_or(path);
        println!(
            "    {} {} created",
            display_path,
            ".".repeat(30 - display_path.len().min(29))
        );
    }

    println!();
    println!(
        "  Entity \"{}\" is ready.",
        style(&entity_name).cyan().bold()
    );
    println!(
        "  Run {} to start.",
        style(format!(
            "cd {} && pulse-null up",
            entity_name.to_lowercase()
        ))
        .green()
    );
    println!(
        "  Manage plugins with: {}",
        style("pulse-null plugin add|remove <name>").green()
    );
    println!();

    Ok(())
}

fn create_directory_structure(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let dirs = vec![
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

    for d in dirs {
        std::fs::create_dir_all(dir.join(d))?;
    }

    Ok(())
}

/// Noop LLM provider — used only to construct PluginContext for setup prompts.
struct NoopProvider;
impl LmProvider for NoopProvider {
    fn invoke(
        &self,
        _: &str,
        _: &[Message],
        _: u32,
        _: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        Box::pin(async {
            Ok(LlmResponse {
                content: vec![],
                stop_reason: StopReason::EndTurn,
                model: "noop".into(),
                input_tokens: None,
                output_tokens: None,
            })
        })
    }
    fn name(&self) -> &str {
        "noop"
    }
}

type PluginConfigs = Vec<(String, Vec<(String, String)>)>;

async fn collect_plugin_configs(
    target_dir: &Path,
) -> Result<PluginConfigs, Box<dyn std::error::Error>> {
    use crate::plugins::registry;

    let available: Vec<_> = registry::known_plugins()
        .into_iter()
        .filter(|p| p.available)
        .collect();

    if available.is_empty() {
        return Ok(Vec::new());
    }

    println!();
    println!("  {}", style("Plugins (optional).").bold());
    println!();

    let labels: Vec<String> = available
        .iter()
        .map(|p| format!("{} — {}", p.name, p.description))
        .collect();

    let selected = MultiSelect::new()
        .with_prompt("  Select plugins to enable (space to toggle, enter to confirm)")
        .items(&labels)
        .interact()?;

    if selected.is_empty() {
        return Ok(Vec::new());
    }

    // Build a dummy context for fetching setup prompts
    let ctx = crate::plugins::PluginContext {
        entity_root: target_dir.to_path_buf(),
        entity_name: "init".into(),
        provider: Arc::new(NoopProvider) as Arc<dyn LmProvider>,
    };

    let mut plugin_configs = Vec::new();

    for &idx in &selected {
        let entry = &available[idx];

        let prompts = match registry::setup_prompts_for(&entry.name, &ctx).await {
            Some(p) => p,
            None => {
                // Plugin has no config or failed to construct — just enable it
                plugin_configs.push((entry.name.clone(), Vec::new()));
                continue;
            }
        };

        println!();
        println!("  Configuring {}...", style(&entry.name).cyan());

        let mut config_pairs = Vec::new();

        for prompt in &prompts {
            let value = if prompt.secret {
                if prompt.required {
                    Password::new()
                        .with_prompt(format!("  {}", prompt.question))
                        .interact()?
                } else {
                    Password::new()
                        .with_prompt(format!("  {} (optional)", prompt.question))
                        .allow_empty_password(true)
                        .interact()?
                }
            } else if let Some(ref default) = prompt.default {
                Input::<String>::new()
                    .with_prompt(format!("  {}", prompt.question))
                    .default(default.clone())
                    .allow_empty(!prompt.required)
                    .interact_text()?
            } else if prompt.required {
                Input::<String>::new()
                    .with_prompt(format!("  {}", prompt.question))
                    .interact_text()?
            } else {
                Input::<String>::new()
                    .with_prompt(format!("  {} (optional)", prompt.question))
                    .allow_empty(true)
                    .interact_text()?
            };

            // Skip empty optional values
            if !value.is_empty() {
                config_pairs.push((prompt.key.clone(), value));
            }
        }

        plugin_configs.push((entry.name.clone(), config_pairs));
    }

    Ok(plugin_configs)
}

fn read_multiline(prompt: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut lines = Vec::new();
    loop {
        let line: String = Input::new()
            .with_prompt(prompt)
            .allow_empty(true)
            .interact_text()?;
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }
    Ok(lines)
}
