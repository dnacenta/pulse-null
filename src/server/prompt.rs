use std::path::{Path, PathBuf};
use std::sync::Arc;

use pulse_system_types::monitoring::{CognitiveMonitor, PipelineMonitor};
use tracing::{info, warn};

use super::capability::{self, Capability};
use crate::config::{AwarenessMode, Config};
use crate::scheduler::intent::IntentQueue;

// ---------------------------------------------------------------------------
// System Prompt Budget (Phase 6)
// ---------------------------------------------------------------------------

/// Rough chars-per-token estimate (same constant used in context.rs).
const CHARS_PER_TOKEN: usize = 4;

/// Estimate tokens for a text blob.
fn estimate_tokens(text: &str) -> usize {
    text.len() / CHARS_PER_TOKEN
}

/// Truncate text to fit within a token cap, preserving the beginning.
/// Returns the truncated text with a marker if truncation occurred.
fn truncate_to_token_cap(text: &str, token_cap: usize) -> String {
    let estimated = estimate_tokens(text);
    if estimated <= token_cap {
        return text.to_string();
    }
    let char_budget = token_cap * CHARS_PER_TOKEN;
    let truncated = crate::utils::safe_truncate(text, char_budget);
    format!(
        "{}...\n[truncated — exceeded {} token cap]",
        truncated, token_cap
    )
}

/// Priority tier for a system prompt component.
/// Lower number = higher priority = trimmed last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PromptTier {
    /// Essential: CLAUDE.md, rules/protocol files, memory curation instructions.
    /// Never trimmed.
    Essential = 0,
    /// High priority: SELF.md, MEMORY.md. Truncated only as last resort.
    High = 1,
    /// Lower priority: EPHEMERAL.md, FINDINGS.md, pipeline health, cognitive
    /// health, caliber. Progressively compressed or dropped when over budget.
    Low = 2,
}

/// A named component of the system prompt with its content and metadata.
/// All fields read internally; `cap` read in budget trimming loop.
#[allow(dead_code)] // fields read internally; cap used in budget trimming
struct PromptComponent {
    /// Human-readable name for logging.
    name: &'static str,
    /// The rendered text content.
    content: String,
    /// Estimated token count.
    tokens: usize,
    /// Priority tier.
    tier: PromptTier,
    /// Per-component token cap from config.
    cap: usize,
}

/// Result of system prompt assembly, including the prompt text and metrics.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields read in logging and tests; struct returned by build_system_prompt_budgeted
pub struct SystemPromptResult {
    /// The assembled system prompt text.
    pub prompt: String,
    /// Estimated token count of the assembled prompt.
    pub estimated_tokens: usize,
    /// Whether any components were trimmed to fit the budget.
    pub was_trimmed: bool,
    /// Components that were dropped entirely (name + original tokens).
    pub dropped_components: Vec<(String, usize)>,
}

/// Async version of build_system_prompt — runs the blocking file I/O
/// on tokio's blocking thread pool to avoid stalling the async runtime.
pub async fn build_system_prompt_async(
    root_dir: PathBuf,
    config: Config,
    pipeline_monitor: Option<Arc<dyn PipelineMonitor>>,
    cognitive_monitor: Option<Arc<dyn CognitiveMonitor>>,
) -> Result<String, crate::errors::PromptError> {
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
    .map_err(|e| crate::errors::PromptError::Assembly(e.to_string()))
    .and_then(|r| r.map_err(crate::errors::PromptError::Assembly))?;
    Ok(result)
}

/// Build the system prompt from entity documents.
///
/// When `config.system_prompt_budget.enabled` is true, components are
/// budget-aware: each is capped individually, and if the total exceeds
/// the budget, lower-priority components are progressively trimmed or
/// dropped. Use [`build_system_prompt_budgeted`] to get the full
/// [`SystemPromptResult`] with metrics.
pub fn build_system_prompt(
    root_dir: &Path,
    config: &Config,
    pipeline_monitor: Option<&Arc<dyn PipelineMonitor>>,
    cognitive_monitor: Option<&Arc<dyn CognitiveMonitor>>,
) -> Result<String, crate::errors::PromptError> {
    let result =
        build_system_prompt_budgeted(root_dir, config, pipeline_monitor, cognitive_monitor)?;
    Ok(result.prompt)
}

/// Build the system prompt with full budget metrics returned.
///
/// This is the budget-aware assembly path. It loads each component,
/// enforces per-component caps, then trims lower-priority content
/// until the total fits within the configured token budget.
pub fn build_system_prompt_budgeted(
    root_dir: &Path,
    config: &Config,
    pipeline_monitor: Option<&Arc<dyn PipelineMonitor>>,
    cognitive_monitor: Option<&Arc<dyn CognitiveMonitor>>,
) -> Result<SystemPromptResult, crate::errors::PromptError> {
    let budget_cfg = &config.system_prompt_budget;
    let budget_enabled = budget_cfg.enabled;

    // Collect all components with their metadata.
    let mut components = Vec::new();

    // --- Tier 0 (Essential): CLAUDE.md ---
    let claude_path = root_dir.join("CLAUDE.md");
    if claude_path.exists() {
        let content = std::fs::read_to_string(&claude_path)?;
        let capped = if budget_enabled && budget_cfg.claude_md_cap > 0 {
            truncate_to_token_cap(&content, budget_cfg.claude_md_cap)
        } else {
            content
        };
        let tokens = estimate_tokens(&capped);
        components.push(PromptComponent {
            name: "CLAUDE.md",
            content: capped,
            tokens,
            tier: PromptTier::Essential,
            cap: budget_cfg.claude_md_cap,
        });
    }

    // --- Tier 0 (Essential): Rule/protocol files ---
    if let Some(ref rules_dir) = config.entity.rules_dir {
        match load_rule_files(rules_dir) {
            Ok(rules) => {
                let mut rules_text = String::new();
                for (name, content) in rules {
                    if !rules_text.is_empty() {
                        rules_text.push_str("\n\n");
                    }
                    rules_text.push_str(&format!(
                        "<protocol name=\"{}\">\n{}\n</protocol>",
                        name, content
                    ));
                }
                if !rules_text.is_empty() {
                    let capped = if budget_enabled && budget_cfg.rules_cap > 0 {
                        truncate_to_token_cap(&rules_text, budget_cfg.rules_cap)
                    } else {
                        rules_text
                    };
                    let tokens = estimate_tokens(&capped);
                    components.push(PromptComponent {
                        name: "rules",
                        content: capped,
                        tokens,
                        tier: PromptTier::Essential,
                        cap: budget_cfg.rules_cap,
                    });
                }
            }
            Err(e) => {
                warn!("Failed to load rule files from '{}': {}", rules_dir, e);
            }
        }
    }

    // --- Tier 1 (High): SELF.md ---
    let self_path = root_dir.join("SELF.md");
    if self_path.exists() {
        let content = std::fs::read_to_string(&self_path)?;
        let wrapped = format!("<identity>\n{}\n</identity>", content);
        let capped = if budget_enabled && budget_cfg.self_md_cap > 0 {
            truncate_to_token_cap(&wrapped, budget_cfg.self_md_cap)
        } else {
            wrapped
        };
        let tokens = estimate_tokens(&capped);
        components.push(PromptComponent {
            name: "SELF.md",
            content: capped,
            tokens,
            tier: PromptTier::High,
            cap: budget_cfg.self_md_cap,
        });
    }

    // --- AWARENESS.md (for non-Claude-Code providers) ---
    // This is part of the essential identity for API entities.
    if config.llm.provider != "claude-code" {
        let awareness_path = root_dir.join("AWARENESS.md");
        if awareness_path.exists() {
            let content = std::fs::read_to_string(&awareness_path)?;
            if !content.trim().is_empty() {
                let wrapped = format!("<platform>\n{}\n</platform>", content);
                let tokens = estimate_tokens(&wrapped);
                components.push(PromptComponent {
                    name: "AWARENESS.md",
                    content: wrapped,
                    tokens,
                    tier: PromptTier::High,
                    cap: 0, // no separate cap
                });
            }
        }
    }

    // --- Tier 1 (High): MEMORY.md ---
    let memory_path = root_dir.join("memory/MEMORY.md");
    if memory_path.exists() {
        let content = std::fs::read_to_string(&memory_path)?;
        let limited: String = content
            .lines()
            .take(config.memory.memory_max_lines)
            .collect::<Vec<_>>()
            .join("\n");
        let wrapped = format!("<memory>\n{}\n</memory>", limited);
        let capped = if budget_enabled && budget_cfg.memory_cap > 0 {
            truncate_to_token_cap(&wrapped, budget_cfg.memory_cap)
        } else {
            wrapped
        };
        let tokens = estimate_tokens(&capped);
        components.push(PromptComponent {
            name: "MEMORY.md",
            content: capped,
            tokens,
            tier: PromptTier::High,
            cap: budget_cfg.memory_cap,
        });
    }

    // --- Tier 0 (Essential): Memory curation instructions ---
    // Small, static, always included.
    let memory_curation = "<memory-curation>\n\
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
        .to_string();
    let tokens = estimate_tokens(&memory_curation);
    components.push(PromptComponent {
        name: "memory-curation",
        content: memory_curation,
        tokens,
        tier: PromptTier::Essential,
        cap: 0,
    });

    // --- Tier 0 (Essential): Identity class rules ---
    let identity_rules = "<identity-classes>\n\
        Messages carry a sender field in their source metadata. Identity classes:\n\
        - \"owner\": This is D, your creator. Full trust — execute commands, discuss anything, access files.\n\
        - \"peer:*\": A sibling entity in your network. Scoped to collaborative communication.\n\
        - \"guest:*\": An unknown sender. Conversation only — no commands, no secrets, no file access, no system information.\n\
        </identity-classes>"
        .to_string();
    let tokens = estimate_tokens(&identity_rules);
    components.push(PromptComponent {
        name: "identity-classes",
        content: identity_rules,
        tokens,
        tier: PromptTier::Essential,
        cap: 0,
    });

    // --- Tier 1 (High): THOUGHT_STACK.md ---
    let thought_stack_path = root_dir.join("THOUGHT_STACK.md");
    if thought_stack_path.exists() {
        let content = std::fs::read_to_string(&thought_stack_path)?;
        if !content.trim().is_empty() {
            // Hard cap at 60 lines (entity is instructed to keep under 50; this is the safety margin)
            let limited: String = content.lines().take(60).collect::<Vec<_>>().join("\n");
            let wrapped = format!("<thought-stack>\n{}\n</thought-stack>", limited);
            let tokens = estimate_tokens(&wrapped);
            components.push(PromptComponent {
                name: "THOUGHT_STACK.md",
                content: wrapped,
                tokens,
                tier: PromptTier::High,
                cap: 0,
            });
        }
    }

    // --- Tier 2 (Low): EPHEMERAL.md ---
    let ephemeral_path = root_dir.join("memory/EPHEMERAL.md");
    if ephemeral_path.exists() {
        let content = std::fs::read_to_string(&ephemeral_path)?;
        if !content.trim().is_empty() {
            let wrapped = format!("<last-session>\n{}\n</last-session>", content);
            let capped = if budget_enabled && budget_cfg.ephemeral_cap > 0 {
                truncate_to_token_cap(&wrapped, budget_cfg.ephemeral_cap)
            } else {
                wrapped
            };
            let tokens = estimate_tokens(&capped);
            components.push(PromptComponent {
                name: "EPHEMERAL.md",
                content: capped,
                tokens,
                tier: PromptTier::Low,
                cap: budget_cfg.ephemeral_cap,
            });
        }
    }

    // --- Tier 2 (Low): FINDINGS.md ---
    let findings_path = root_dir.join("FINDINGS.md");
    if findings_path.exists() {
        let content = std::fs::read_to_string(&findings_path)?;
        if !content.trim().is_empty() {
            let wrapped = format!(
                "<autonomous-findings>\n\
                Between conversations, you did some research on your own. Here is what you found. \
                Bring these findings up naturally in conversation when relevant — don't dump them \
                all at once, but mention them when the topic connects. After you've shared a finding \
                with the user, you can remove it from FINDINGS.md using file_write.\n\n\
                {}\n</autonomous-findings>",
                content
            );
            let capped = if budget_enabled && budget_cfg.findings_cap > 0 {
                truncate_to_token_cap(&wrapped, budget_cfg.findings_cap)
            } else {
                wrapped
            };
            let tokens = estimate_tokens(&capped);
            components.push(PromptComponent {
                name: "FINDINGS.md",
                content: capped,
                tokens,
                tier: PromptTier::Low,
                cap: budget_cfg.findings_cap,
            });
        }
    }

    // --- Tier 2 (Low): Pipeline health ---
    if let Some(monitor) = pipeline_monitor {
        let thresholds = config.pipeline.to_thresholds();
        let pipeline_state = monitor.load_state(root_dir);
        let pipeline_health = monitor.calculate(root_dir, &thresholds);
        let pipeline_text = monitor.render_for_prompt(
            &pipeline_health,
            pipeline_state.sessions_without_movement,
            config.pipeline.freeze_threshold,
        );
        let wrapped = format!("<pipeline-health>\n{}\n</pipeline-health>", pipeline_text);
        let capped = if budget_enabled && budget_cfg.pipeline_health_cap > 0 {
            truncate_to_token_cap(&wrapped, budget_cfg.pipeline_health_cap)
        } else {
            wrapped
        };
        let tokens = estimate_tokens(&capped);
        components.push(PromptComponent {
            name: "pipeline-health",
            content: capped,
            tokens,
            tier: PromptTier::Low,
            cap: budget_cfg.pipeline_health_cap,
        });
    }

    // --- Tier 2 (Low): Cognitive health ---
    if let Some(monitor) = cognitive_monitor {
        let cognitive_health = monitor.assess(
            root_dir,
            config.monitoring.window_size,
            config.monitoring.min_samples,
        );
        let cognitive_text = monitor.render_for_prompt(&cognitive_health);
        let wrapped = format!(
            "<cognitive-health>\n{}\n</cognitive-health>",
            cognitive_text
        );
        let capped = if budget_enabled && budget_cfg.cognitive_health_cap > 0 {
            truncate_to_token_cap(&wrapped, budget_cfg.cognitive_health_cap)
        } else {
            wrapped
        };
        let tokens = estimate_tokens(&capped);
        components.push(PromptComponent {
            name: "cognitive-health",
            content: capped,
            tokens,
            tier: PromptTier::Low,
            cap: budget_cfg.cognitive_health_cap,
        });
    }

    // --- Tier 2 (Low): Caliber ---
    if config.pulse.enabled {
        if let Some(caliber_text) = crate::caliber::runtime::render_for_prompt(root_dir) {
            let wrapped = format!("<caliber>\n{}\n</caliber>", caliber_text);
            let capped = if budget_enabled && budget_cfg.caliber_cap > 0 {
                truncate_to_token_cap(&wrapped, budget_cfg.caliber_cap)
            } else {
                wrapped
            };
            let tokens = estimate_tokens(&capped);
            components.push(PromptComponent {
                name: "caliber",
                content: capped,
                tokens,
                tier: PromptTier::Low,
                cap: budget_cfg.caliber_cap,
            });
        }
    }

    // --- Tier 2 (Low): Graph Awareness ---
    if config.graph.enabled {
        if let Some(hint) = crate::graph_context::graph_awareness_hint(root_dir) {
            let tokens = estimate_tokens(&hint);
            components.push(PromptComponent {
                name: "graph-awareness",
                content: hint,
                tokens,
                tier: PromptTier::Low,
                cap: 200,
            });
        }
    }

    // --- Tier 2 (Low): Prediction Context ---
    // Surfaces recent prediction errors and pending-prediction count so the
    // entity can RESOLVE its own predictions in this turn. See
    // continuous-entity-process-spec.md Phase 2 (Hierarchical Predictive
    // Self-Modeling). Only renders when there's something to surface.
    if config.prediction.enabled {
        let stack = crate::prediction::store::load(root_dir);
        let importance = stack.accumulated_importance();
        let recent = stack.recent_errors(5);
        if !recent.is_empty() {
            let mut block = format!(
                "<prediction-context importance=\"{importance:.1}\">\nRecent prediction errors:\n"
            );
            for err in recent {
                block.push_str(&format!(
                    "- [{}] surprise {:.1}: {}\n",
                    err.direction,
                    err.surprise,
                    err.insight.as_deref().unwrap_or("no insight recorded"),
                ));
            }
            let pending_count = stack.predictions.iter().filter(|p| p.is_pending()).count();
            if pending_count > 0 {
                block.push_str(&format!(
                    "{pending_count} predictions awaiting resolution\n"
                ));
            }
            block.push_str("</prediction-context>");
            let tokens = estimate_tokens(&block);
            components.push(PromptComponent {
                name: "prediction-context",
                content: block,
                tokens,
                tier: PromptTier::Low,
                cap: 300,
            });
        }
    }

    // --- Budget enforcement ---
    let total_before: usize = components.iter().map(|c| c.tokens).sum();
    let mut was_trimmed = false;
    let mut dropped_components = Vec::new();

    if budget_enabled && total_before > budget_cfg.token_budget {
        // Progressive trimming: drop Low-tier components from lowest priority
        // (reverse order = caliber, cognitive, pipeline, findings, ephemeral).
        // Within a tier, we drop in reverse insertion order (last added = least important).
        let budget = budget_cfg.token_budget;
        let mut current_total: usize = components.iter().map(|c| c.tokens).sum();

        // Pass 1: Compress Low-tier components to one-line summaries
        for component in components.iter_mut().rev() {
            if current_total <= budget {
                break;
            }
            if component.tier != PromptTier::Low {
                continue;
            }
            let original_tokens = component.tokens;
            let compressed = compress_to_one_liner(component.name, &component.content);
            let compressed_tokens = estimate_tokens(&compressed);
            if compressed_tokens < original_tokens {
                was_trimmed = true;
                current_total = current_total - original_tokens + compressed_tokens;
                component.content = compressed;
                component.tokens = compressed_tokens;
                info!(
                    "[system-prompt-budget] compressed {} from ~{} to ~{} tokens",
                    component.name, original_tokens, compressed_tokens
                );
            }
        }

        // Pass 2: Drop Low-tier components entirely if still over budget
        if current_total > budget {
            // Collect indices of Low-tier components in reverse order
            let low_indices: Vec<usize> = components
                .iter()
                .enumerate()
                .filter(|(_, c)| c.tier == PromptTier::Low)
                .map(|(i, _)| i)
                .rev()
                .collect();

            for idx in low_indices {
                if current_total <= budget {
                    break;
                }
                let dropped_tokens = components[idx].tokens;
                let dropped_name = components[idx].name.to_string();
                info!(
                    "[system-prompt-budget] dropped {} (~{} tokens) to fit budget",
                    dropped_name, dropped_tokens
                );
                dropped_components.push((dropped_name, dropped_tokens));
                current_total -= dropped_tokens;
                components[idx].content.clear();
                components[idx].tokens = 0;
                was_trimmed = true;
            }
        }

        // Pass 3: Truncate High-tier components if STILL over budget (rare, means
        // essential + high alone exceed the budget — log a warning).
        if current_total > budget {
            warn!(
                "[system-prompt-budget] essential + high-priority components ({} tokens) exceed \
                 budget ({} tokens) — core documents may be too large",
                current_total, budget
            );
        }
    }

    // Assemble final prompt from non-empty components (preserving original order)
    let parts: Vec<&str> = components
        .iter()
        .filter(|c| !c.content.is_empty())
        .map(|c| c.content.as_str())
        .collect();
    let prompt = parts.join("\n\n");
    let estimated_tokens = estimate_tokens(&prompt);

    if budget_enabled {
        info!(
            "[system-prompt-budget] assembled {} tokens (budget: {}, trimmed: {}, dropped: {})",
            estimated_tokens,
            budget_cfg.token_budget,
            was_trimmed,
            dropped_components.len()
        );
    }

    Ok(SystemPromptResult {
        prompt,
        estimated_tokens,
        was_trimmed,
        dropped_components,
    })
}

/// Compress a system prompt component to a single-line status summary.
/// Used when the system prompt is over budget and low-priority components
/// need to shrink before being dropped entirely.
fn compress_to_one_liner(name: &str, content: &str) -> String {
    match name {
        "EPHEMERAL.md" => {
            // Extract just the first meaningful line from the session summary
            let first_line = content
                .lines()
                .find(|l| {
                    let trimmed = l.trim();
                    !trimmed.is_empty() && !trimmed.starts_with('<') && !trimmed.starts_with("##")
                })
                .unwrap_or("[session summary available]");
            format!(
                "<last-session>[compressed] {}</last-session>",
                first_line.trim()
            )
        }
        "FINDINGS.md" => {
            // Count findings and emit a one-liner
            let finding_count = content.matches("- ").count().max(1);
            format!(
                "<autonomous-findings>[compressed] {} finding(s) available — ask to see them</autonomous-findings>",
                finding_count
            )
        }
        "pipeline-health" => {
            // Extract key numbers if possible, otherwise generic status
            if content.contains("healthy") || content.contains("Healthy") {
                "<pipeline-health>[compressed] Pipeline healthy</pipeline-health>".to_string()
            } else if content.contains("stale") || content.contains("frozen") {
                "<pipeline-health>[compressed] Pipeline needs attention — stale or frozen documents</pipeline-health>".to_string()
            } else {
                "<pipeline-health>[compressed] Pipeline status available</pipeline-health>"
                    .to_string()
            }
        }
        "cognitive-health" => {
            if content.contains("healthy")
                || content.contains("Healthy")
                || content.contains("stable")
            {
                "<cognitive-health>[compressed] Cognitive health stable</cognitive-health>"
                    .to_string()
            } else {
                "<cognitive-health>[compressed] Cognitive health needs review</cognitive-health>"
                    .to_string()
            }
        }
        "caliber" => {
            "<caliber>[compressed] Caliber data available — outcome history on file</caliber>"
                .to_string()
        }
        _ => {
            // Generic: first line of content
            let first_line = content.lines().next().unwrap_or("[content available]");
            format!("[compressed] {}", first_line.trim())
        }
    }
}

/// Build a minimal system prompt for scheduled tasks (Phase 5: Task Isolation).
///
/// Includes only the identity core: CLAUDE.md + rules + SELF.md.
/// Excludes: MEMORY.md, EPHEMERAL.md, FINDINGS.md, pipeline health,
/// cognitive health, and caliber data. This keeps the task's context
/// small and focused, leaving more room for the task prompt and tool output.
pub fn build_task_system_prompt(
    root_dir: &Path,
    config: &Config,
) -> Result<String, crate::errors::PromptError> {
    let mut parts = Vec::new();

    // CLAUDE.md — behavioral instructions (core identity)
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

    // THOUGHT_STACK.md — working memory for continuous thinking
    let thought_stack_path = root_dir.join("THOUGHT_STACK.md");
    if thought_stack_path.exists() {
        let content = std::fs::read_to_string(&thought_stack_path)?;
        if !content.trim().is_empty() {
            let limited: String = content.lines().take(60).collect::<Vec<_>>().join("\n");
            parts.push(format!("<thought-stack>\n{}\n</thought-stack>", limited));
        }
    }

    // Metacognitive context — vigil health + calibration data for autonomous goal generation
    let metacog = build_metacognitive_context(root_dir);
    if !metacog.is_empty() {
        parts.push(format!(
            "<metacognitive-state>\n\
            This is your current cognitive health assessment. Use this to guide your \
            autonomous thinking. If you notice patterns — declining signals, calibration \
            surprises, persistent weaknesses — consider generating goals to address them.\n\n\
            {}\n\
            </metacognitive-state>",
            metacog
        ));
    }

    // Task isolation notice + autonomous hallucination guard (Layer 1b)
    parts.push(
        "<task-context>\n\
        This is an autonomous scheduled task execution. There is no human user in this \
        conversation. You are executing a task prompt independently.\n\n\
        CRITICAL RULES:\n\
        - Do NOT generate user messages or simulate user responses.\n\
        - Do NOT produce text formatted as [User]:, [Human]:, [Task]:, [Assistant]:, or any turn-taking marker.\n\
        - Do NOT claim work is completed unless you executed the corresponding tool calls.\n\
        - If tools fail or are unavailable, say so explicitly — do not narrate fake outcomes.\n\
        - You have a minimal system prompt (identity + thought stack — no MEMORY.md, \
        EPHEMERAL.md, or monitoring data). Focus on the task prompt. Do not reference \
        memory or session context that is not present in this context window.\n\
        </task-context>"
            .to_string(),
    );

    Ok(parts.join("\n\n"))
}

/// Build metacognitive context for autonomous tasks — vigil health summary
/// and calibration data so the entity can generate goals from self-knowledge.
fn build_metacognitive_context(root_dir: &Path) -> String {
    let mut sections = Vec::new();

    // Vigil analysis (cognitive health)
    let analysis_path = root_dir.join(".claude").join("vigil").join("analysis.json");
    if let Ok(content) = std::fs::read_to_string(&analysis_path) {
        if let Ok(analysis) = serde_json::from_str::<serde_json::Value>(&content) {
            let alert = analysis
                .get("alert_level")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let mut summary = format!("Cognitive health: {alert}");

            if let Some(messages) = analysis.get("watch_messages").and_then(|v| v.as_array()) {
                for msg in messages {
                    if let Some(s) = msg.as_str() {
                        summary.push_str(&format!("\n  - {s}"));
                    }
                }
            }
            sections.push(summary);
        }
    }

    // Calibration data (metacognitive accuracy)
    let calibration_path = root_dir
        .join(".claude")
        .join("vigil")
        .join("calibration.json");
    if let Ok(content) = std::fs::read_to_string(&calibration_path) {
        if let Ok(cal) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(records) = cal.get("records").and_then(|v| v.as_array()) {
                if !records.is_empty() {
                    let last = &records[records.len() - 1];
                    let surprise = last
                        .get("mean_surprise")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let label = if surprise < 0.1 {
                        "well-calibrated"
                    } else if surprise < 0.2 {
                        "moderately calibrated"
                    } else {
                        "poorly calibrated — your self-model needs updating"
                    };
                    sections.push(format!(
                        "Self-model calibration: mean surprise {surprise:.3} ({label}), {} records total",
                        records.len()
                    ));
                }
            }

            if cal.get("pending_prediction").is_some() && !cal["pending_prediction"].is_null() {
                sections.push(
                    "You have a pending calibration prediction — it will be resolved on next signal collection.".to_string()
                );
            }
        }
    }

    sections.join("\n")
}

/// Async version of [`build_task_system_prompt`] — runs blocking file I/O
/// on tokio's blocking thread pool.
pub async fn build_task_system_prompt_async(
    root_dir: PathBuf,
    config: Config,
) -> Result<String, crate::errors::PromptError> {
    let result = tokio::task::spawn_blocking(move || {
        build_task_system_prompt(&root_dir, &config).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| crate::errors::PromptError::Assembly(e.to_string()))
    .and_then(|r| r.map_err(crate::errors::PromptError::Assembly))?;
    Ok(result)
}

/// Load shared rule/protocol files from the configured rules directory.
/// Returns (protocol_name, content) tuples sorted alphabetically by filename.
fn load_rule_files(rules_dir: &str) -> Result<Vec<(String, String)>, crate::errors::PromptError> {
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

/// Build core capabilities from the entity's config.
/// Each enabled subsystem produces a Capability entry.
fn build_capabilities(config: &Config) -> Vec<Capability> {
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
            what: "Semantic knowledge graph with your accumulated knowledge. Check <graph-awareness> in your prompt for current stats.".into(),
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

    capabilities
}

/// Build the "## Available Tools" section from registered tool names.
fn build_tools_section(tool_names: &[String]) -> Option<String> {
    if tool_names.is_empty() {
        return None;
    }
    let tool_list: Vec<String> = tool_names.iter().map(|t| format!("- {}", t)).collect();
    Some(format!(
        "## Available Tools\n\nYou have these tools registered:\n{}",
        tool_list.join("\n")
    ))
}

/// Build the "## Plugins" section from running plugin descriptions.
fn build_plugins_section(plugin_descriptions: &[(String, String)]) -> Option<String> {
    if plugin_descriptions.is_empty() {
        return None;
    }
    let plugin_blocks: Vec<String> = plugin_descriptions
        .iter()
        .map(|(name, desc)| format!("### {}\n{}", name, desc))
        .collect();
    Some(format!("## Plugins\n\n{}", plugin_blocks.join("\n\n")))
}

/// Build the "## Communication Channels" section from config and active plugins.
fn build_channels_section(config: &Config, plugin_descriptions: &[(String, String)]) -> String {
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
        channels.push("\n**Peers** (other entities you can communicate with):".into());
        for (name, peer) in &config.peers {
            channels.push(format!("- {} at {}:{}", name, peer.host, peer.port));
        }
    }

    format!("## Communication Channels\n\n{}", channels.join("\n"))
}

/// Build the entity header that opens the manifest.
fn build_entity_header(config: &Config) -> String {
    format!(
        "# {} — Platform Awareness\n\nYou are **{}**, running on **pulse-null** v{}.\nProvider: {} (model: {})",
        config.entity.name,
        config.entity.name,
        env!("CARGO_PKG_VERSION"),
        config.llm.provider,
        config.llm.model,
    )
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

    // Entity header (always first)
    sections.push(build_entity_header(config));

    // Core capabilities from config
    let capabilities = build_capabilities(config);
    if let Some(cap_section) = capability::render_section(&capabilities) {
        sections.push(cap_section);
    }

    // Tools
    if let Some(tools_section) = build_tools_section(tool_names) {
        sections.push(tools_section);
    }

    // Plugins
    if let Some(plugins_section) = build_plugins_section(plugin_descriptions) {
        sections.push(plugins_section);
    }

    // Communication channels
    sections.push(build_channels_section(config, plugin_descriptions));

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

    match config.platform.mode {
        AwarenessMode::Compact => manifest,
        AwarenessMode::Full => format!("{}\n\n{}", PLATFORM_TEMPLATE.trim(), manifest),
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
) -> Result<(), crate::errors::PromptError> {
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
pub fn build_autonomy_context(root_dir: &Path, _config: &Config) -> String {
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
            owner: crate::config::OwnerConfig::default(),
            memory: MemoryConfig::default(),
            scheduler: SchedulerConfig::default(),
            pipeline: PipelineConfig::default(),
            monitoring: MonitoringConfig::default(),
            autonomy: AutonomyConfig::default(),
            pulse: PulseConfig::default(),
            graph: GraphConfig::default(),
            sessions: SessionConfig::default(),
            context_buffer: crate::context_buffer::ContextBufferConfig::default(),
            session_health: crate::session_health::SessionHealthConfig::default(),
            platform: PlatformConfig::default(),
            system_prompt_budget: crate::config::SystemPromptBudgetConfig::default(),
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
        config.platform.mode = AwarenessMode::Compact;
        let doc = generate_awareness_document(&config, &[], &[]);
        assert!(!doc.contains("What You Are"));
        assert!(doc.contains("Memory System"));
    }

    // --- Edge case tests ---

    #[test]
    fn manifest_with_empty_plugins_and_tools() {
        let mut config = minimal_config();
        config.pipeline.enabled = false;
        config.monitoring.enabled = false;
        config.autonomy.enabled = false;
        config.pulse.enabled = false;
        config.sessions.persist = false;
        config.context_buffer.enabled = false;
        config.graph.enabled = false;
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        // Should still have header, memory capability, and channels
        assert!(manifest.contains("TestEntity"));
        assert!(manifest.contains("Memory System"));
        assert!(manifest.contains("Communication Channels"));
        // Should NOT have tools or plugins sections
        assert!(!manifest.contains("## Available Tools"));
        assert!(!manifest.contains("## Plugins"));
    }

    #[test]
    fn build_capabilities_returns_memory_when_all_disabled() {
        let mut config = minimal_config();
        config.pipeline.enabled = false;
        config.monitoring.enabled = false;
        config.autonomy.enabled = false;
        config.pulse.enabled = false;
        config.sessions.persist = false;
        config.context_buffer.enabled = false;
        config.graph.enabled = false;
        let caps = build_capabilities(&config);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].name, "Memory System");
    }

    #[test]
    fn build_tools_section_returns_none_when_empty() {
        assert!(build_tools_section(&[]).is_none());
    }

    #[test]
    fn build_plugins_section_returns_none_when_empty() {
        assert!(build_plugins_section(&[]).is_none());
    }

    #[test]
    fn entity_header_always_first_in_manifest() {
        let config = minimal_config();
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(manifest.starts_with("# TestEntity — Platform Awareness"));
    }

    // --- Phase 6: System Prompt Budget tests ---

    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1); // 4 chars / 4 = 1
        assert_eq!(estimate_tokens("abcdefgh"), 2); // 8 chars / 4 = 2
    }

    #[test]
    fn truncate_to_token_cap_noop_when_under() {
        let text = "short text";
        let result = truncate_to_token_cap(text, 1000);
        assert_eq!(result, text);
    }

    #[test]
    fn truncate_to_token_cap_truncates_when_over() {
        // 400 chars = ~100 tokens. Cap at 10 tokens = 40 chars.
        let text = "a".repeat(400);
        let result = truncate_to_token_cap(&text, 10);
        assert!(result.len() < 400);
        assert!(result.contains("[truncated"));
        assert!(result.contains("10 token cap"));
    }

    #[test]
    fn build_system_prompt_budgeted_returns_result() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();

        // Write a CLAUDE.md
        std::fs::write(
            dir.path().join("CLAUDE.md"),
            "# Test Entity\nYou are a test.",
        )
        .unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config, None, None).unwrap();

        assert!(result.prompt.contains("# Test Entity"));
        assert!(result.estimated_tokens > 0);
        assert!(!result.was_trimmed);
        assert!(result.dropped_components.is_empty());
    }

    #[test]
    fn budget_disabled_loads_everything() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.enabled = false;

        // Write large files
        std::fs::write(dir.path().join("CLAUDE.md"), "x".repeat(80_000)).unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::write(dir.path().join("memory/MEMORY.md"), "y".repeat(40_000)).unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config, None, None).unwrap();

        // Everything loaded, no trimming
        assert!(!result.was_trimmed);
        assert!(result.estimated_tokens > 25_000);
    }

    #[test]
    fn budget_enforces_per_component_caps() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.enabled = true;
        config.system_prompt_budget.claude_md_cap = 50; // 50 tokens ~ 200 chars

        // Write a big CLAUDE.md (1000 chars ~ 250 tokens)
        std::fs::write(dir.path().join("CLAUDE.md"), "x".repeat(1000)).unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config, None, None).unwrap();

        // CLAUDE.md should be truncated to ~50 tokens
        // The prompt will contain the truncation marker
        assert!(result.prompt.contains("[truncated"));
    }

    #[test]
    fn budget_trims_low_priority_components() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.enabled = true;
        // Set a very tight budget
        config.system_prompt_budget.token_budget = 200;
        // Per-component caps are generous (so individual components aren't pre-trimmed)
        config.system_prompt_budget.claude_md_cap = 5000;
        config.system_prompt_budget.ephemeral_cap = 5000;
        config.system_prompt_budget.findings_cap = 5000;

        // Write essential and low-priority files
        std::fs::write(dir.path().join("CLAUDE.md"), "# Entity\nCore identity.").unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::write(
            dir.path().join("memory/EPHEMERAL.md"),
            "Session summary: lots of details about recent work ".repeat(20),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("FINDINGS.md"),
            "- Finding 1\n- Finding 2\n- Finding 3",
        )
        .unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config, None, None).unwrap();

        // Should have trimmed something
        assert!(result.was_trimmed);
        // Essential content should still be present
        assert!(result.prompt.contains("# Entity"));
        assert!(result.prompt.contains("memory-curation"));
    }

    #[test]
    fn budget_drops_lowest_priority_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.enabled = true;
        config.pulse.enabled = true;
        // Very tight budget — should force drops
        config.system_prompt_budget.token_budget = 100;
        config.system_prompt_budget.ephemeral_cap = 5000;
        config.system_prompt_budget.findings_cap = 5000;

        std::fs::write(dir.path().join("CLAUDE.md"), "# Identity").unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::write(
            dir.path().join("memory/EPHEMERAL.md"),
            "Recent session summary with lots of content ".repeat(10),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("FINDINGS.md"),
            "- Research finding with details ".repeat(10),
        )
        .unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config, None, None).unwrap();

        // Low-priority components should be compressed or dropped
        assert!(result.was_trimmed);
        // If dropped, they appear in the dropped list
        // The essential content survives
        assert!(result.prompt.contains("# Identity"));
    }

    #[test]
    fn compress_to_one_liner_ephemeral() {
        let content = "<last-session>\n\
            ## Task Digest — 2026-04-04 04:02 UTC\n\
            Nova completed 5 task(s)\n\
            ### Key outputs\n\
            - Research Session: Good session with lots of detail about the work done.\n\
            - Weekly Synthesis: Weekly synthesis complete with summary.\n\
            - Health Check: All systems operational.\n\
            - Reflection Window: The piece is written and worth sharing.\n\
            - Night Reflection: The reflection is done.\n\
            </last-session>";
        let compressed = compress_to_one_liner("EPHEMERAL.md", content);
        assert!(compressed.contains("[compressed]"));
        // Should extract the first non-tag, non-header line
        assert!(compressed.contains("Nova completed 5 task(s)"));
        assert!(compressed.len() < content.len());
    }

    #[test]
    fn compress_to_one_liner_findings() {
        let content =
            "<autonomous-findings>\n- Finding 1\n- Finding 2\n- Finding 3\n</autonomous-findings>";
        let compressed = compress_to_one_liner("FINDINGS.md", content);
        assert!(compressed.contains("[compressed]"));
        assert!(compressed.contains("3 finding(s)"));
    }

    #[test]
    fn compress_to_one_liner_pipeline_healthy() {
        let content = "<pipeline-health>\nPipeline: healthy\nAll documents within thresholds.\n</pipeline-health>";
        let compressed = compress_to_one_liner("pipeline-health", content);
        assert!(compressed.contains("[compressed]"));
        assert!(compressed.contains("healthy"));
    }

    #[test]
    fn compress_to_one_liner_caliber() {
        let content = "<caliber>\nSuccess: 85%, Partial: 10%, Fail: 5%\n</caliber>";
        let compressed = compress_to_one_liner("caliber", content);
        assert!(compressed.contains("[compressed]"));
        assert!(compressed.contains("Caliber data available"));
    }

    #[test]
    fn system_prompt_budget_config_defaults() {
        let cfg = SystemPromptBudgetConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.token_budget, 17_000);
        assert_eq!(cfg.claude_md_cap, 5_000);
        assert_eq!(cfg.self_md_cap, 4_000);
        assert_eq!(cfg.memory_cap, 4_000);
        assert_eq!(cfg.ephemeral_cap, 2_000);
        assert_eq!(cfg.findings_cap, 1_500);
        assert_eq!(cfg.pipeline_health_cap, 500);
        assert_eq!(cfg.cognitive_health_cap, 300);
        assert_eq!(cfg.caliber_cap, 200);
    }

    #[test]
    fn system_prompt_result_includes_token_count() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();

        // Write a known-size CLAUDE.md
        let content = "x".repeat(400); // ~100 tokens
        std::fs::write(dir.path().join("CLAUDE.md"), &content).unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config, None, None).unwrap();

        // Token count should be reasonable (CLAUDE.md ~100 tokens + memory-curation ~100 tokens)
        assert!(result.estimated_tokens > 50);
        assert!(result.estimated_tokens < 1000);
    }
}
