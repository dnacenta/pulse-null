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
