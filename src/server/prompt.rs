use std::path::Path;
use std::sync::Arc;

use pulse_system_types::monitoring::{CognitiveMonitor, PipelineMonitor};

use crate::config::Config;
use crate::scheduler::intent::IntentQueue;

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
