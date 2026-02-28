use std::path::Path;

use crate::config::Config;
use crate::monitoring::assess;
use crate::pipeline;
use crate::pipeline::health;

/// Build the system prompt from entity documents
pub fn build_system_prompt(
    root_dir: &Path,
    config: &Config,
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

    // EPHEMERAL.md — last session summary
    let ephemeral_path = root_dir.join("memory/EPHEMERAL.md");
    if ephemeral_path.exists() {
        let content = std::fs::read_to_string(&ephemeral_path)?;
        if !content.trim().is_empty() {
            parts.push(format!("<last-session>\n{}\n</last-session>", content));
        }
    }

    // Pipeline health — document counts and threshold status
    if config.pipeline.enabled {
        let pipeline_state = pipeline::PipelineState::load(root_dir);
        let pipeline_health = health::calculate(root_dir, &config.pipeline);
        let pipeline_text = health::render(
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
    if config.monitoring.enabled {
        let cognitive_health = assess::assess(root_dir, &config.monitoring);
        let cognitive_text = assess::render(&cognitive_health);
        parts.push(format!(
            "<cognitive-health>\n{}\n</cognitive-health>",
            cognitive_text
        ));
    }

    Ok(parts.join("\n\n"))
}
