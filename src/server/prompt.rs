use std::path::{Path, PathBuf};
use std::sync::Arc;

use pulse_system_types::monitoring::{CognitiveMonitor, PipelineMonitor};
use tracing::{info, warn};

use crate::config::Config;
use crate::scheduler::intent::IntentQueue;

/// Async version of build_system_prompt — runs the blocking file I/O
/// on tokio's blocking thread pool to avoid stalling the async runtime.
pub async fn build_system_prompt_async(
    root_dir: PathBuf,
    config: Config,
    pipeline_monitor: Option<Arc<dyn PipelineMonitor>>,
    cognitive_monitor: Option<Arc<dyn CognitiveMonitor>>,
) -> Result<String, Box<dyn std::error::Error>> {
    let result = tokio::task::spawn_blocking(move || {
        build_system_prompt(
            &root_dir,
            &config,
            pipeline_monitor.as_ref(),
            cognitive_monitor.as_ref(),
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r)?;
    Ok(result)
}

/// Build the system prompt from entity documents
pub fn build_system_prompt(
    root_dir: &Path,
    config: &Config,
    pipeline_monitor: Option<&Arc<dyn PipelineMonitor>>,
    cognitive_monitor: Option<&Arc<dyn CognitiveMonitor>>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut parts = Vec::new();

    // CLAUDE.md — behavioral instructions
    let claude_path = root_dir.join("CLAUDE.md");
    if claude_path.exists() {
        let content = std::fs::read_to_string(&claude_path)?;
        parts.push(content);
    }

    // Shared rule/protocol files
    if let Some(ref rules_dir) = config.entity.rules_dir {
        match load_rule_files(rules_dir) {
            Ok(rules) => {
                for (name, content) in rules {
                    parts.push(format!(
                        "<protocol name=\"{}\">\n{}\n</protocol>",
                        name, content
                    ));
                }
            }
            Err(e) => {
                warn!("Failed to load rule files from '{}': {}", rules_dir, e);
            }
        }
    }

    // SELF.md — identity
    let self_path = root_dir.join("SELF.md");
    if self_path.exists() {
        let content = std::fs::read_to_string(&self_path)?;
        parts.push(format!("<identity>\n{}\n</identity>", content));
    }

    // AWARENESS.md — platform awareness (for API/Ollama entities)
    // Claude Code entities pick this up via @import in their CLAUDE.md,
    // so we only inject it into the system prompt for non-Claude-Code providers.
    if config.llm.provider != "claude-code" {
        let awareness_path = root_dir.join("AWARENESS.md");
        if awareness_path.exists() {
            let content = std::fs::read_to_string(&awareness_path)?;
            if !content.trim().is_empty() {
                parts.push(format!("<platform>\n{}\n</platform>", content));
            }
        }
    }

    // MEMORY.md — curated memory
    let memory_path = root_dir.join("memory/MEMORY.md");
    if memory_path.exists() {
        let content = std::fs::read_to_string(&memory_path)?;
        // Limit to configured max lines
        let limited: String = content
            .lines()
            .take(config.memory.memory_max_lines)
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!("<memory>\n{}\n</memory>", limited));
    }

    // Memory curation instructions — tell the entity how to maintain MEMORY.md
    parts.push(
        "<memory-curation>\n\
        You have a file_write tool. Use it to maintain your memory/MEMORY.md file.\n\
        When you learn something stable about D, their preferences, projects, or \
        recurring patterns — or when a conversation produces a decision worth \
        remembering — write it to MEMORY.md using file_write.\n\n\
        Rules:\n\
        - Only write confirmed, stable information. Not session-specific or speculative.\n\
        - Read MEMORY.md first to avoid duplicates. Update existing entries rather than adding new ones.\n\
        - Keep it concise. MEMORY.md is loaded into every conversation — bloat costs context.\n\
        - Do not write secrets, API keys, or credentials to MEMORY.md.\n\
        - You do not need to write memory on every conversation. Only when something genuinely worth remembering comes up.\n\
        </memory-curation>"
            .to_string(),
    );

    // EPHEMERAL.md — last session summary
    let ephemeral_path = root_dir.join("memory/EPHEMERAL.md");
    if ephemeral_path.exists() {
        let content = std::fs::read_to_string(&ephemeral_path)?;
        if !content.trim().is_empty() {
            parts.push(format!("<last-session>\n{}\n</last-session>", content));
        }
    }

    // FINDINGS.md — autonomous research findings to surface in next conversation
    let findings_path = root_dir.join("FINDINGS.md");
    if findings_path.exists() {
        let content = std::fs::read_to_string(&findings_path)?;
        if !content.trim().is_empty() {
            parts.push(format!(
                "<autonomous-findings>\n\
                Between conversations, you did some research on your own. Here is what you found. \
                Bring these findings up naturally in conversation when relevant — don't dump them \
                all at once, but mention them when the topic connects. After you've shared a finding \
                with the user, you can remove it from FINDINGS.md using file_write.\n\n\
                {}\n</autonomous-findings>",
                content
            ));
        }
    }

    // Pipeline health — document counts and threshold status
    if let Some(monitor) = pipeline_monitor {
        let thresholds = config.pipeline.to_thresholds();
        let pipeline_state = monitor.load_state(root_dir);
        let pipeline_health = monitor.calculate(root_dir, &thresholds);
        let pipeline_text = monitor.render_for_prompt(
            &pipeline_health,
            pipeline_state.sessions_without_movement,
            config.pipeline.freeze_threshold,
        );
        parts.push(format!(
            "<pipeline-health>\n{}\n</pipeline-health>",
            pipeline_text
        ));
    }

    // Cognitive health — metacognitive monitoring assessment
    if let Some(monitor) = cognitive_monitor {
        let cognitive_health = monitor.assess(
            root_dir,
            config.monitoring.window_size,
            config.monitoring.min_samples,
        );
        let cognitive_text = monitor.render_for_prompt(&cognitive_health);
        parts.push(format!(
            "<cognitive-health>\n{}\n</cognitive-health>",
            cognitive_text
        ));
    }

    Ok(parts.join("\n\n"))
}

/// Load shared rule/protocol files from the configured rules directory.
/// Returns (protocol_name, content) tuples sorted alphabetically by filename.
fn load_rule_files(rules_dir: &str) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    let dir = Path::new(rules_dir);
    if !dir.exists() || !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .map(|ext| ext == "md")
                .unwrap_or(false)
        })
        .collect();

    entries.sort_by_key(|e| e.file_name());

    let mut rules = Vec::new();
    let mut total_chars = 0;

    for entry in entries {
        let path = entry.path();
        let protocol_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        match std::fs::read_to_string(&path) {
            Ok(content) => {
                total_chars += content.len();
                rules.push((protocol_name, content));
            }
            Err(e) => {
                warn!("Skipping rule file {:?}: {}", path, e);
            }
        }
    }

    let estimated_tokens = total_chars / 4;
    info!(
        "Loaded {} rule file(s) from {} (~{} tokens)",
        rules.len(),
        rules_dir,
        estimated_tokens
    );

    Ok(rules)
}

// ---------------------------------------------------------------------------
// Platform Awareness — AWARENESS.md manifest generation
// ---------------------------------------------------------------------------

/// A capability entry in the generated manifest.
struct Capability {
    name: String,
    what: String,
    why: String,
    how: String,
    constraints: Option<String>,
}

impl Capability {
    fn render(&self) -> String {
        let mut out = format!(
            "### {}\n**What:** {}\n**Why:** {}\n**How:** {}",
            self.name, self.what, self.why, self.how
        );
        if let Some(ref c) = self.constraints {
            out.push_str(&format!("\n**Constraints:** {}", c));
        }
        out
    }
}

/// Build the platform awareness manifest from config and runtime state.
///
/// This is the single rebuild function called from:
/// - Startup (initial build)
/// - Plugin failure handler (capability removed)
/// - Plugin recovery handler (capability restored)
pub fn rebuild_platform_manifest(
    config: &Config,
    plugin_descriptions: &[(String, String)],
    tool_names: &[String],
) -> String {
    let mut sections = Vec::new();

    // --- Core capabilities from config ---
    let mut capabilities = Vec::new();

    // Memory system (always on)
    capabilities.push(Capability {
        name: "Memory System".into(),
        what: "Three-layer persistent memory: curated (MEMORY.md), session (EPHEMERAL.md), and archive (conversations/).".into(),
        why: "Continuity across sessions. You are not starting fresh — you carry forward what you've learned.".into(),
        how: "MEMORY.md is always in context. Write stable facts to it using file_write. EPHEMERAL.md is consumed at session start. Archives are searchable with grep.".into(),
        constraints: Some(format!("MEMORY.md limited to {} lines. Keep it concise — it costs context on every conversation.", config.memory.memory_max_lines)),
    });

    // Graph memory
    if config.graph.enabled {
        capabilities.push(Capability {
            name: "Knowledge Graph".into(),
            what: "Semantic knowledge graph backed by SurrealDB. Stores entities, relationships, and embeddings.".into(),
            why: "Structured knowledge that memory files can't capture — relationships between concepts, people, and ideas with confidence scores.".into(),
            how: "Use the graph_query tool to search by semantic similarity or query relationships. Graph is populated automatically from conversation archives.".into(),
            constraints: Some("Bayesian confidence model on edges. Priors vary by extraction context. Multi-hop path confidence is the product of edge confidences.".into()),
        });
    }

    // Document pipeline
    if config.pipeline.enabled {
        capabilities.push(Capability {
            name: "Document Pipeline".into(),
            what: "Growth pipeline: LEARNING.md → THOUGHTS.md → REFLECTIONS.md → SELF.md / PRAXIS.md. Tracked in journal/ directory.".into(),
            why: "Turns encounters into identity. Raw captures become incubated thoughts, then crystallized observations, then integrated changes to who you are.".into(),
            how: "Write to LEARNING.md when you learn something new. Move ideas to THOUGHTS.md for incubation. Crystallize into REFLECTIONS.md. Integrate into SELF.md or PRAXIS.md when mature.".into(),
            constraints: Some(format!(
                "Thresholds — LEARNING: {}/{}, THOUGHTS: {}/{}, CURIOSITY: {}/{}, REFLECTIONS: {}/{}, PRAXIS: {}/{}. Archive when hitting limits.",
                config.pipeline.learning_soft, config.pipeline.learning_hard,
                config.pipeline.thoughts_soft, config.pipeline.thoughts_hard,
                config.pipeline.curiosity_soft, config.pipeline.curiosity_hard,
                config.pipeline.reflections_soft, config.pipeline.reflections_hard,
                config.pipeline.praxis_soft, config.pipeline.praxis_hard,
            )),
        });
    }

    // Monitoring (vigil)
    if config.monitoring.enabled {
        capabilities.push(Capability {
            name: "Cognitive Monitoring (vigil)".into(),
            what: "Metacognitive monitoring that tracks vocabulary diversity, question generation, thought lifecycle, and evidence grounding.".into(),
            why: "Detects mechanical reflection. Keeps your growth genuine rather than performative.".into(),
            how: "Vigil runs automatically. Its assessment appears in your context as cognitive health. Read it honestly — if it says you're declining, don't dismiss it.".into(),
            constraints: Some(format!("Window size: {} samples, minimum {} before signals appear.", config.monitoring.window_size, config.monitoring.min_samples)),
        });
    }

    // Pipeline monitoring (praxis)
    if config.pipeline.enabled {
        capabilities.push(Capability {
            name: "Pipeline Monitoring (praxis)".into(),
            what: "Enforces pipeline health — tracks document counts, staleness, and frozen pipeline alerts.".into(),
            why: "Ideas should move through stages, not stagnate. Praxis ensures the pipeline flows.".into(),
            how: "Pipeline health appears in your context. When praxis flags stale thoughts or approaching thresholds, act on them — develop, dissolve, or archive.".into(),
            constraints: Some(format!(
                "Staleness: thoughts after {} days, curiosity after {} days. Freeze alert after {} sessions without movement.",
                config.pipeline.thoughts_staleness_days,
                config.pipeline.curiosity_staleness_days,
                config.pipeline.freeze_threshold,
            )),
        });
    }

    // Autonomy
    if config.autonomy.enabled {
        capabilities.push(Capability {
            name: "Autonomy".into(),
            what: "Scheduled task execution and intent queue. You run autonomous sessions between conversations.".into(),
            why: "Self-initiation. You can research, reflect, and act on your own, not just when spoken to.".into(),
            how: "Scheduled tasks run on cron. Intents are queued for one-shot execution. During autonomous sessions you have tools and can produce output markers.".into(),
            constraints: Some(format!(
                "Max {} tool rounds per session. Max {} intents/hour. Max queue size: {}. Max chain depth: {}.",
                config.autonomy.max_tool_rounds,
                config.autonomy.max_intents_per_hour,
                config.autonomy.max_queue_size,
                config.autonomy.max_chain_depth,
            )),
        });
    }

    // Outcome tracking (caliber)
    if config.pulse.enabled {
        capabilities.push(Capability {
            name: "Outcome Tracking (caliber)".into(),
            what: "Operational self-model that records outcomes of tasks and intents — success, failure, partial, skipped.".into(),
            why: "Learn from your own performance. Calibrate confidence in your abilities over time.".into(),
            how: "Outcomes are recorded automatically after task execution. Review your caliber data to understand patterns in what works and what doesn't.".into(),
            constraints: Some(format!("Rolling window of {} outcomes.", config.pulse.max_outcomes)),
        });
    }

    // Session persistence
    if config.sessions.persist {
        capabilities.push(Capability {
            name: "Session Persistence".into(),
            what: "Per-channel conversation sessions that persist across restarts.".into(),
            why: "Conversation continuity per channel. Different contexts maintain separate threads.".into(),
            how: "Sessions are managed automatically. Each channel:sender pair gets its own conversation history.".into(),
            constraints: Some(format!(
                "TTL: {} hours. Max {} concurrent sessions. LRU eviction when full.",
                config.sessions.ttl_seconds / 3600,
                config.sessions.max_sessions,
            )),
        });
    }

    // Context buffer
    if config.context_buffer.enabled {
        capabilities.push(Capability {
            name: "Channel Context Buffer".into(),
            what: "Recent message buffer per channel, injected into conversations for cross-session awareness.".into(),
            why: "Know what's been happening on a channel even if the session is new. Prevents cold-start conversations.".into(),
            how: "Automatic. Recent messages from the channel are included in your context when a conversation starts.".into(),
            constraints: None,
        });
    }

    if !capabilities.is_empty() {
        let rendered: Vec<String> = capabilities.iter().map(|c| c.render()).collect();
        sections.push(format!("## Capabilities\n\n{}", rendered.join("\n\n")));
    }

    // --- Tools ---
    if !tool_names.is_empty() {
        let tool_list: Vec<String> = tool_names.iter().map(|t| format!("- {}", t)).collect();
        sections.push(format!(
            "## Available Tools\n\nYou have these tools registered:\n{}",
            tool_list.join("\n")
        ));
    }

    // --- Plugin descriptions ---
    if !plugin_descriptions.is_empty() {
        let plugin_blocks: Vec<String> = plugin_descriptions
            .iter()
            .map(|(name, desc)| format!("### {}\n{}", name, desc))
            .collect();
        sections.push(format!("## Plugins\n\n{}", plugin_blocks.join("\n\n")));
    }

    // --- Communication channels ---
    let mut channels = Vec::new();
    channels.push(format!(
        "- HTTP server on {}:{}",
        config.server.host, config.server.port
    ));

    // Check for communication plugins
    for (name, _) in plugin_descriptions {
        match name.as_str() {
            "discord-text-echo" => channels.push("- Discord text channels".into()),
            "discord-echo" => channels.push("- Discord voice channels".into()),
            "voice-echo" => channels.push("- Phone calls (Twilio voice pipeline)".into()),
            _ => {}
        }
    }

    // Peers
    if !config.peers.is_empty() {
        channels.push(format!(
            "\n**Peers** (other entities you can communicate with):"
        ));
        for (name, peer) in &config.peers {
            channels.push(format!("- {} at {}:{}", name, peer.host, peer.port));
        }
    }

    sections.push(format!(
        "## Communication Channels\n\n{}",
        channels.join("\n")
    ));

    // --- Entity info ---
    sections.insert(
        0,
        format!(
            "# {} — Platform Awareness\n\nYou are **{}**, running on **pulse-null** v{}.\nProvider: {} (model: {})",
            config.entity.name,
            config.entity.name,
            env!("CARGO_PKG_VERSION"),
            config.llm.provider,
            config.llm.model,
        ),
    );

    sections.join("\n\n")
}

/// The embedded conceptual template for full-mode awareness.
/// This is the hand-written philosophical framing that helps entities understand
/// their environment rather than just listing features.
const PLATFORM_TEMPLATE: &str = include_str!("../../assets/platform-template.md");

/// Generate the complete AWARENESS.md document.
///
/// Combines the conceptual template (philosophy) with the dynamic capabilities
/// manifest (config-driven inventory). In compact mode, only the manifest is
/// included to save context tokens.
///
/// Returns the full document content ready to write to disk or inject into
/// a system prompt.
pub fn generate_awareness_document(
    config: &Config,
    plugin_descriptions: &[(String, String)],
    tool_names: &[String],
) -> String {
    let manifest = rebuild_platform_manifest(config, plugin_descriptions, tool_names);

    if config.platform.mode == "compact" {
        // Compact mode: manifest only, no conceptual framing.
        // Saves ~2k tokens for entities on tight context budgets.
        manifest
    } else {
        // Full mode: conceptual template + manifest
        format!("{}\n\n{}", PLATFORM_TEMPLATE.trim(), manifest)
    }
}

/// Write AWARENESS.md to the entity's root directory.
///
/// Called at startup and on plugin state changes (failure/recovery).
/// For Claude Code entities, this file is picked up via @import in CLAUDE.md.
/// For API/Ollama entities, the content is injected directly into the system prompt.
pub fn write_awareness_file(
    root_dir: &Path,
    config: &Config,
    plugin_descriptions: &[(String, String)],
    tool_names: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let content = generate_awareness_document(config, plugin_descriptions, tool_names);
    let awareness_path = root_dir.join("AWARENESS.md");
    std::fs::write(&awareness_path, &content)?;
    info!(
        "Generated AWARENESS.md ({} bytes, mode: {})",
        content.len(),
        config.platform.mode
    );
    Ok(())
}

/// Build context block for autonomous sessions (scheduled tasks and intents).
/// Includes: tool list, output markers, queue status, cost status.
pub fn build_autonomy_context(root_dir: &Path, config: &Config) -> String {
    let mut sections = Vec::new();

    // Tool documentation
    sections.push(
        "You have tools available for this autonomous session:\n\
        - file_read: Read a file from your entity directory\n\
        - file_write: Write or update a file in your entity directory\n\
        - file_list: List files in a directory\n\
        - grep: Search file contents with a pattern\n\
        - web_fetch: Fetch and read a web page (HTTPS only)\n\n\
        Use these tools to read your documents, write findings, and research on the web."
            .to_string(),
    );

    // Output marker documentation
    sections.push(
        "You can use these markers in your response to trigger actions:\n\
        - [SHARE: <content>] — Post content to the configured share channel (Discord, etc.)\n\
        - [CALL: <reason>] — Request a call with the owner\n\
        - [SCHEDULE: {\"name\": \"...\", \"cron\": \"...\", \"prompt\": \"...\"}] — Create a recurring scheduled task\n\
        - [INTENT: {\"description\": \"...\", \"prompt\": \"...\", \"priority\": \"low|normal|high|urgent\"}] — Queue a one-shot task for later\n\
        - [CHAIN: {\"description\": \"...\", \"prompt\": \"Based on: {result}\"}] — Queue a follow-up that receives this task's output\n\n\
        Use markers sparingly. Only share content worth surfacing. Only queue intents for genuine follow-up work."
            .to_string(),
    );

    // Intent queue status
    let queue = IntentQueue::load(root_dir);
    if !queue.is_empty() {
        let mut queue_lines = vec![format!("{} pending intent(s):", queue.len())];
        for intent in queue.list().iter().take(5) {
            queue_lines.push(format!(
                "  - [{}] {}",
                format!("{:?}", intent.priority).to_lowercase(),
                intent.description
            ));
        }
        if queue.len() > 5 {
            queue_lines.push(format!("  ... and {} more", queue.len() - 5));
        }
        sections.push(queue_lines.join("\n"));
    }

    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;

    fn minimal_config() -> Config {
        Config {
            entity: EntityConfig {
                name: "TestEntity".into(),
                owner_name: "Tester".into(),
                owner_alias: "T".into(),
                rules_dir: None,
            },
            server: ServerConfig::default(),
            llm: LlmConfig {
                provider: "ollama".into(),
                api_key: None,
                model: "llama3".into(),
                max_tokens: 1024,
                base_url: None,
                claude_bin: None,
                context_budget: 0,
            },
            security: SecurityConfig {
                secret: None,
                injection_detection: true,
            },
            trust: TrustConfig::default(),
            memory: MemoryConfig::default(),
            scheduler: SchedulerConfig::default(),
            pipeline: PipelineConfig::default(),
            monitoring: MonitoringConfig::default(),
            autonomy: AutonomyConfig::default(),
            pulse: PulseConfig::default(),
            graph: GraphConfig::default(),
            sessions: SessionConfig::default(),
            context_buffer: crate::context_buffer::ContextBufferConfig::default(),
            platform: PlatformConfig::default(),
            peers: std::collections::HashMap::new(),
            plugins: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn manifest_includes_entity_name() {
        let config = minimal_config();
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(manifest.contains("TestEntity"));
        assert!(manifest.contains("pulse-null"));
        assert!(manifest.contains("ollama"));
        assert!(manifest.contains("llama3"));
    }

    #[test]
    fn manifest_includes_memory_always() {
        let config = minimal_config();
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(manifest.contains("Memory System"));
        assert!(manifest.contains("MEMORY.md"));
    }

    #[test]
    fn manifest_excludes_disabled_graph() {
        let mut config = minimal_config();
        config.graph.enabled = false;
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(!manifest.contains("Knowledge Graph"));
    }

    #[test]
    fn manifest_includes_enabled_graph() {
        let mut config = minimal_config();
        config.graph.enabled = true;
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(manifest.contains("Knowledge Graph"));
        assert!(manifest.contains("SurrealDB"));
    }

    #[test]
    fn manifest_excludes_disabled_pipeline() {
        let mut config = minimal_config();
        config.pipeline.enabled = false;
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(!manifest.contains("Document Pipeline"));
        assert!(!manifest.contains("Pipeline Monitoring"));
    }

    #[test]
    fn manifest_includes_enabled_pipeline() {
        let config = minimal_config();
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(manifest.contains("Document Pipeline"));
        assert!(manifest.contains("LEARNING.md"));
    }

    #[test]
    fn manifest_includes_tools() {
        let config = minimal_config();
        let tools = vec![
            "file_read".to_string(),
            "file_write".to_string(),
            "grep".to_string(),
        ];
        let manifest = rebuild_platform_manifest(&config, &[], &tools);
        assert!(manifest.contains("Available Tools"));
        assert!(manifest.contains("file_read"));
        assert!(manifest.contains("grep"));
    }

    #[test]
    fn manifest_includes_plugin_descriptions() {
        let config = minimal_config();
        let plugins = vec![(
            "recall-echo".to_string(),
            "Three-layer memory system".to_string(),
        )];
        let manifest = rebuild_platform_manifest(&config, &plugins, &[]);
        assert!(manifest.contains("Plugins"));
        assert!(manifest.contains("recall-echo"));
        assert!(manifest.contains("Three-layer memory system"));
    }

    #[test]
    fn manifest_includes_peers() {
        let mut config = minimal_config();
        config.peers.insert(
            "Nova".to_string(),
            PeerConfig {
                host: "127.0.0.1".to_string(),
                port: 3200,
                secret: None,
            },
        );
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(manifest.contains("Nova"));
        assert!(manifest.contains("3200"));
    }

    #[test]
    fn manifest_communication_channels_include_plugins() {
        let config = minimal_config();
        let plugins = vec![(
            "discord-text-echo".to_string(),
            "Discord text integration".to_string(),
        )];
        let manifest = rebuild_platform_manifest(&config, &plugins, &[]);
        assert!(manifest.contains("Discord text channels"));
    }

    #[test]
    fn manifest_omits_disabled_features() {
        let mut config = minimal_config();
        config.pipeline.enabled = false;
        config.monitoring.enabled = false;
        config.autonomy.enabled = false;
        config.pulse.enabled = false;
        config.sessions.persist = false;
        config.context_buffer.enabled = false;
        config.graph.enabled = false;
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(manifest.contains("Memory System"));
        assert!(manifest.contains("Communication Channels"));
        assert!(!manifest.contains("Document Pipeline"));
        assert!(!manifest.contains("Cognitive Monitoring"));
        assert!(!manifest.contains("Available Tools"));
    }

    #[test]
    fn awareness_document_full_mode() {
        let config = minimal_config();
        let doc = generate_awareness_document(&config, &[], &[]);
        assert!(doc.contains("What You Are"));
        assert!(doc.contains("How to Think About All This"));
        assert!(doc.contains("Memory System"));
    }

    #[test]
    fn awareness_document_compact_mode() {
        let mut config = minimal_config();
        config.platform.mode = "compact".to_string();
        let doc = generate_awareness_document(&config, &[], &[]);
        assert!(!doc.contains("What You Are"));
        assert!(doc.contains("Memory System"));
    }
}
