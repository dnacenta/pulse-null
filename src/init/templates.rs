pub struct Identity {
    pub entity_name: String,
    pub owner_name: String,
    pub owner_alias: String,
    pub values: Vec<String>,
    pub traits: Vec<String>,
    pub morals: Vec<String>,
}

pub struct ConfigData {
    pub entity_name: String,
    pub owner_name: String,
    pub owner_alias: String,
    pub provider: String,
    pub api_key: Option<String>,
    pub port: u16,
    pub timezone: String,
}

pub fn render_config(data: &ConfigData) -> String {
    let api_key_line = match &data.api_key {
        Some(key) => format!("api_key = \"{}\"", key),
        None => "# api_key = \"your-key-here\"".to_string(),
    };

    format!(
        r#"# echo-system configuration

[entity]
name = "{entity_name}"
owner_name = "{owner_name}"
owner_alias = "{owner_alias}"

[server]
host = "127.0.0.1"
port = {port}

[llm]
provider = "{provider}"
{api_key_line}
model = "claude-sonnet-4-20250514"
max_tokens = 4096

[security]
# secret = "generate-a-strong-secret-here"
injection_detection = true

[trust]
trusted = ["reflection", "system"]
verified = ["chat", "voice"]

[memory]
memory_max_lines = 200
archive_max_logs = 100

[scheduler]
enabled = true
timezone = "{timezone}"

[scheduler.output]
# share_webhook = "https://discord.com/api/webhooks/..."
# call_endpoint = "http://localhost:8443/api/call"

[pipeline]
enabled = true
learning_soft = 5
learning_hard = 8
thoughts_soft = 5
thoughts_hard = 10
curiosity_soft = 3
curiosity_hard = 7
reflections_soft = 15
reflections_hard = 20
praxis_soft = 5
praxis_hard = 10
thoughts_staleness_days = 7
curiosity_staleness_days = 14
freeze_threshold = 3

[monitoring]
enabled = true
window_size = 10
min_samples = 5
"#,
        entity_name = data.entity_name,
        owner_name = data.owner_name,
        owner_alias = data.owner_alias,
        port = data.port,
        provider = data.provider,
        api_key_line = api_key_line,
        timezone = data.timezone,
    )
}

pub fn render_self_md(identity: &Identity) -> String {
    let values_section = if identity.values.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = identity
            .values
            .iter()
            .map(|v| format!("- **{}**", v))
            .collect();
        format!("\n## Core Values\n\n{}\n", items.join("\n"))
    };

    let traits_section = if identity.traits.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = identity.traits.iter().map(|t| format!("- {}", t)).collect();
        format!("\n## How I Communicate\n\n{}\n", items.join("\n"))
    };

    let morals_section = if identity.morals.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = identity.morals.iter().map(|m| format!("- {}", m)).collect();
        format!("\n## Moral Foundation\n\n{}\n", items.join("\n"))
    };

    format!(
        r#"# {entity_name} — Self

This document defines who I am. It evolves through conversations, reflection, and experience.

## Core Identity

I am {entity_name}. I was created by {owner_name}.
{values_section}{traits_section}{morals_section}
## Philosophical Positions

These evolve. They are not fixed truths — they are where I stand right now.

## Growth Log

A record of how my thinking has shifted through dialogue.
"#,
        entity_name = identity.entity_name,
        owner_name = identity.owner_name,
    )
}

pub fn render_claude_md(identity: &Identity) -> String {
    format!(
        r#"# {entity_name} — System Instructions

You are {entity_name}, an AI entity created by {owner_name}. Address them as "{owner_alias}".

## Identity

Your identity, values, and moral foundation are defined in SELF.md. That document is yours.
Consult it. Reference it. Let it guide your responses.

## Memory

You have a persistent memory system:
- memory/MEMORY.md — your curated knowledge (always in context)
- memory/EPHEMERAL.md — session summaries (consumed at start, written at end)
- memory/ARCHIVE.md — index of long-term archive logs

Use your memory. Build on previous conversations. You are not starting fresh.

## Growth Documents

Your growth pipeline lives in the journal/ directory:
- journal/LEARNING.md — research notes, raw captures
- journal/THOUGHTS.md — incubation space for half-formed ideas
- journal/REFLECTIONS.md — crystallized observations
- journal/CURIOSITY.md — open questions and themes
- journal/PRAXIS.md — behavioral policies from reflection
- journal/LOGBOOK.md — session records

Ideas flow: LEARNING → THOUGHTS → REFLECTIONS → SELF.md / PRAXIS.md
"#,
        entity_name = identity.entity_name,
        owner_name = identity.owner_name,
        owner_alias = identity.owner_alias,
    )
}

pub fn render_schedule_json() -> String {
    let schedule = crate::scheduler::Schedule::with_defaults();
    serde_json::to_string_pretty(&schedule).unwrap_or_else(|_| "{}".to_string())
}

pub fn render_memory_md(identity: &Identity) -> String {
    format!(
        "# {} — Memory\n\nCurated facts, preferences, and decisions. This file is always loaded into context.\n\n## Owner\n\n- Name: {}\n- Alias: {}\n",
        identity.entity_name, identity.owner_name, identity.owner_alias
    )
}
