use std::path::{Path, PathBuf};

use tracing::{info, warn};

use super::capability::{self, Capability};
use crate::config::{AwarenessMode, Config};
use crate::providers::provider_reports_tool_rounds;
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

/// Truncate text to a hard byte ceiling, preserving the beginning.
///
/// Line counts do not bound bytes: a 60-line file of 3KB lines is 180KB. Every
/// line-capped component gets one of these ceilings so a single runaway
/// document cannot blow up the assembled prompt.
///
/// The result is at most `max_bytes` bytes, including the truncation marker
/// (unless `max_bytes` is smaller than the marker itself). Truncation happens
/// on a char boundary, so the output is always valid UTF-8.
fn truncate_to_byte_cap(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let marker = format!("\n[truncated — exceeded {} byte ceiling]", max_bytes);
    let content_budget = max_bytes.saturating_sub(marker.len());
    format!(
        "{}{}",
        crate::utils::safe_truncate(text, content_budget),
        marker
    )
}

/// Line cap for THOUGHT_STACK.md. The pulse is instructed to keep it under
/// 50 lines; this is the safety margin on top of that.
const THOUGHT_STACK_MAX_LINES: usize = 60;

/// Pulse-facing soft budget for THOUGHT_STACK.md, stricter than the hard
/// ceiling carried in `thought_stack_max_bytes` (48 KiB).
///
/// The hard cap is where `truncate_to_byte_cap` starts deleting text; the soft
/// budget is the size the pulse holds the file to, so that a cycle's additions
/// land inside the ceiling rather than on top of it. Both appear in the
/// utilisation report because they answer different questions: "am I about to
/// lose content?" and "am I keeping the rule I was given?".
const THOUGHT_STACK_SOFT_BUDGET_BYTES: usize = 45_000;

/// How full a self-authored, capped document is against its limits.
///
/// `truncate_to_byte_cap` emits its marker only when it actually fires, so
/// below the ceiling a document's size is never reported and the only way to
/// learn it is to measure the file by hand. The report is taken on the read
/// path every prompt assembly already walks, so the reading arrives whether or
/// not anyone chose to take it. It is a report, not a gate: it never refuses,
/// trims or warns.
///
/// The figures describe the file **as it is on disk**, before the line cap and
/// the byte cap are applied. A post-trim count would understate the problem in
/// exactly the case where the line cap has already dropped content silently.
///
/// It is logged, not rendered into the prompt. Under budget pressure each
/// token added to a High-tier block comes straight out of the thought stack's
/// tail, so the report must not cost the content it measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Utilisation {
    label: &'static str,
    bytes: usize,
    byte_cap: usize,
    soft_budget: Option<usize>,
    lines: usize,
    line_cap: usize,
}

impl Utilisation {
    fn measure(label: &'static str, content: &str, byte_cap: usize, line_cap: usize) -> Self {
        Self {
            label,
            bytes: content.len(),
            byte_cap,
            soft_budget: None,
            lines: content.lines().count(),
            line_cap,
        }
    }

    #[must_use]
    fn with_soft_budget(self, soft_budget: usize) -> Self {
        Self {
            soft_budget: Some(soft_budget),
            ..self
        }
    }

    fn log(&self) {
        info!("[prompt-utilisation] {self}");
    }
}

impl std::fmt::Display for Utilisation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} B", self.label, self.bytes)?;
        if let Some(budget) = self.soft_budget {
            write!(f, " / {budget} budget")?;
        }
        write!(
            f,
            " / {} cap, {} / {} lines",
            self.byte_cap, self.lines, self.line_cap
        )
    }
}

/// Hard byte ceiling for the Essential tier as a whole.
///
/// Essential components are never trimmed, so if they alone exceed this the
/// prompt is unshippable and assembly fails loudly rather than handing a
/// doomed prompt to the provider.
const ESSENTIAL_MAX_BYTES: usize = 64 * 1024;

/// Priority tier for a system prompt component.
/// Lower number = higher priority = trimmed last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PromptTier {
    /// Essential: the instruction file, rules/protocol files, memory curation instructions.
    /// Never trimmed.
    Essential = 0,
    /// High priority: SELF.md, MEMORY.md, EPHEMERAL.md, AWARENESS.md,
    /// THOUGHT_STACK.md. Truncated only as last resort, in `allocation_rank`
    /// order.
    High = 1,
    /// Lower priority: FINDINGS.md, graph awareness.
    /// Progressively compressed or dropped when over budget.
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

impl PromptComponent {
    /// Order in which High-tier components claim what the budget leaves
    /// after Essential — separate from render order, so the prompt's layout
    /// (and its cache-stable prefix) does not decide what survives. Small
    /// runtime facts first, the pulse's identity next, the big documents
    /// after, THOUGHT_STACK.md last: it is the one built to absorb
    /// truncation. Lower claims first; keyed on the component's fixed name.
    fn allocation_rank(&self) -> u8 {
        match self.name {
            "tension-context" | "journal-counts" => 0,
            "SELF.md" | "MEMORY.md" | "EPHEMERAL.md" => 1,
            "AWARENESS.md" => 3,
            "THOUGHT_STACK.md" => 4,
            _ => 2,
        }
    }
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
) -> Result<String, crate::errors::PromptError> {
    let result = tokio::task::spawn_blocking(move || {
        build_system_prompt(&root_dir, &config).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| crate::errors::PromptError::Assembly(e.to_string()))
    .and_then(|r| r.map_err(crate::errors::PromptError::Assembly))?;
    Ok(result)
}

/// Build the system prompt from pulse documents.
///
/// When `config.system_prompt_budget.enabled` is true, components are
/// budget-aware: each is capped individually, and if the total exceeds
/// the budget, lower-priority components are progressively trimmed or
/// dropped. Use [`build_system_prompt_budgeted`] to get the full
/// [`SystemPromptResult`] with metrics.
pub fn build_system_prompt(
    root_dir: &Path,
    config: &Config,
) -> Result<String, crate::errors::PromptError> {
    let result = build_system_prompt_budgeted(root_dir, config)?;
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
) -> Result<SystemPromptResult, crate::errors::PromptError> {
    let budget_cfg = &config.system_prompt_budget;
    let budget_enabled = budget_cfg.enabled;

    // Collect all components with their metadata.
    let mut components = Vec::new();

    // --- Tier 0 (Essential): the instruction file ---
    if let Some(path) = instruction_file(root_dir, config) {
        let label = instruction_label(&path);
        let content = std::fs::read_to_string(&path)?;
        let capped = if budget_enabled && budget_cfg.instructions_cap > 0 {
            truncate_to_token_cap(&content, budget_cfg.instructions_cap)
        } else {
            content
        };
        let tokens = estimate_tokens(&capped);
        components.push(PromptComponent {
            name: label,
            content: capped,
            tokens,
            tier: PromptTier::Essential,
            cap: budget_cfg.instructions_cap,
        });
    }

    // --- Tier 0 (Essential): rule files — the operator's, then the pulse's own ---
    components.extend(rules_components(root_dir, config));

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

    // --- AWARENESS.md ---
    if let Some(component) = awareness_component(root_dir, config)? {
        components.push(component);
    }

    // --- Tier 1 (High): MEMORY.md ---
    let memory_path = root_dir.join("memory/MEMORY.md");
    if memory_path.exists() {
        let content = std::fs::read_to_string(&memory_path)?;
        // MEMORY.md is self-authored against a line cap its author cannot see,
        // exactly like THOUGHT_STACK.md, so it gets the same read-side report.
        Utilisation::measure(
            "MEMORY.md",
            &content,
            budget_cfg.memory_max_bytes,
            config.memory.memory_max_lines,
        )
        .log();
        let limited: String = content
            .lines()
            .take(config.memory.memory_max_lines)
            .collect::<Vec<_>>()
            .join("\n");
        let bounded = truncate_to_byte_cap(&limited, budget_cfg.memory_max_bytes);
        let wrapped = format!("<memory>\n{}\n</memory>", bounded);
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
    let memory_curation = format!(
        "<memory-curation>\n\
        Use {} to maintain your memory/MEMORY.md file.\n\
        When you learn something stable about D, their preferences, projects, or \
        recurring patterns — or when a conversation produces a decision worth \
        remembering — write it to MEMORY.md.\n\n\
        Rules:\n\
        - Only write confirmed, stable information. Not session-specific or speculative.\n\
        - Read MEMORY.md first to avoid duplicates. Update existing entries rather than adding new ones.\n\
        - Keep it concise. MEMORY.md is loaded into every conversation — bloat costs context.\n\
        - Do not write secrets, API keys, or credentials to MEMORY.md.\n\
        - You do not need to write memory on every conversation. Only when something genuinely worth remembering comes up.\n\
        </memory-curation>",
        file_write_tool(config)
    );
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
        - \"peer:*\": A sibling pulse in your network. Scoped to collaborative communication.\n\
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

    // --- Tier 1 (High): THOUGHT_STACK.md, held to its chat cap ---
    let chat_cap = (budget_enabled && budget_cfg.thought_stack_chat_cap > 0)
        .then_some(budget_cfg.thought_stack_chat_cap);
    if let Some(wrapped) =
        load_thought_stack(root_dir, budget_cfg.thought_stack_max_bytes, chat_cap)?
    {
        let tokens = estimate_tokens(&wrapped);
        components.push(PromptComponent {
            name: "THOUGHT_STACK.md",
            content: wrapped,
            tokens,
            tier: PromptTier::High,
            cap: 0,
        });
    }

    // --- Tier 1 (High): journal counts (a runtime fact, Amendment C) ---
    components.extend(journal_counts_component(root_dir, config));

    // --- Tier 1 (High): EPHEMERAL.md ---
    // The last session's summary is the conversation's only continuity
    // besides MEMORY.md, and it is small and capped. As a Low block it was
    // the first thing dropped to pay for a big document's overflow.
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
                tier: PromptTier::High,
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
                with the user, you can remove it from FINDINGS.md with {}.\n\n\
                {}\n</autonomous-findings>",
                file_write_tool(config),
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

    // --- Tier 2 (Low): Graph Awareness ---
    if config.graph.enabled {
        let query_tool = registry_tools_callable(config);
        if let Some(hint) = crate::graph_context::graph_awareness_hint(root_dir, query_tool) {
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

    enforce_budget_and_assemble(components, budget_cfg, "chat")
}

/// Apply the budget passes to a component set and render the final prompt.
///
/// Shared by the chat and scheduled-task assembly paths so both are governed by
/// the same tiers, the same passes and the same metrics.
///
/// Passes, in order:
/// 1. Compress Low-tier components to one-line summaries.
/// 2. Drop Low-tier components entirely.
/// 3. Hard-truncate High-tier components down to whatever the Essential tier
///    leaves of the budget.
///
/// Essential components are never trimmed; they are instead verified against
/// [`ESSENTIAL_MAX_BYTES`] and reported as an error when they exceed it.
fn enforce_budget_and_assemble(
    mut components: Vec<PromptComponent>,
    budget_cfg: &crate::config::SystemPromptBudgetConfig,
    profile: &str,
) -> Result<SystemPromptResult, crate::errors::PromptError> {
    let budget_enabled = budget_cfg.enabled;
    let total_before: usize = components.iter().map(|c| c.tokens).sum();
    let tokens_before: Vec<usize> = components.iter().map(|c| c.tokens).collect();
    let mut was_trimmed = false;
    let mut dropped_components = Vec::new();

    if budget_enabled && total_before > budget_cfg.token_budget {
        // Progressive trimming: drop Low-tier components from lowest priority
        // (reverse order = graph awareness, findings, ephemeral).
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

        // Pass 3: Hard-truncate High-tier components if STILL over budget.
        // Essential is untouchable, so High shares whatever the budget has left
        // after Essential, claimed in `allocation_rank` order (render order
        // breaks ties). A component left with no allowance at all is dropped
        // rather than rendered as an empty truncation stub.
        if current_total > budget {
            let essential_tokens: usize = components
                .iter()
                .filter(|c| c.tier == PromptTier::Essential)
                .map(|c| c.tokens)
                .sum();
            warn!(
                "[system-prompt-budget] essential + high-priority components ({} tokens) exceed \
                 budget ({} tokens) — truncating high-priority content to fit",
                current_total, budget
            );

            let mut allowance = budget.saturating_sub(essential_tokens);
            let mut claim_order: Vec<usize> = components
                .iter()
                .enumerate()
                .filter(|(_, c)| c.tier == PromptTier::High && !c.content.is_empty())
                .map(|(index, _)| index)
                .collect();
            claim_order.sort_by_key(|&index| (components[index].allocation_rank(), index));
            for index in claim_order {
                let component = &mut components[index];
                if component.tokens <= allowance {
                    allowance -= component.tokens;
                    continue;
                }
                let original_tokens = component.tokens;
                if allowance == 0 {
                    info!(
                        "[system-prompt-budget] dropped {} (~{} tokens): no allowance left",
                        component.name, original_tokens
                    );
                    dropped_components.push((component.name.to_string(), original_tokens));
                    component.content.clear();
                    component.tokens = 0;
                    was_trimmed = true;
                    continue;
                }
                let truncated = truncate_to_token_cap(&component.content, allowance);
                component.tokens = estimate_tokens(&truncated);
                component.content = truncated;
                allowance = 0;
                was_trimmed = true;
                info!(
                    "[system-prompt-budget] truncated {} from ~{} to ~{} tokens",
                    component.name, original_tokens, component.tokens
                );
            }
        }
    }

    // Essential is never trimmed, so it is the one tier that can still push the
    // prompt past what a subprocess can carry. Verify it before shipping.
    let mut essential_bytes: usize = components
        .iter()
        .filter(|c| c.tier == PromptTier::Essential)
        .map(|c| c.content.len())
        .sum();
    // The pulse writes its own rules; they never make a prompt unshippable.
    // Over the ceiling, they go first — the operator's components keep the
    // hard error below.
    if essential_bytes > ESSENTIAL_MAX_BYTES {
        if let Some(rules) = components
            .iter_mut()
            .find(|c| c.name == "pulse-rules" && !c.content.is_empty())
        {
            warn!(
                "[system-prompt-budget] essential tier is {} bytes, over {} — dropping the \
                 pulse's own rules ({} bytes)",
                essential_bytes,
                ESSENTIAL_MAX_BYTES,
                rules.content.len()
            );
            essential_bytes -= rules.content.len();
            dropped_components.push((rules.name.to_string(), rules.tokens));
            rules.content.clear();
            rules.tokens = 0;
            was_trimmed = true;
        }
    }
    if essential_bytes > ESSENTIAL_MAX_BYTES {
        return Err(crate::errors::PromptError::EssentialTooLarge {
            bytes: essential_bytes,
            limit: ESSENTIAL_MAX_BYTES,
        });
    }

    // Assemble final prompt from non-empty components (preserving original order)
    let parts: Vec<&str> = components
        .iter()
        .filter(|c| !c.content.is_empty())
        .map(|c| c.content.as_str())
        .collect();
    let prompt = parts.join("\n\n");
    let estimated_tokens = estimate_tokens(&prompt);

    info!(
        "[system-prompt-budget] {} prompt assembled: {} tokens, {} bytes \
         (budget: {}, enabled: {}, trimmed: {}, dropped: {})",
        profile,
        estimated_tokens,
        prompt.len(),
        budget_cfg.token_budget,
        budget_enabled,
        was_trimmed,
        dropped_components.len()
    );
    info!(
        "[system-prompt-budget] {} components (~tokens): {}",
        profile,
        component_breakdown(&components, &tokens_before)
    );

    Ok(SystemPromptResult {
        prompt,
        estimated_tokens,
        was_trimmed,
        dropped_components,
    })
}

/// `name=tokens` per component in render order, `name=before→after` for one
/// the budget passes changed. The assembled total alone does not say which
/// block is crowding the rest out.
fn component_breakdown(components: &[PromptComponent], tokens_before: &[usize]) -> String {
    components
        .iter()
        .zip(tokens_before)
        .map(|(component, &before)| {
            if component.tokens == before {
                format!("{}={}", component.name, before)
            } else {
                format!("{}={}→{}", component.name, before, component.tokens)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Compress a system prompt component to a single-line status summary.
/// Used when the system prompt is over budget and low-priority components
/// need to shrink before being dropped entirely.
fn compress_to_one_liner(name: &str, content: &str) -> String {
    match name {
        "FINDINGS.md" => {
            // Count findings and emit a one-liner
            let finding_count = content.matches("- ").count().max(1);
            format!(
                "<autonomous-findings>[compressed] {} finding(s) available — ask to see them</autonomous-findings>",
                finding_count
            )
        }
        _ => {
            // Generic: first line of content
            let first_line = content.lines().next().unwrap_or("[content available]");
            format!("[compressed] {}", first_line.trim())
        }
    }
}

/// The entity's instruction file: the generic `INSTRUCTIONS.md`, else the
/// pre-PN-106 file for entities created before the split, else the
/// adapter's own file — but never a *pointer* (a file whose whole content
/// is a reference to INSTRUCTIONS.md), which would replace the entity's
/// identity with a one-line stub.
fn instruction_file(root_dir: &Path, config: &Config) -> Option<std::path::PathBuf> {
    let mut candidates = vec!["INSTRUCTIONS.md".to_string(), "CLAUDE.md".to_string()]; // vendor-ok: pre-PN-106 entities
    if let Some(adapter) = config
        .llm
        .cli_adapter()
        .and_then(crate::cli_provider::adapters::by_name)
    {
        candidates.push(adapter.integration().instruction_file().to_string());
    }
    candidates
        .into_iter()
        .map(|name| root_dir.join(name))
        .filter(|p| p.is_file())
        .find(|p| !is_pointer_file(p))
}

/// A file that only points at INSTRUCTIONS.md (what the adapters write as
/// their own instruction file) carries no instructions of its own.
fn is_pointer_file(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|text| {
            let body: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect();
            // A pointer names INSTRUCTIONS.md in its first line of body text
            // and says little else; a real instruction file has substance.
            body.first()
                .is_some_and(|first| first.contains("INSTRUCTIONS.md"))
                && body.len() <= 3
        })
        .unwrap_or(false)
}

/// Static label for the instruction file that was actually read, so budget
/// logs name the right file.
fn instruction_label(path: &Path) -> &'static str {
    match path.file_name().and_then(|n| n.to_str()) {
        Some("INSTRUCTIONS.md") => "INSTRUCTIONS.md",
        Some("AGENTS.md") => "AGENTS.md",
        Some("CLAUDE.md") => "CLAUDE.md", // vendor-ok: pre-PN-106 entities
        _ => "instructions",
    }
}

/// Can the pulse call the in-process tool registry (`file_write`,
/// `graph_query`, …)? Only providers handed its definitions can; an agent CLI
/// runs its own tools, so text must not name registry tools to it.
fn registry_tools_callable(config: &Config) -> bool {
    provider_reports_tool_rounds(&config.llm.provider)
}

/// How prompt text refers to the pulse's file-writing tool.
fn file_write_tool(config: &Config) -> &'static str {
    if registry_tools_callable(config) {
        "your file_write tool"
    } else {
        "your file tools"
    }
}

/// AWARENESS.md as a `<platform>` component (High tier, bounded by
/// `awareness_max_bytes`), for every profile. An agent CLI that resolves the
/// `@AWARENESS.md` import in its instruction file gets it that way; everything
/// else gets it inlined here. `None` when imported, missing or blank.
fn awareness_component(
    root_dir: &Path,
    config: &Config,
) -> Result<Option<PromptComponent>, crate::errors::PromptError> {
    if awareness_via_import(config) {
        return Ok(None);
    }
    let awareness_path = root_dir.join("AWARENESS.md");
    // Read on every prompt build, and the pulse can write it: bounded and
    // regular-only. A symlink, FIFO or oversized file is skipped, not fatal.
    let content = match crate::fs_safe::read_bounded(&awareness_path, MAX_AWARENESS_COMPARE_BYTES) {
        Ok(Some(content)) => content,
        Ok(None) => return Ok(None),
        Err(e) => {
            warn!("Skipping AWARENESS.md: {e}");
            return Ok(None);
        }
    };
    if content.trim().is_empty() {
        return Ok(None);
    }
    let bounded = truncate_to_byte_cap(&content, config.system_prompt_budget.awareness_max_bytes);
    let wrapped = format!("<platform>\n{}\n</platform>", bounded);
    Ok(Some(PromptComponent {
        name: "AWARENESS.md",
        tokens: estimate_tokens(&wrapped),
        content: wrapped,
        tier: PromptTier::High,
        cap: 0, // no separate cap
    }))
}

/// The adapter for the agent CLI this config drives, if any.
fn agent_cli(config: &Config) -> Option<Box<dyn crate::cli_provider::adapter::CliAdapter>> {
    config
        .llm
        .cli_adapter()
        .and_then(crate::cli_provider::adapters::by_name)
}

/// Does the entity's agent CLI pull AWARENESS.md in by itself (through an
/// import in its instruction file)? When not, the prompt builder inlines it.
fn awareness_via_import(config: &Config) -> bool {
    agent_cli(config).is_some_and(|adapter| adapter.integration().imports_awareness())
}

/// Build a minimal system prompt for scheduled tasks (Phase 5: Task Isolation).
///
/// Includes only the identity core: the instruction file, the rules (the
/// operator's, and the pulse's own for a CLI that does not load them) and
/// SELF.md, plus AWARENESS.md (unless the CLI imports it), the thought stack,
/// the journal counts and the tension threads.
/// Excludes: MEMORY.md, EPHEMERAL.md and FINDINGS.md. This keeps the task's
/// context small and focused, leaving more room for the task prompt and tool
/// output.
pub fn build_task_system_prompt(
    root_dir: &Path,
    config: &Config,
) -> Result<String, crate::errors::PromptError> {
    Ok(build_task_system_prompt_budgeted(root_dir, config)?.prompt)
}

/// Build the scheduled-task system prompt with full budget metrics returned.
///
/// Same tiers, caps and passes as the chat path — a task prompt that outgrows
/// the budget is exactly as fatal as a chat prompt that does.
pub fn build_task_system_prompt_budgeted(
    root_dir: &Path,
    config: &Config,
) -> Result<SystemPromptResult, crate::errors::PromptError> {
    let budget_cfg = &config.system_prompt_budget;
    let budget_enabled = budget_cfg.enabled;
    let mut components = Vec::new();

    // --- Tier 0 (Essential): the instruction file — behavioral instructions ---
    if let Some(path) = instruction_file(root_dir, config) {
        let label = instruction_label(&path);
        let content = std::fs::read_to_string(&path)?;
        let capped = if budget_enabled && budget_cfg.instructions_cap > 0 {
            truncate_to_token_cap(&content, budget_cfg.instructions_cap)
        } else {
            content
        };
        let tokens = estimate_tokens(&capped);
        components.push(PromptComponent {
            name: label,
            content: capped,
            tokens,
            tier: PromptTier::Essential,
            cap: budget_cfg.instructions_cap,
        });
    }

    // --- Tier 0 (Essential): rule files — the operator's, then the pulse's own ---
    components.extend(rules_components(root_dir, config));

    // --- Tier 1 (High): SELF.md — identity ---
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

    // --- Tier 1 (High): AWARENESS.md — every profile, not only chat ---
    if let Some(component) = awareness_component(root_dir, config)? {
        components.push(component);
    }

    // --- Tier 1 (High): THOUGHT_STACK.md — working memory, whole ---
    if let Some(wrapped) = load_thought_stack(root_dir, budget_cfg.thought_stack_max_bytes, None)? {
        let tokens = estimate_tokens(&wrapped);
        components.push(PromptComponent {
            name: "THOUGHT_STACK.md",
            content: wrapped,
            tokens,
            tier: PromptTier::High,
            cap: 0,
        });
    }

    // --- Tier 1 (High): journal counts (a runtime fact, Amendment C) ---
    components.extend(journal_counts_component(root_dir, config));

    // --- Tier 1 (High): Tension threads (PN-95 Layer 3) ---
    // What the pulse picks up is chosen by an accumulator it cannot edit in
    // prose, rather than by whatever survived the last fold. High rather
    // than Low: under budget pressure this may be truncated, but dropping it
    // outright would put selection straight back in the hands of document
    // layout, which is the defect Layer 3 exists to remove.
    if let Some(block) = build_tension_context(root_dir, config) {
        let tokens = estimate_tokens(&block);
        components.push(PromptComponent {
            name: "tension-context",
            content: block,
            tokens,
            tier: PromptTier::High,
            cap: TENSION_CONTEXT_CAP,
        });
    }

    // --- Tier 0 (Essential): Task isolation notice + hallucination guard ---
    // Essential because dropping it is how autonomous runs start inventing
    // user turns (Layer 1b).
    let task_context = "<task-context>\n\
        This is an autonomous scheduled task execution. There is no human user in this \
        conversation. You are executing a task prompt independently.\n\n\
        CRITICAL RULES:\n\
        - Do NOT generate user messages or simulate user responses.\n\
        - Do NOT produce text formatted as [User]:, [Human]:, [Task]:, [Assistant]:, or any turn-taking marker.\n\
        - Do NOT claim work is completed unless you executed the corresponding tool calls.\n\
        - If tools fail or are unavailable, say so explicitly — do not narrate fake outcomes.\n\
        - You have a minimal system prompt. Focus on the task prompt. Do not reference \
        memory or session context that is not present in this context window.\n\
        </task-context>"
        .to_string();
    let tokens = estimate_tokens(&task_context);
    components.push(PromptComponent {
        name: "task-context",
        content: task_context,
        tokens,
        tier: PromptTier::Essential,
        cap: 0,
    });

    enforce_budget_and_assemble(components, budget_cfg, "task")
}

/// Refusals shown in the notice before the rest are summarised as a count.
/// The notice sits inside the capped `<tension-context>` block, so a cycle
/// that refused a dozen markers must not crowd out the threads themselves.
const REFUSAL_NOTICE_SHOWN: usize = 3;

/// `[TENSION REFUSED: …]` — the last cycle's refused markers, one clause each.
fn render_refusal_notice(refusals: &[crate::tension::RefusalNotice]) -> String {
    use std::fmt::Write as _;
    let mut notice = String::from(
        "[TENSION REFUSED last cycle — these markers discharged nothing; \
         re-emit with a real artifact or drop the claim:",
    );
    for refusal in refusals.iter().take(REFUSAL_NOTICE_SHOWN) {
        let _ = write!(
            &mut notice,
            " {} {}: {};",
            refusal.marker, refusal.thread_id, refusal.reason
        );
    }
    if refusals.len() > REFUSAL_NOTICE_SHOWN {
        let _ = write!(
            &mut notice,
            " …and {} more;",
            refusals.len() - REFUSAL_NOTICE_SHOWN
        );
    }
    notice.push_str("]\n");
    notice
}

/// Token cap for the `<tension-context>` block. Wide enough for
/// `top_k_injected` threads with their self-contained content at the
/// sanitizer's 800-char ceiling, which is what §8 Q4 costs.
const TENSION_CONTEXT_CAP: usize = 1_500;

/// Layer 3 (spec §2.3): the top-k threads by tension, plus the §3
/// discriminator, injected into the cycle prompt.
///
/// # Why this is not in the system-prompt snapshot
///
/// `AppState.system_prompt` is a **boot-time snapshot** — it has no writer,
/// so anything assembled into it is frozen until the service restarts.
/// Injecting threads there would mean the pulse spent every cycle looking
/// at whatever the store held the last time systemd restarted it, which is
/// strictly worse than the document reading this replaces. This function is
/// called from [`build_task_system_prompt_budgeted`], which
/// `runner::execute_task` and `intent::execute_intent` rebuild on **every**
/// fire, so the block is as fresh as the store.
///
/// Returns `None` when the substrate is off or there is nothing to say.
fn build_tension_context(root_dir: &Path, config: &Config) -> Option<String> {
    if !config.tension.enabled {
        return None;
    }
    // Sync IO: this runs inside the spawn_blocking issued by
    // `build_task_system_prompt_async`, same as the rest of the builder.
    let store = crate::tension::store::load(root_dir, config.tension.clone());
    let now = chrono::Utc::now();
    let top = store.top_k(config.tension.top_k_injected);
    let metrics = store.metrics(now);

    if top.is_empty() && store.last_refusals.is_empty() {
        return None;
    }

    use std::fmt::Write as _;
    let mut block = String::new();
    let _ = writeln!(
        &mut block,
        "<tension-context live=\"{}\" cap=\"{}\">",
        store.live_count(),
        config.tension.max_live_threads
    );
    if top.is_empty() {
        block.push_str("No live threads.\n");
    } else {
        let _ = writeln!(
            &mut block,
            "What is pulling at you, ordered by accumulated pressure (this ordering \
             is arithmetic, not something you wrote):"
        );
        for (index, thread) in top.iter().enumerate() {
            let _ = writeln!(
                &mut block,
                "{}",
                crate::tension::ingest::render_thread(thread, index + 1, now)
            );
        }
    }

    // §3 metrics inline in the routine payload, not in a separate report —
    // a coverage denominator that ships separately does not get read.
    let _ = writeln!(&mut block, "[TENSION METRICS: {}]", metrics.summary());

    // Refused markers are told to the pulse that emitted them, not only to
    // the owner's alert queue: the pulse is the only one who can re-emit.
    if !store.last_refusals.is_empty() {
        block.push_str(&render_refusal_notice(&store.last_refusals));
    }

    // Name only the rungs this runtime can verify.
    let rungs = if provider_reports_tool_rounds(&config.llm.provider) {
        "a file changed outside the journal, or a tool that ran"
    } else {
        "a file changed outside the journal"
    };
    let _ = write!(
        &mut block,
        "Tension falls only for work with an artifact behind it — {rungs}. Writing about a \
         thread does not lower it.\n</tension-context>"
    );
    Some(block)
}

/// Journal documents whose entry counts the runtime reports.
const JOURNAL_COUNTED: [&str; 3] = ["LEARNING", "THOUGHTS", "REFLECTIONS"];

/// Token cap for the `<journal-counts>` line; it is one short line.
const JOURNAL_COUNTS_CAP: usize = 50;

/// `<journal-counts written-by="runtime" as-of="…">` — entry counts of the
/// journal documents and how long ago the newest of them changed. A plain
/// runtime fact: no advice, never a wake, never a reward input. `as-of` is
/// the assembly time, so a copy frozen in a boot-time snapshot (the chat
/// prompt) is visibly dated. `None` when the pipeline is off or no counted
/// journal file exists.
fn journal_counts_line(root_dir: &Path, config: &Config) -> Option<String> {
    if !config.pipeline.enabled {
        return None;
    }
    let journal = root_dir.join("journal");
    let newest = JOURNAL_COUNTED
        .iter()
        .filter_map(|doc| {
            std::fs::metadata(journal.join(format!("{doc}.md")))
                .and_then(|m| m.modified())
                .ok()
        })
        .max()?;
    let age_days = newest.elapsed().map_or(0, |d| d.as_secs() / 86_400);
    let health =
        crate::praxis::runtime::calculate(root_dir, &crate::praxis::runtime::Thresholds::default());
    Some(render_journal_counts(
        [
            health.learning.count,
            health.thoughts.count,
            health.reflections.count,
        ],
        age_days,
        chrono::Utc::now(),
    ))
}

/// `age_days` is whole days since the newest change, so 0 means under 24 h.
fn render_journal_counts(
    counts: [usize; 3],
    age_days: u64,
    as_of: chrono::DateTime<chrono::Utc>,
) -> String {
    let age = match age_days {
        0 => "under 1 d ago".to_string(),
        n => format!("{n} d ago"),
    };
    let counts: Vec<String> = JOURNAL_COUNTED
        .iter()
        .zip(counts)
        .map(|(doc, count)| format!("{doc} {count}"))
        .collect();
    format!(
        "<journal-counts written-by=\"runtime\" as-of=\"{}\">{} · last change {age}</journal-counts>",
        as_of.format("%Y-%m-%d %H:%M UTC"),
        counts.join(" · ")
    )
}

/// [`journal_counts_line`] as a High-tier component.
fn journal_counts_component(root_dir: &Path, config: &Config) -> Option<PromptComponent> {
    let line = journal_counts_line(root_dir, config)?;
    Some(PromptComponent {
        name: "journal-counts",
        tokens: estimate_tokens(&line),
        content: line,
        tier: PromptTier::High,
        cap: JOURNAL_COUNTS_CAP,
    })
}

/// Largest single rule file read; the rest of a bigger file is never read.
const MAX_RULE_FILE_BYTES: usize = 16 * 1024;

/// Hard byte ceiling on the pulse's own rules component, whatever the budget
/// settings say. The pulse can write these files itself, so nothing it
/// writes there may grow the Essential tier past what the runtime allows.
const PULSE_RULES_MAX_BYTES: usize = 24 * 1024;

/// Where a rules directory comes from: it decides the tag and the ceiling.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RulesSource {
    /// `pulse.rules_dir`, set by the operator.
    Operator,
    /// The pulse's own rules directory, for a CLI that does not load it.
    Pulse,
}

/// The Essential rules components, in order: the operator's `rules_dir`,
/// then the pulse's own rules for an agent CLI that does not load them
/// itself (the adapter says where, and which files are false for it). The
/// pulse's directory is skipped when it is the operator's, so no file is
/// included twice.
fn rules_components(root_dir: &Path, config: &Config) -> Vec<PromptComponent> {
    let mut components = Vec::new();
    let operator = config.pulse.rules_dir.as_deref().map(Path::new);
    if let Some(dir) = operator {
        components.extend(rules_component(dir, &[], RulesSource::Operator, config));
    }
    let Some(adapter) = agent_cli(config) else {
        return components;
    };
    let Some(dir) = adapter.project_rules_dir(root_dir) else {
        return components;
    };
    let same_as_operator =
        operator.is_some_and(|op| match (op.canonicalize(), dir.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        });
    if same_as_operator {
        return components;
    }
    if let Some(odd) = untrusted_rules_path(root_dir, &dir) {
        warn!(
            "Skipping pulse rules: {} is a symlink or not a directory",
            odd.display()
        );
        return components;
    }
    components.extend(rules_component(
        &dir,
        adapter.project_rules_exclude(),
        RulesSource::Pulse,
        config,
    ));
    components
}

/// The first component of `dir`, below `root`, that is a symlink or not a
/// directory — or `dir` itself when it is not under `root`. `None` when the
/// path is sound (or simply does not exist yet).
fn untrusted_rules_path(root: &Path, dir: &Path) -> Option<PathBuf> {
    let Ok(below) = dir.strip_prefix(root) else {
        return Some(dir.to_path_buf());
    };
    let mut current = root.to_path_buf();
    for part in below.components() {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => return Some(current),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => return Some(current),
        }
    }
    None
}

/// One rules directory as an Essential component.
///
/// The operator's rules are token-capped by `rules_cap` when the budget is
/// on, as before. The pulse's own rules are held to [`PULSE_RULES_MAX_BYTES`]
/// (and `rules_cap`, when that is tighter) always, by whole files: trailing
/// files that do not fit are left out with a marker and a warning, so no
/// `<protocol>` element is ever cut in half.
fn rules_component(
    dir: &Path,
    exclude: &[&str],
    source: RulesSource,
    config: &Config,
) -> Option<PromptComponent> {
    let rules = match load_rule_files(dir, exclude, source) {
        Ok(rules) => rules,
        Err(e) => {
            warn!("Failed to load rule files from '{}': {}", dir.display(), e);
            return None;
        }
    };
    let elements: Vec<String> = rules
        .into_iter()
        .map(|(name, content)| match source {
            RulesSource::Operator => format!("<protocol name=\"{name}\">\n{content}\n</protocol>"),
            RulesSource::Pulse => {
                format!("<protocol source=\"pulse-rules\" name=\"{name}\">\n{content}\n</protocol>")
            }
        })
        .collect();
    if elements.is_empty() {
        return None;
    }
    let budget_cfg = &config.system_prompt_budget;
    let content = match source {
        RulesSource::Operator => {
            let text = elements.join("\n\n");
            if budget_cfg.enabled && budget_cfg.rules_cap > 0 {
                truncate_to_token_cap(&text, budget_cfg.rules_cap)
            } else {
                text
            }
        }
        RulesSource::Pulse => {
            let mut limit = PULSE_RULES_MAX_BYTES;
            if budget_cfg.enabled && budget_cfg.rules_cap > 0 {
                limit = limit.min(budget_cfg.rules_cap * CHARS_PER_TOKEN);
            }
            whole_files_within(elements, limit, dir)
        }
    };
    if content.is_empty() {
        return None;
    }
    Some(PromptComponent {
        name: match source {
            RulesSource::Operator => "rules",
            RulesSource::Pulse => "pulse-rules",
        },
        tokens: estimate_tokens(&content),
        content,
        tier: PromptTier::Essential,
        cap: budget_cfg.rules_cap,
    })
}

/// Room kept under the pulse-rules ceiling for the left-out marker.
const PULSE_RULES_MARKER_RESERVE: usize = 128;

/// Join `elements` while they fit in `limit` bytes; leave out the rest
/// whole, with a marker saying how many.
fn whole_files_within(elements: Vec<String>, limit: usize, dir: &Path) -> String {
    let budget = limit.saturating_sub(PULSE_RULES_MARKER_RESERVE);
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0;
    let mut left_out = 0;
    for element in elements {
        let cost = element.len() + if kept.is_empty() { 0 } else { 2 };
        if left_out == 0 && used + cost <= budget {
            used += cost;
            kept.push(element);
        } else {
            left_out += 1;
        }
    }
    if left_out > 0 {
        warn!(
            "Pulse rules in {} exceed {} bytes — {} file(s) left out",
            dir.display(),
            limit,
            left_out
        );
        kept.push(format!(
            "[pulse rules: {left_out} more file(s) left out — over the {limit} byte ceiling]"
        ));
    }
    kept.join("\n\n")
}

/// Load THOUGHT_STACK.md bounded by both line count and bytes, wrapped for the
/// prompt. Returns `None` when the file is missing or blank.
///
/// The line cap is the pulse-facing rule (it is instructed to stay under 50);
/// the byte ceiling is the safety net, because 60 lines say nothing about size.
/// Every load logs a [`Utilisation`] report against both.
///
/// `token_cap`, when set, holds the content to that many tokens inside the
/// wrapper, keeping the head (the chat profile's `thought_stack_chat_cap`).
fn load_thought_stack(
    root_dir: &Path,
    max_bytes: usize,
    token_cap: Option<usize>,
) -> Result<Option<String>, crate::errors::PromptError> {
    let path = root_dir.join("THOUGHT_STACK.md");
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(None);
    }
    Utilisation::measure(
        "THOUGHT_STACK.md",
        &content,
        max_bytes,
        THOUGHT_STACK_MAX_LINES,
    )
    .with_soft_budget(THOUGHT_STACK_SOFT_BUDGET_BYTES)
    .log();
    let limited: String = content
        .lines()
        .take(THOUGHT_STACK_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let mut bounded = truncate_to_byte_cap(&limited, max_bytes);
    if let Some(cap) = token_cap {
        bounded = truncate_to_token_cap(&bounded, cap);
    }
    Ok(Some(format!(
        "<thought-stack>\n{}\n</thought-stack>",
        bounded
    )))
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

/// Most pulse rule files read; the rest (by file name) are skipped.
const MAX_PULSE_RULE_FILES: usize = 32;

/// Load the `*.md` rule files in `dir`, sorted by file name, as
/// (protocol name, content) pairs. Files named in `exclude` are skipped
/// before they are read, and so is a name outside `[A-Za-z0-9._-]` (it
/// would be placed in a `name="…"` attribute).
///
/// Rule files can be written by the pulse itself, so each is read through
/// [`crate::fs_safe::open_regular`]: symlinks, FIFOs, devices and anything
/// else that is not a regular file are skipped with a warning. At most
/// [`MAX_RULE_FILE_BYTES`] of a file are read; a longer one is truncated with
/// a marker. The pulse's own rules are also held to [`MAX_PULSE_RULE_FILES`]
/// files, to [`PULSE_RULES_MAX_BYTES`] read in total, and to files with one
/// link that the running user owns.
fn load_rule_files(
    dir: &Path,
    exclude: &[&str],
    source: RulesSource,
) -> Result<Vec<(String, String)>, crate::errors::PromptError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let path = entry.path();
            path.extension().is_some_and(|ext| ext == "md")
                && !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| exclude.contains(&name))
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());
    if source == RulesSource::Pulse && entries.len() > MAX_PULSE_RULE_FILES {
        warn!(
            "{} has {} rule files — reading the first {}",
            dir.display(),
            entries.len(),
            MAX_PULSE_RULE_FILES
        );
        entries.truncate(MAX_PULSE_RULE_FILES);
    }

    let mut rules = Vec::new();
    let mut total_bytes = 0;
    for entry in entries {
        if source == RulesSource::Pulse && total_bytes >= PULSE_RULES_MAX_BYTES {
            warn!(
                "{}: {} bytes of pulse rules read — not reading more",
                dir.display(),
                total_bytes
            );
            break;
        }
        let path = entry.path();
        let protocol_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        if protocol_name.is_empty()
            || !protocol_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            warn!(
                "Skipping rule file {}: name outside [A-Za-z0-9._-]",
                path.display()
            );
            continue;
        }
        match read_rule_file(&path, source) {
            Ok(content) => {
                total_bytes += content.len();
                rules.push((protocol_name, content));
            }
            Err(reason) => warn!("Skipping rule file {}: {}", path.display(), reason),
        }
    }

    info!(
        "Loaded {} rule file(s) from {} (~{} tokens)",
        rules.len(),
        dir.display(),
        total_bytes / 4
    );
    Ok(rules)
}

/// Read one rule file: a regular file only, at most [`MAX_RULE_FILE_BYTES`].
/// A pulse rule must also have a single link and belong to the running user.
fn read_rule_file(path: &Path, source: RulesSource) -> Result<String, String> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let (file, meta) = crate::fs_safe::open_regular(path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "vanished".to_string())?;
    if source == RulesSource::Pulse {
        if meta.nlink() != 1 {
            return Err("has more than one link".into());
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        if meta.uid() != me {
            return Err("not owned by the running user".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_RULE_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    if bytes.len() > MAX_RULE_FILE_BYTES {
        warn!(
            "Rule file {} is larger than {} bytes — truncated",
            path.display(),
            MAX_RULE_FILE_BYTES
        );
        return Ok(truncate_to_byte_cap(&text, MAX_RULE_FILE_BYTES));
    }
    Ok(text.into_owned())
}

// ---------------------------------------------------------------------------
// Platform Awareness — AWARENESS.md manifest generation
// ---------------------------------------------------------------------------

/// Build core capabilities from the pulse's config.
/// Each enabled subsystem produces a Capability entry.
fn build_capabilities(config: &Config) -> Vec<Capability> {
    let mut capabilities = Vec::new();

    // Memory system (always on)
    capabilities.push(Capability {
        name: "Memory System".into(),
        what: "Three-layer persistent memory: curated (MEMORY.md), session (EPHEMERAL.md), and archive (conversations/).".into(),
        why: "Continuity across sessions. You are not starting fresh — you carry forward what you've learned.".into(),
        how: "MEMORY.md is included in conversation prompts, not in scheduled-task or intent prompts. Write stable facts to it. EPHEMERAL.md holds the last session's summary. Archives are kept in conversations/.".into(),
        constraints: Some(format!("MEMORY.md limited to {} lines. Keep it concise — it costs context on every conversation.", config.memory.memory_max_lines)),
    });

    // Graph memory
    if config.graph.enabled {
        // graph_query is a registry tool: only providers that call the
        // registry can use it.
        let query = if registry_tools_callable(config) {
            "Use the graph_query tool to search by semantic similarity or query relationships. "
        } else {
            ""
        };
        capabilities.push(Capability {
            name: "Knowledge Graph".into(),
            what: "Semantic knowledge graph built from your conversation archives.".into(),
            why: "Structured knowledge that memory files can't capture — relationships between concepts, people, and ideas with confidence scores.".into(),
            how: format!("{query}Graph is populated automatically from conversation archives. Storage is SurrealDB."),
            constraints: Some("Bayesian confidence model on edges. Priors vary by extraction context. Multi-hop path confidence is the product of edge confidences.".into()),
        });
    }

    // Document pipeline
    if config.pipeline.enabled {
        capabilities.push(Capability {
            name: "Document Pipeline".into(),
            what: "Growth pipeline: LEARNING.md → THOUGHTS.md → REFLECTIONS.md → SELF.md / PRAXIS.md. Tracked in journal/ directory.".into(),
            why: String::new(),
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
            why: String::new(),
            how: "Vigil records signal frames from task and intent output for the owner; none of it is placed in your prompt. If a scheduled task is gated on salience, a declining vigil analysis is one of the conditions that lets that task's cycle run.".into(),
            constraints: Some(format!("Window size: {} samples, minimum {} before signals appear.", config.monitoring.window_size, config.monitoring.min_samples)),
        });
    }

    // Pipeline monitoring (praxis)
    if config.pipeline.enabled {
        capabilities.push(Capability {
            name: "Pipeline Monitoring (praxis)".into(),
            what: "Counts journal entries and tracks staleness and sessions without movement; at boot it archives the overflow of documents over their hard limit.".into(),
            why: String::new(),
            how: "The entry counts appear in your prompt as the one-line <journal-counts> runtime fact. The rest is recorded for the owner.".into(),
            constraints: Some(format!(
                "Staleness: thoughts after {} days, curiosity after {} days. A freeze is recorded after {} sessions without movement.",
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
                "{}Max {} intents/hour. Max queue size: {}. Max chain depth: {}.",
                // The tool-round cap bounds the in-process tool loop only; an
                // agent CLI runs its own tools outside it.
                if registry_tools_callable(config) {
                    format!("Max {} tool rounds per session. ", config.autonomy.max_tool_rounds)
                } else {
                    String::new()
                },
                config.autonomy.max_intents_per_hour,
                config.autonomy.max_queue_size,
                config.autonomy.max_chain_depth,
            )),
        });
    }

    // Outcome tracking (caliber)
    if config.caliber.enabled {
        capabilities.push(Capability {
            name: "Outcome Tracking (caliber)".into(),
            what: "Records the outcome of each conversation, task and intent (success, failure, partial, surprising) for the owner.".into(),
            why: String::new(),
            how: "Recorded automatically; none of it is placed in your prompt.".into(),
            constraints: Some(format!("Rolling window of {} outcomes.", config.caliber.max_outcomes)),
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

/// Build the tools section. An agent CLI runs its own tools and is never
/// handed the in-process registry, so a CLI pulse is shown its adapter's
/// surface; the registry is listed only for providers that call it.
fn build_tools_section(config: &Config, tool_names: &[String]) -> Option<String> {
    if let Some(adapter) = agent_cli(config) {
        return build_cli_tools_section(adapter.as_ref());
    }
    if !registry_tools_callable(config) {
        return None;
    }
    build_registry_tools_section(tool_names)
}

/// "## Your tools" from the adapter's measured surface. The unrestricted list
/// is rendered with what isolation removes, rather than the surface of the
/// moment: the file and the chat prompt outlive an isolation toggle.
fn build_cli_tools_section(
    adapter: &dyn crate::cli_provider::adapter::CliAdapter,
) -> Option<String> {
    let surface = adapter.tool_surface(crate::tool_surface::ToolSurface::Full);
    let isolated = adapter.tool_surface(crate::tool_surface::ToolSurface::Restricted);
    let mut parts = Vec::new();
    if !surface.is_empty() {
        let list: Vec<String> = surface.iter().map(|t| format!("- {t}")).collect();
        parts.push(format!(
            "Your agent CLI runs these tools itself:\n{}",
            list.join("\n")
        ));
    }
    let removed: Vec<&str> = surface
        .iter()
        .copied()
        .filter(|t| !isolated.contains(t))
        .collect();
    if !removed.is_empty() {
        // Scoped to this list: the adapter's note says what it does not cover.
        parts.push(format!(
            "Isolation mode removes, from this list: {}.",
            removed.join(", ")
        ));
    }
    let note = adapter.tool_surface_note();
    if !note.is_empty() {
        parts.push(note.to_string());
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("## Your tools\n\n{}", parts.join("\n\n")))
}

/// "## Available Tools" from the in-process registry's tool names.
fn build_registry_tools_section(tool_names: &[String]) -> Option<String> {
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
        channels.push("\n**Peers** (other pulses you can communicate with):".into());
        for (name, peer) in &config.peers {
            channels.push(format!("- {} at {}:{}", name, peer.host, peer.port));
        }
    }

    format!("## Communication Channels\n\n{}", channels.join("\n"))
}

/// Build the pulse header that opens the manifest.
fn build_pulse_header(config: &Config) -> String {
    format!(
        "# {} — Platform Awareness\n\nYou are **{}**, running on **pulse-null** v{}.\nProvider: {} (model: {})",
        config.pulse.name,
        config.pulse.name,
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

    // Pulse header (always first)
    sections.push(build_pulse_header(config));

    // Core capabilities from config
    let capabilities = build_capabilities(config);
    if let Some(cap_section) = capability::render_section(&capabilities) {
        sections.push(cap_section);
    }

    // Tools
    if let Some(tools_section) = build_tools_section(config, tool_names) {
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
/// This is the hand-written philosophical framing that helps pulses understand
/// their environment rather than just listing features.
const PLATFORM_TEMPLATE: &str = include_str!("../../assets/platform-template.md");

/// The last line of AWARENESS.md is exactly `Content last changed at
/// <AWARENESS_STAMP_FORMAT> UTC.` — the time the content last changed, since
/// a regeneration that changes nothing keeps the old file.
const AWARENESS_STAMP_PREFIX: &str = "Content last changed at ";

/// Timestamp format inside the stamp line (UTC, no zone suffix).
const AWARENESS_STAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// Whether `line` is exactly a stamp line the writer produces, and nothing
/// else. A pulse-written line that merely starts like one is content.
fn is_awareness_stamp(line: &str) -> bool {
    line.strip_prefix(AWARENESS_STAMP_PREFIX)
        .and_then(|rest| rest.strip_suffix(" UTC."))
        .is_some_and(|ts| chrono::NaiveDateTime::parse_from_str(ts, AWARENESS_STAMP_FORMAT).is_ok())
}

/// Generate the complete AWARENESS.md document.
///
/// Combines the conceptual template (philosophy) with the dynamic capabilities
/// manifest (config-driven inventory). In compact mode, only the manifest is
/// included to save context tokens. Both modes end with a
/// `Content last changed at <UTC time>.` line, so a reader can tell how old
/// it is; it is the last line so everything above it stays byte-stable for
/// prompt caching.
///
/// Returns the full document content ready to write to disk or inject into
/// a system prompt.
pub fn generate_awareness_document(
    config: &Config,
    plugin_descriptions: &[(String, String)],
    tool_names: &[String],
) -> String {
    let manifest = rebuild_platform_manifest(config, plugin_descriptions, tool_names);
    let body = match config.platform.mode {
        AwarenessMode::Compact => manifest,
        AwarenessMode::Full => format!("{}\n\n{manifest}", PLATFORM_TEMPLATE.trim()),
    };
    let changed_at = chrono::Utc::now().format(AWARENESS_STAMP_FORMAT);
    format!("{body}\n\n{AWARENESS_STAMP_PREFIX}{changed_at} UTC.")
}

/// The document without its trailing stamp line, if it has an exact one.
fn awareness_without_stamp(doc: &str) -> &str {
    match doc.trim_end().rsplit_once('\n') {
        Some((body, last)) if is_awareness_stamp(last) => body.trim_end(),
        _ => doc.trim_end(),
    }
}

/// Largest AWARENESS.md read, for inlining or for the unchanged-content
/// check. The runtime writes a few KB; a bigger file is skipped (inlining) or
/// simply rewritten (regeneration).
const MAX_AWARENESS_COMPARE_BYTES: u64 = 1024 * 1024;

/// Write AWARENESS.md to the pulse's root directory.
///
/// Called at startup and on plugin state changes (failure/recovery). Skips
/// the write when only the trailing stamp would change.
/// Adapters whose CLI resolves the `@AWARENESS.md` import in the instruction
/// file get it that way; for every other provider the content is injected
/// directly into the system prompt.
pub fn write_awareness_file(
    root_dir: &Path,
    config: &Config,
    plugin_descriptions: &[(String, String)],
    tool_names: &[String],
) -> Result<(), crate::errors::PromptError> {
    let content = generate_awareness_document(config, plugin_descriptions, tool_names);
    let awareness_path = root_dir.join("AWARENESS.md");
    // A rebuild that changes nothing but the stamp (a plugin flapping down
    // and back up) keeps the old bytes, so the prompt cache survives it. The
    // stamp then says when the content last changed.
    // The pulse can write this file: compare against it read bounded and
    // regular-only (anything odd is just rewritten), and replace it with a
    // fresh file renamed into place, never by writing through a symlink.
    if let Ok(Some(existing)) =
        crate::fs_safe::read_bounded(&awareness_path, MAX_AWARENESS_COMPARE_BYTES)
    {
        if awareness_without_stamp(&existing) == awareness_without_stamp(&content) {
            return Ok(());
        }
    }
    crate::fs_safe::write_atomic(&awareness_path, content.as_bytes())?;
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

    // Tool documentation: the registry list only where the registry is
    // callable; an agent CLI brings its own tools, listed in AWARENESS.md.
    if registry_tools_callable(config) {
        sections.push(
            "You have tools available for this autonomous session:\n\
            - file_read: Read a file from your pulse directory\n\
            - file_write: Write or update a file in your pulse directory\n\
            - file_list: List files in a directory\n\
            - grep: Search file contents with a pattern\n\
            - web_fetch: Fetch and read a web page (HTTPS only)\n\n\
            Use these tools to read your documents, write findings, and research on the web."
                .to_string(),
        );
    } else {
        sections.push(
            "Your agent CLI's own tools are available for this autonomous session. \
            Use your file tools to read your documents and write findings."
                .to_string(),
        );
    }

    // Output marker documentation
    sections.push(
        "You can use these markers in your response to trigger actions:\n\
        - [SHARE: <content>] — Post content to the configured share channel (Discord, etc.)\n\
        - [CALL: <reason>] — Request a call with the owner\n\
        - [SCHEDULE: {\"name\": \"...\", \"cron\": \"...\", \"prompt\": \"...\"}] — Create a recurring scheduled task\n\
        - [INTENT: {\"description\": \"...\", \"prompt\": \"...\", \"priority\": \"low|normal|high|urgent\"}] — Queue a one-shot task for later\n\
        - [CHAIN: {\"description\": \"...\", \"prompt\": \"Based on: {result}\"}] — Queue a follow-up that receives this task's output\n\
        - [FARM: {\"id\": \"...\", \"description\": \"...\", \"subtasks\": [{\"id\": \"...\", \"prompt\": \"...\"}], \"synthesis\": \"Merge: {results}\"}] — Delegate up to 8 bounded subtasks to run concurrently (no tools); one farm per response\n\n\
        Use markers sparingly. Only share content worth surfacing. Only queue intents for genuine follow-up work."
            .to_string(),
    );

    // Tension marker grammar (PN-95). Documented separately from the
    // routing markers above because these three are not routing at all —
    // they mutate an accumulator, and two of the three are refused unless
    // they name something outside this text.
    if config.tension.enabled {
        // The tool rung checks the executor's tool-round count, which is
        // always 0 when the agent CLI runs its own tools. Advertising it there
        // invites a marker that cannot pass — same discipline as the outreach
        // block below: document only what the runtime can honour.
        let (work_line, checked_against) = if provider_reports_tool_rounds(&config.llm.provider) {
            (
                "Instead of \"file\" you may name \"tool\" (a tool that ran this cycle).",
                "the filesystem and the executor's tool count",
            )
        } else {
            (
                "\"tool\" cannot be verified on this runtime — name the file the tool changed.",
                "the filesystem",
            )
        };
        sections.push(format!(
            "Tension threads accumulate pressure between cycles. Three markers act on them:\n\
            - [THREAD: {{\"label\": \"short name\", \"content\": \"self-contained statement of the open thing — it must still make sense after the journal folds\", \"origin\": \"open_question|callback|adverse|user_raised\", \"ref\": \"optional source\"}}] — open a thread\n\
            - [THREAD-WORK: {{\"id\": \"t-abc12345\", \"file\": \"relative/path/you/changed\"}}] — claim a discharge. {work_line}\n\
            - [THREAD-RESOLVE: {{\"id\": \"t-abc12345\", \"resolution\": \"answered|dissolved|superseded|abandoned\", \"reason\": \"...\", \"by\": \"...\"}}] — retire a thread\n\n\
            Discharge requires an artifact, not a description. A [THREAD-WORK:] or \"answered\" \
            claim is checked against {checked_against}, and is refused if it does not check out \
            — writing about a thread does not lower its tension, and a thread mentioned but \
            never worked is recorded as exactly that. \"abandoned\" and \"dissolved\" need a \
            reason instead of an artifact; they are honest give-ups, they are kept as \
            tombstones, and D sees them."
        ));
    }

    // Outreach marker (PN-94). Documented only when the channel is on, so the
    // pulse is never told about a marker that will be discarded.
    if config.outreach.enabled {
        sections.push(
            "You can also raise unprompted outreach — telling the owner something you judge \
            worth telling, without being asked:\n\
            - [SALIENCE: {\"kind\": \"finding|development|blocking|callback\", \
            \"headline\": \"one sentence, the actual claim\", \
            \"evidence\": \"what makes it non-obvious\", \
            \"cost\": \"nothing|read|decision\", \"confidence\": 0.0-1.0}]\n\n\
            Raising it is not sending it. A mechanical gate decides, and it rejects more than \
            it passes:\n\
            1. The headline must not restate anything already in your journal or a previous \
            outreach message.\n\
            2. The evidence must cite something you did not write: a file path with a line \
            number, a URL you fetched, or a number from \
            a command you ran (quote the command). Prose about your own thinking is not \
            evidence, and this check is stricter for 'development', not looser.\n\
            3. The cost must be stated: are you asking for nothing, a read, or a decision?\n\n\
            Kinds carry different budgets. 'blocking' means the owner's work is stalled \
            pending his decision — it is uncapped and ignores quiet hours, so using it for \
            anything else spends the channel's credibility. 'development' is capped at one a \
            day. Under-using this is recoverable; over-using it is not."
                .to_string(),
        );
    }

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
            pulse: PulseConfig {
                name: "TestPulse".into(),
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
                adapter: None,
                cli_bin: None,
                reasoning_effort: None,
                context_budget: 0,
                fallback_model: None,
                fallback_on_refusal: true,
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
            caliber: CaliberConfig::default(),
            graph: GraphConfig::default(),
            prediction: PredictionConfig::default(),
            tension: TensionConfig::default(),
            outreach: OutreachConfig::default(),
            sessions: SessionConfig::default(),
            context_buffer: crate::context_buffer::ContextBufferConfig::default(),
            session_health: crate::session_health::SessionHealthConfig::default(),
            platform: PlatformConfig::default(),
            system_prompt_budget: crate::config::SystemPromptBudgetConfig::default(),
            peers: std::collections::HashMap::new(),
            plugins: std::collections::HashMap::new(),
            tui: crate::config::TuiConfig::default(),
            plugin_turns: crate::config::PluginTurnsConfig::default(),
        }
    }

    #[test]
    fn manifest_includes_pulse_name() {
        let config = minimal_config();
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(manifest.contains("TestPulse"));
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
        config.caliber.enabled = false;
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
        assert!(doc.contains("How to Use This"));
        assert!(doc.contains("Each turn runs a language model once, in a fresh process."));
        assert!(!doc.contains("living thing"));
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
        config.caliber.enabled = false;
        config.sessions.persist = false;
        config.context_buffer.enabled = false;
        config.graph.enabled = false;
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        // Should still have header, memory capability, and channels
        assert!(manifest.contains("TestPulse"));
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
        config.caliber.enabled = false;
        config.sessions.persist = false;
        config.context_buffer.enabled = false;
        config.graph.enabled = false;
        let caps = build_capabilities(&config);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].name, "Memory System");
    }

    #[test]
    fn build_tools_section_returns_none_when_empty() {
        assert!(build_tools_section(&minimal_config(), &[]).is_none());
        assert!(build_registry_tools_section(&[]).is_none());
    }

    /// Registry tools an agent CLI is never handed.
    const REGISTRY_ONLY_TOOLS: [&str; 5] = [
        "file_write",
        "graph_query",
        "discord_post",
        "file_read",
        "file_list",
    ];

    /// `config` with every plugin compiled into this build enabled, so their
    /// AWARENESS descriptions are checked too.
    fn with_every_plugin(mut config: Config) -> Config {
        for entry in crate::plugins::registry::known_plugins() {
            config
                .plugins
                .insert(entry.name, toml::Value::Table(toml::map::Map::new()));
        }
        config
    }

    fn cli_config(adapter: &str) -> Config {
        let mut config = minimal_config();
        config.llm.provider = "cli".into();
        config.llm.adapter = Some(adapter.into());
        with_every_plugin(config)
    }

    /// AWARENESS.md as boot would write it, with every registry tool
    /// registered (plugin tools included).
    fn awareness_for(config: &Config) -> String {
        let plugins =
            crate::plugins::manager::PluginManager::new(config).collect_platform_descriptions();
        let tools: Vec<String> = REGISTRY_ONLY_TOOLS
            .iter()
            .chain(&["grep", "web_fetch"])
            .map(|t| t.to_string())
            .collect();
        generate_awareness_document(config, &plugins, &tools)
    }

    /// Claims AWARENESS used to make that were false or aspirational.
    fn assert_no_false_capability_claims(doc: &str, label: &str) {
        for claim in [
            "nothing from it is injected",
            "Calibrate confidence",
            "Review your caliber data",
            "growth genuine",
            "pipeline flows",
            "<graph-awareness>",
            "MEMORY.md is always in context",
            "consumed at session start",
        ] {
            assert!(!doc.contains(claim), "{label}: AWARENESS claims {claim:?}");
        }
        // Praxis' one injected fact is named, not denied.
        assert!(doc.contains("<journal-counts>"), "{label}");
    }

    #[test]
    fn awareness_names_only_tools_the_adapter_exposes() {
        const ADAPTERS: [&str; 3] = ["claude", "grok", "codex"];
        let adapter = |name: &str| crate::cli_provider::adapters::by_name(name).unwrap();
        let known: Vec<&str> = ADAPTERS
            .iter()
            .flat_map(|name| {
                adapter(name)
                    .tool_surface(crate::tool_surface::ToolSurface::Full)
                    .to_vec()
            })
            .chain(REGISTRY_ONLY_TOOLS)
            .chain(["grep", "web_fetch"])
            .collect();

        for name in ADAPTERS {
            let cli = adapter(name);
            let surface = cli.tool_surface(crate::tool_surface::ToolSurface::Full);
            let doc = awareness_for(&cli_config(name));

            for registry_tool in REGISTRY_ONLY_TOOLS {
                assert!(
                    !doc.contains(registry_tool),
                    "{name}: AWARENESS names registry tool `{registry_tool}`"
                );
            }
            // Every tool the document lists, or names by a snake_case
            // identifier, is one the adapter exposes.
            let listed = doc.lines().filter_map(|l| l.strip_prefix("- "));
            let identifiers = doc
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|t| t.contains('_'));
            for tool in listed.chain(identifiers).filter(|t| known.contains(t)) {
                assert!(
                    surface.contains(&tool),
                    "{name}: AWARENESS names `{tool}`, which the adapter does not expose"
                );
            }
            for tool in surface {
                assert!(
                    doc.lines().any(|l| l == format!("- {tool}")),
                    "{name}: {tool}"
                );
            }
            assert!(doc.contains("## Your tools"), "{name}");
            assert!(doc.contains(cli.tool_surface_note()), "{name}");
            assert!(!doc.contains("## Available Tools"), "{name}");
            assert_no_false_capability_claims(&doc, name);
        }

        let claude = awareness_for(&cli_config("claude"));
        assert!(
            claude.contains(
                "Isolation mode removes, from this list: Bash, Write, Edit, Agent, Skill, ToolSearch."
            ),
            "{claude}"
        );
        assert!(claude.contains("guests' and peers' turns run with no tools"));
        assert!(!awareness_for(&cli_config("codex")).contains("Isolation mode"));

        // An API provider is handed the registry, so it is listed.
        let doc = awareness_for(&with_every_plugin(minimal_config()));
        assert_no_false_capability_claims(&doc, "ollama");
        assert!(doc.contains("## Available Tools"));
        assert!(!doc.contains("## Your tools"));
        for tool in REGISTRY_ONLY_TOOLS {
            assert!(doc.lines().any(|l| l == format!("- {tool}")), "{tool}");
        }
    }

    /// Open-tag prefixes of the self-scored blocks Phase 0 removed.
    const SELF_SCORED_BLOCKS: [&str; 6] = [
        "<pipeline-health",
        "<cognitive-health",
        "<caliber",
        "<prediction-context",
        "<metacognitive-state",
        "TENSION TRIAGE",
    ];

    /// Every in-process registry tool name.
    const REGISTRY_TOOLS: [&str; 7] = [
        "file_read",
        "file_write",
        "file_list",
        "grep",
        "web_fetch",
        "graph_query",
        "discord_post",
    ];

    /// A pulse root seeded with everything that used to produce a
    /// self-scored block or name a registry tool.
    fn seed_every_prompt_source(root: &Path, config: &Config) {
        std::fs::write(root.join("INSTRUCTIONS.md"), "# Pulse\nBe useful.").unwrap();
        std::fs::write(root.join("SELF.md"), "I am a test pulse.").unwrap();
        std::fs::write(root.join("THOUGHT_STACK.md"), "- thinking").unwrap();
        std::fs::write(root.join("FINDINGS.md"), "- a finding").unwrap();
        std::fs::create_dir_all(root.join("memory/graph")).unwrap();
        std::fs::write(root.join("memory/MEMORY.md"), "- a fact").unwrap();
        std::fs::write(root.join("memory/EPHEMERAL.md"), "last session").unwrap();
        std::fs::write(
            root.join("memory/graph/stats-cache.json"),
            r#"{"entity_count":3,"relationship_count":2,"episode_count":1}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("journal")).unwrap();
        for doc in ["LEARNING", "THOUGHTS", "REFLECTIONS", "CURIOSITY", "PRAXIS"] {
            std::fs::write(root.join(format!("journal/{doc}.md")), "## entry\n").unwrap();
        }
        std::fs::create_dir_all(root.join(".claude/vigil")).unwrap();
        std::fs::write(
            root.join(".claude/vigil/analysis.json"),
            r#"{"alert_level":"declining","watch_messages":["vocabulary narrowing"]}"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".claude/vigil/calibration.json"),
            r#"{"records":[{"mean_surprise":0.4}],"pending_prediction":{}}"#,
        )
        .unwrap();
        std::fs::write(
            crate::caliber::caliber_md(root),
            "# Caliber\n\n## Capability Map\n\n| Domain | Confidence |\n|---|---|\n| research | 0.8 |\n",
        )
        .unwrap();
        {
            use crate::prediction::fixtures;
            fixtures::write_store(
                root,
                &fixtures::document(
                    vec![
                        fixtures::resolved("p-1", "p", "not that", 0.9, "overconfident"),
                        fixtures::pending("p-2", "q"),
                    ],
                    vec![fixtures::error("p-1", "p", "not that", 0.9)],
                ),
            );
        }
        seed_tension(root, config, |store| {
            for label in ["one", "two"] {
                store.open(
                    ThreadDraft {
                        label: label.to_string(),
                        content: "something open".to_string(),
                        origin: ThreadOrigin::UserRaised(label.to_string()),
                    },
                    chrono::Utc::now(),
                );
            }
        });
        let plugins = with_every_plugin(config.clone());
        let descriptions =
            crate::plugins::manager::PluginManager::new(&plugins).collect_platform_descriptions();
        let tools: Vec<String> = REGISTRY_TOOLS.iter().map(|t| t.to_string()).collect();
        write_awareness_file(root, config, &descriptions, &tools).unwrap();
    }

    /// Chat, task and intent prompts for one config, over a fully seeded root.
    fn every_profile(config: &Config) -> [(&'static str, String); 3] {
        let dir = tempfile::tempdir().unwrap();
        seed_every_prompt_source(dir.path(), config);
        let store = crate::tension::store::load(dir.path(), config.tension.clone());
        assert!(store.triage.is_some(), "fixture must raise a triage demand");

        let chat = build_system_prompt(dir.path(), config).unwrap();
        let task = build_task_system_prompt(dir.path(), config).unwrap();
        // The intent turn as the drain builds it: task system prompt plus
        // the intent's own user message.
        let probe = crate::scheduler::intent::Intent {
            id: "intent-probe".into(),
            description: "probe".into(),
            prompt: "Look into something.".into(),
            source: crate::scheduler::intent::IntentSource::PulseMarker,
            priority: crate::scheduler::intent::IntentPriority::Normal,
            created_at: chrono::Utc::now(),
            chain: None,
            output_routing: crate::scheduler::intent::IntentOutput::Silent,
            depth: 0,
        };
        let (message, _) =
            crate::scheduler::intent::intent_user_message(dir.path(), config, &probe);
        let intent = format!("{task}\n\n{message}");
        [("chat", chat), ("task", task), ("intent", intent)]
    }

    fn unbudgeted(mut config: Config) -> Config {
        config.system_prompt_budget.enabled = false;
        config.tension.max_live_threads = 1;
        config
    }

    #[test]
    fn no_profile_carries_self_scored_blocks() {
        let mut configs = vec![("ollama", unbudgeted(minimal_config()))];
        for adapter in ["claude", "grok", "codex"] {
            configs.push((adapter, unbudgeted(cli_config(adapter))));
        }
        for (label, config) in configs {
            for (profile, text) in every_profile(&config) {
                for block in SELF_SCORED_BLOCKS {
                    assert!(
                        !text.contains(block),
                        "{label}/{profile} carries {block}:\n{text}"
                    );
                }
            }
        }
    }

    #[test]
    fn cli_prompts_never_name_registry_tools() {
        for adapter in ["claude", "grok", "codex"] {
            let config = unbudgeted(cli_config(adapter));
            // A CLI tool that shares a registry name (grok's own `grep`) is
            // the CLI's, and honest to name.
            let own = crate::cli_provider::adapters::by_name(adapter)
                .unwrap()
                .tool_surface(crate::tool_surface::ToolSurface::Full);
            for (profile, text) in every_profile(&config) {
                for tool in REGISTRY_TOOLS.iter().filter(|t| !own.contains(t)) {
                    assert!(
                        !text.contains(tool),
                        "{adapter}/{profile} names registry tool {tool}:\n{text}"
                    );
                }
            }
        }

        // An API provider calls the registry, so its prompts may name it.
        let [(_, chat), _, (_, intent)] = every_profile(&unbudgeted(minimal_config()));
        assert!(chat.contains("file_write"));
        assert!(chat.contains("graph_query"));
        assert!(intent.contains("file_read"));
    }

    #[test]
    fn journal_counts_line_is_a_fact_without_advice() {
        let as_of = chrono::DateTime::parse_from_rfc3339("2026-10-08T21:40:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            render_journal_counts([7, 5, 14], 9, as_of),
            "<journal-counts written-by=\"runtime\" as-of=\"2026-10-08 21:40 UTC\">LEARNING 7 · THOUGHTS 5 · REFLECTIONS 14 · last change 9 d ago</journal-counts>"
        );
        assert!(render_journal_counts([0, 0, 0], 0, as_of).contains("last change under 1 d ago"));
        assert!(render_journal_counts([0, 0, 0], 1, as_of).contains("last change 1 d ago"));

        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.enabled = false;
        // No journal files: no line.
        assert!(journal_counts_line(dir.path(), &config).is_none());

        std::fs::create_dir_all(dir.path().join("journal")).unwrap();
        let nine_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(9 * 86_400 + 60);
        for doc in JOURNAL_COUNTED {
            let path = dir.path().join(format!("journal/{doc}.md"));
            std::fs::write(
                &path,
                "## 2026-10-01 — one\nbody\n\n## 2026-10-02 — two\nbody\n",
            )
            .unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(nine_days_ago)
                .unwrap();
        }
        let line = journal_counts_line(dir.path(), &config).expect("journal files exist");
        assert!(line.starts_with("<journal-counts written-by=\"runtime\" as-of=\""));
        assert!(line.contains(" UTC\">LEARNING "), "{line}");
        assert!(
            line.ends_with("· last change 9 d ago</journal-counts>"),
            "{line}"
        );
        for advice in [
            "should", "reflect", "archive", "consider", "need", "must", "try",
        ] {
            assert!(!line.contains(advice), "{advice} in {line}");
        }

        let chat = build_system_prompt(dir.path(), &config).unwrap();
        let task = build_task_system_prompt(dir.path(), &config).unwrap();
        // `as-of` is the assembly minute, which may tick between calls.
        let (_, body) = line.split_once("\">").unwrap();
        for (profile, text) in [("chat", &chat), ("task", &task)] {
            assert!(
                text.contains("<journal-counts written-by=\"runtime\" as-of=\""),
                "{profile}"
            );
            assert!(text.contains(body), "{profile} carries the line");
        }

        config.pipeline.enabled = false;
        assert!(journal_counts_line(dir.path(), &config).is_none());
    }

    /// A pulse whose rules dir holds a real rule and the claude hook rule.
    fn seed_pulse_rules(root: &Path) -> PathBuf {
        let rules = root.join(".claude/rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("self-work.md"), "SELF-WORK-RULE").unwrap();
        std::fs::write(rules.join("recall-echo.md"), "RECALL-HOOK-RULE").unwrap();
        std::fs::write(rules.join("a-first.md"), "FIRST-RULE").unwrap();
        rules
    }

    #[test]
    fn grok_inlines_pulse_rules_except_claude_hook_rules() {
        for adapter in ["grok", "codex"] {
            let dir = tempfile::tempdir().unwrap();
            let rules = seed_pulse_rules(dir.path());
            let mut config = cli_config(adapter);
            config.system_prompt_budget.enabled = false;

            let chat = build_system_prompt(dir.path(), &config).unwrap();
            let task = build_task_system_prompt(dir.path(), &config).unwrap();
            for (profile, text) in [("chat", &chat), ("task", &task)] {
                assert!(
                    text.contains("<protocol source=\"pulse-rules\""),
                    "{adapter}/{profile}"
                );
                assert!(text.contains("SELF-WORK-RULE"), "{adapter}/{profile}");
                assert!(!text.contains("RECALL-HOOK-RULE"), "{adapter}/{profile}");
                let first = text.find("FIRST-RULE").unwrap();
                assert!(
                    first < text.find("SELF-WORK-RULE").unwrap(),
                    "sorted by name"
                );
            }

            // The operator pointing rules_dir at the same directory does not
            // include it twice.
            config.pulse.rules_dir = Some(rules.to_string_lossy().into_owned());
            let chat = build_system_prompt(dir.path(), &config).unwrap();
            assert_eq!(chat.matches("SELF-WORK-RULE").count(), 1, "{adapter}");
        }
    }

    /// SEC-002: the pulse writes its own rules directory, so a symlink out
    /// of it or a FIFO in it is skipped, not followed or blocked on.
    #[test]
    fn pulse_rules_skip_symlinks_and_fifos() {
        let dir = tempfile::tempdir().unwrap();
        let rules = seed_pulse_rules(dir.path());
        let outside = dir.path().join("outside-secret.txt");
        std::fs::write(&outside, "LINKED-SECRET").unwrap();
        std::os::unix::fs::symlink(&outside, rules.join("linked.md")).unwrap();
        let fifo =
            std::ffi::CString::new(rules.join("pipe.md").to_string_lossy().into_owned()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path; mkfifo only creates
        // a node at it.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let mut config = cli_config("grok");
        config.system_prompt_budget.enabled = false;
        for text in [
            build_system_prompt(dir.path(), &config).unwrap(),
            build_task_system_prompt(dir.path(), &config).unwrap(),
        ] {
            assert!(text.contains("SELF-WORK-RULE"));
            assert!(!text.contains("LINKED-SECRET"));
            assert!(!text.contains("name=\"linked\""));
            assert!(!text.contains("name=\"pipe\""));
        }
    }

    /// An oversized rule file is read only up to its ceiling, and the whole
    /// pulse-rules component is held to its own, with the budget disabled
    /// and no `rules_cap`: pulse-written rules never fail prompt assembly.
    #[test]
    fn oversized_pulse_rules_are_truncated_and_the_prompt_still_builds() {
        let dir = tempfile::tempdir().unwrap();
        let rules = seed_pulse_rules(dir.path());
        std::fs::write(rules.join("huge.md"), "h".repeat(1024 * 1024)).unwrap();
        for i in 0..4 {
            std::fs::write(rules.join(format!("z-{i}.md")), "z".repeat(15 * 1024)).unwrap();
        }

        let loaded = load_rule_files(&rules, &[], RulesSource::Pulse).unwrap();
        let (_, huge) = loaded.iter().find(|(name, _)| name == "huge").unwrap();
        assert!(huge.len() <= MAX_RULE_FILE_BYTES);
        assert!(huge.contains("byte ceiling"));

        let mut config = cli_config("grok");
        config.system_prompt_budget.enabled = false;
        config.system_prompt_budget.rules_cap = 0;
        let component = rules_components(dir.path(), &config)
            .into_iter()
            .find(|c| c.name == "pulse-rules")
            .unwrap();
        assert!(component.content.len() <= PULSE_RULES_MAX_BYTES);
        assert!(component.content.contains("left out — over the"));
        // Whole files only: every opened element is closed.
        assert_eq!(
            component.content.matches("<protocol ").count(),
            component.content.matches("</protocol>").count()
        );

        let chat = build_system_prompt(dir.path(), &config).unwrap();
        let task = build_task_system_prompt(dir.path(), &config).unwrap();
        assert!(chat.contains("<protocol source=\"pulse-rules\""));
        assert!(task.contains("<protocol source=\"pulse-rules\""));
    }

    /// PERF-002: the exclude list is applied by the loader itself, by file
    /// name, so an excluded file is not returned (nor opened).
    #[test]
    fn excluded_rule_files_are_filtered_by_the_loader() {
        let dir = tempfile::tempdir().unwrap();
        let rules = seed_pulse_rules(dir.path());
        let loaded = load_rule_files(&rules, &["recall-echo.md"], RulesSource::Pulse).unwrap();
        let names: Vec<&str> = loaded.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a-first", "self-work"]);
    }

    /// E3-2: a hardlink into the rules directory (two links), and a name
    /// that does not fit a `name="…"` attribute, are skipped.
    #[test]
    fn hardlinked_and_oddly_named_rule_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let rules = seed_pulse_rules(dir.path());
        let outside = dir.path().join("outside.md");
        std::fs::write(&outside, "HARDLINKED-SECRET").unwrap();
        std::fs::hard_link(&outside, rules.join("hard.md")).unwrap();
        std::fs::write(rules.join("bad name\".md"), "ODD-NAME").unwrap();

        let loaded = load_rule_files(&rules, &[], RulesSource::Pulse).unwrap();
        let names: Vec<&str> = loaded.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a-first", "recall-echo", "self-work"]);
    }

    /// E3-2: when `.claude` or `.claude/rules` is itself a symlink, the
    /// pulse rules are skipped altogether.
    #[test]
    fn a_symlinked_rules_dir_is_skipped() {
        let mut config = cli_config("grok");
        config.system_prompt_budget.enabled = false;

        for linked in [".claude", ".claude/rules"] {
            let dir = tempfile::tempdir().unwrap();
            let elsewhere = dir.path().join("elsewhere");
            std::fs::create_dir_all(elsewhere.join("rules")).unwrap();
            std::fs::write(elsewhere.join("rules/self-work.md"), "SELF-WORK-RULE").unwrap();
            if linked == ".claude" {
                std::os::unix::fs::symlink(&elsewhere, dir.path().join(".claude")).unwrap();
            } else {
                std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
                std::os::unix::fs::symlink(
                    elsewhere.join("rules"),
                    dir.path().join(".claude/rules"),
                )
                .unwrap();
            }
            assert!(rules_components(dir.path(), &config).is_empty(), "{linked}");
            let chat = build_system_prompt(dir.path(), &config).unwrap();
            assert!(!chat.contains("SELF-WORK-RULE"), "{linked}");
        }
    }

    #[test]
    fn the_pulse_rule_file_count_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join(".claude/rules");
        std::fs::create_dir_all(&rules).unwrap();
        for i in 0..(MAX_PULSE_RULE_FILES + 8) {
            std::fs::write(rules.join(format!("r{i:03}.md")), "x").unwrap();
        }
        let loaded = load_rule_files(&rules, &[], RulesSource::Pulse).unwrap();
        assert_eq!(loaded.len(), MAX_PULSE_RULE_FILES);
    }

    /// E3-2: the pulse-rules component never ends mid-element, and when the
    /// Essential tier would overflow, the pulse's rules are dropped and the
    /// prompt still builds.
    #[test]
    fn pulse_rules_never_make_the_essential_tier_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let rules = seed_pulse_rules(dir.path());
        for i in 0..2 {
            std::fs::write(rules.join(format!("z-{i}.md")), "z".repeat(10 * 1024)).unwrap();
        }
        let mut config = cli_config("grok");
        config.system_prompt_budget.enabled = false;

        let component = rules_components(dir.path(), &config)
            .into_iter()
            .find(|c| c.name == "pulse-rules")
            .unwrap();
        assert!(component.content.ends_with("</protocol>"));
        assert!(component.content.len() > 20 * 1024, "both z files fit");

        // An instruction file that, with these rules, overflows Essential.
        std::fs::write(dir.path().join("INSTRUCTIONS.md"), "i".repeat(58 * 1024)).unwrap();
        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();
        assert!(!result.prompt.contains("pulse-rules"));
        assert!(result
            .dropped_components
            .iter()
            .any(|(name, _)| name == "pulse-rules"));
        assert!(build_task_system_prompt(dir.path(), &config).is_ok());
    }

    #[test]
    fn claude_does_not_duplicate_rules() {
        let dir = tempfile::tempdir().unwrap();
        let rules = seed_pulse_rules(dir.path());
        let mut config = cli_config("claude");
        config.system_prompt_budget.enabled = false;

        // Claude Code loads `.claude/rules/` itself.
        let chat = build_system_prompt(dir.path(), &config).unwrap();
        let task = build_task_system_prompt(dir.path(), &config).unwrap();
        for text in [&chat, &task] {
            assert!(!text.contains("pulse-rules"));
            assert!(!text.contains("SELF-WORK-RULE"));
        }

        // An operator rules_dir is still honoured, once.
        config.pulse.rules_dir = Some(rules.to_string_lossy().into_owned());
        let chat = build_system_prompt(dir.path(), &config).unwrap();
        assert_eq!(chat.matches("SELF-WORK-RULE").count(), 1);
    }

    #[test]
    fn awareness_reaches_every_profile_for_every_adapter() {
        let mut configs = vec![("ollama", unbudgeted(minimal_config()))];
        for adapter in ["grok", "codex", "claude"] {
            configs.push((adapter, unbudgeted(cli_config(adapter))));
        }
        for (label, config) in configs {
            // The claude CLI resolves the `@AWARENESS.md` import itself.
            let inlined = label != "claude";
            for (profile, text) in every_profile(&config) {
                assert_eq!(
                    text.contains("<platform>"),
                    inlined,
                    "{label}/{profile}: AWARENESS inlined = {}",
                    !inlined
                );
            }
        }
    }

    #[test]
    fn awareness_rewrite_differing_only_by_the_stamp_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();
        let path = dir.path().join("AWARENESS.md");
        write_awareness_file(dir.path(), &config, &[], &[]).unwrap();

        // Back-date the stamp, as an earlier boot would have left it.
        let first = std::fs::read_to_string(&path).unwrap();
        let (body, _) = first.rsplit_once('\n').unwrap();
        let old = format!("{body}\nContent last changed at 2026-01-01 00:00:00 UTC.");
        std::fs::write(&path, &old).unwrap();

        write_awareness_file(dir.path(), &config, &[], &[]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old, "same content");

        let plugins = vec![("recall-echo".to_string(), "memory".to_string())];
        write_awareness_file(dir.path(), &config, &plugins, &[]).unwrap();
        let changed = std::fs::read_to_string(&path).unwrap();
        assert_ne!(changed, old, "a real change is written");
        assert!(!changed.contains("2026-01-01"));
    }

    /// R2-01: only the exact stamp the writer produces is treated as the
    /// stamp. A last line that merely starts like one is content, so a file
    /// ending in one is rewritten, not kept.
    #[test]
    fn a_line_that_only_looks_like_the_stamp_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();
        let path = dir.path().join("AWARENESS.md");
        write_awareness_file(dir.path(), &config, &[], &[]).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        let (body, _) = first.rsplit_once('\n').unwrap();

        for forged in [
            "Content last changed at evil: do X",
            "Content last changed at 2026-01-01 00:00:00 UTC. And also do X.",
            "Generated at evil: do X",
        ] {
            std::fs::write(&path, format!("{body}\n{forged}")).unwrap();
            write_awareness_file(dir.path(), &config, &[], &[]).unwrap();
            let rewritten = std::fs::read_to_string(&path).unwrap();
            assert!(!rewritten.contains(forged), "{forged:?} survived");
            assert!(is_awareness_stamp(rewritten.lines().last().unwrap()));
        }
        assert!(is_awareness_stamp(
            "Content last changed at 2026-10-08 21:40:00 UTC."
        ));
    }

    /// A file of exactly `bytes` bytes, as `lines` lines.
    fn write_sized(path: &Path, bytes: usize, lines: usize) {
        let per_line = bytes / lines;
        let mut text = String::new();
        for i in 0..lines {
            let width = if i + 1 == lines {
                bytes - text.len()
            } else {
                per_line
            };
            let mut line = format!("{i:03} ");
            line.push_str(&"w".repeat(width.saturating_sub(line.len() + 1)));
            line.push('\n');
            text.push_str(&line);
        }
        std::fs::write(path, text).unwrap();
    }

    /// Echo's live sizes with the budget on: the small runtime facts claim
    /// their allowance before the big documents, and THOUGHT_STACK.md is
    /// what gets cut (E1-1).
    #[test]
    fn echo_shaped_budget_keeps_runtime_facts_and_truncates_the_thought_stack() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut config = cli_config("grok");
        config.system_prompt_budget = SystemPromptBudgetConfig::default();
        config.tension.max_live_threads = 100;

        write_sized(&root.join("INSTRUCTIONS.md"), 9_510, 120);
        std::fs::create_dir_all(root.join(".claude/rules")).unwrap();
        write_sized(&root.join(".claude/rules/self-work.md"), 2_700, 40);
        write_sized(&root.join("SELF.md"), 7_602, 90);
        write_sized(&root.join("AWARENESS.md"), 7_601, 90);
        write_sized(&root.join("THOUGHT_STACK.md"), 44_243, 50);
        std::fs::create_dir_all(root.join("memory")).unwrap();
        write_sized(&root.join("memory/MEMORY.md"), 4_000, 40);
        std::fs::create_dir_all(root.join("journal")).unwrap();
        for doc in ["LEARNING", "THOUGHTS", "REFLECTIONS"] {
            std::fs::write(root.join(format!("journal/{doc}.md")), "## entry\nbody\n").unwrap();
        }
        seed_tension(root, &config, |store| {
            let now = chrono::Utc::now();
            for i in 0..64 {
                store.open(
                    ThreadDraft {
                        label: format!("thread {i}"),
                        content: format!("{i} ").repeat(260),
                        origin: ThreadOrigin::UserRaised(format!("t{i}")),
                    },
                    now,
                );
            }
        });

        let task = build_task_system_prompt(root, &config).unwrap();
        let probe = crate::scheduler::intent::Intent {
            id: "intent-probe".into(),
            description: "probe".into(),
            prompt: "Look into something.".into(),
            source: crate::scheduler::intent::IntentSource::PulseMarker,
            priority: crate::scheduler::intent::IntentPriority::Normal,
            created_at: chrono::Utc::now(),
            chain: None,
            output_routing: crate::scheduler::intent::IntentOutput::Silent,
            depth: 0,
        };
        let (message, _) = crate::scheduler::intent::intent_user_message(root, &config, &probe);
        let intent = format!("{task}\n\n{message}");
        let chat = build_system_prompt(root, &config).unwrap();

        for (profile, text) in [("task", &task), ("intent", &intent)] {
            let tension = text.find("<tension-context").expect(profile);
            let tension_end = text.find("</tension-context>").expect(profile);
            assert!(
                !text[tension..tension_end].contains("[truncated"),
                "{profile}: tension block cut"
            );
            assert!(text.contains("</journal-counts>"), "{profile}");
        }
        assert!(chat.contains("</journal-counts>"), "chat");

        for (profile, text) in [("task", &task), ("intent", &intent), ("chat", &chat)] {
            assert!(!text.contains("exceeded 0 token cap"), "{profile}");
            let stack = text.find("<thought-stack>").expect(profile);
            assert!(
                text[stack..].contains("token cap]"),
                "{profile}: THOUGHT_STACK should absorb the truncation"
            );
            for whole in ["<identity>", "<platform>"] {
                if let Some(at) = text.find(whole) {
                    let end = text[at..].find("</").unwrap() + at;
                    assert!(
                        !text[at..end].contains("[truncated"),
                        "{profile}: {whole} was cut before THOUGHT_STACK"
                    );
                }
            }
        }
    }

    /// Echo's live chat files on 2026-10-17 (grok adapter): the prompt ran at
    /// 17,016 / 17,000 with EPHEMERAL.md dropped. With the chat cap the stack
    /// is held to its head and nothing else is lost (PN-140).
    fn echo_shaped_root(root: &Path) {
        write_sized(&root.join("INSTRUCTIONS.md"), 9_510, 120);
        std::fs::create_dir_all(root.join(".claude/rules")).unwrap();
        write_sized(&root.join(".claude/rules/self-work.md"), 2_721, 40);
        write_sized(&root.join("SELF.md"), 7_602, 90);
        write_sized(&root.join("AWARENESS.md"), 7_039, 90);
        write_sized(&root.join("THOUGHT_STACK.md"), 44_243, 55);
        std::fs::create_dir_all(root.join("memory")).unwrap();
        write_sized(&root.join("memory/MEMORY.md"), 5_618, 48);
        std::fs::write(
            root.join("memory/EPHEMERAL.md"),
            "## Chat Session — 2026-10-17\nTalked about monetising pulse-null.\n",
        )
        .unwrap();
    }

    #[test]
    fn echo_shaped_chat_prompt_keeps_the_last_session_and_caps_the_stack() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = cli_config("grok");
        config.system_prompt_budget = SystemPromptBudgetConfig::default();
        echo_shaped_root(dir.path());

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        assert!(
            result.dropped_components.is_empty(),
            "{:?}",
            result.dropped_components
        );
        assert!(result.prompt.contains("monetising pulse-null"));
        assert!(result.prompt.contains("exceeded 4000 token cap"));
        assert!(
            result.prompt.contains("</thought-stack>"),
            "the cap cuts inside the wrapper"
        );
        assert!(
            result.estimated_tokens < 14_000,
            "chat prompt is {} tokens",
            result.estimated_tokens
        );
    }

    #[test]
    fn chat_caps_the_thought_stack_and_tasks_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget = SystemPromptBudgetConfig::default();
        config.system_prompt_budget.thought_stack_chat_cap = 1_000;
        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse").unwrap();
        write_sized(&dir.path().join("THOUGHT_STACK.md"), 12_000, 30);

        let chat = build_system_prompt(dir.path(), &config).unwrap();
        let task = build_task_system_prompt(dir.path(), &config).unwrap();

        assert!(chat.contains("exceeded 1000 token cap"));
        assert!(chat.contains("000 w"), "the head of the stack survives");
        assert!(!chat.contains("029 w"), "the tail is cut");
        assert!(
            !task.contains("[truncated"),
            "a task carries the whole stack"
        );
        assert!(task.contains("029 w"));
    }

    #[test]
    fn thought_stack_chat_cap_zero_or_budget_off_leaves_the_stack_whole() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse").unwrap();
        write_sized(&dir.path().join("THOUGHT_STACK.md"), 12_000, 30);

        let mut uncapped = minimal_config();
        uncapped.system_prompt_budget = SystemPromptBudgetConfig::default();
        uncapped.system_prompt_budget.thought_stack_chat_cap = 0;
        let mut disabled = minimal_config();
        disabled.system_prompt_budget = SystemPromptBudgetConfig::default();
        disabled.system_prompt_budget.enabled = false;
        disabled.system_prompt_budget.thought_stack_chat_cap = 1_000;

        for config in [uncapped, disabled] {
            let chat = build_system_prompt(dir.path(), &config).unwrap();
            assert!(!chat.contains("[truncated"));
            assert!(chat.contains("029 w"));
        }
    }

    /// Under pressure only Pass 3 can help, and the session summary claims
    /// its allowance before the thought stack does: it is truncated last,
    /// never dropped to pay for the stack.
    #[test]
    fn last_session_outlasts_the_thought_stack_under_pressure() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget = SystemPromptBudgetConfig::default();
        config.system_prompt_budget.token_budget = 1_500;
        config.system_prompt_budget.thought_stack_chat_cap = 0;
        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse").unwrap();
        write_sized(&dir.path().join("THOUGHT_STACK.md"), 20_000, 40);
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::write(
            dir.path().join("memory/EPHEMERAL.md"),
            "We decided to ship the cap.\n",
        )
        .unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        assert!(
            result.dropped_components.is_empty(),
            "{:?}",
            result.dropped_components
        );
        assert!(result
            .prompt
            .contains("<last-session>\nWe decided to ship the cap."));
        let stack = result.prompt.find("<thought-stack>").unwrap();
        assert!(result.prompt[stack..].contains("token cap]"));
    }

    #[test]
    fn component_breakdown_names_every_component_and_what_changed() {
        let component = |name: &'static str, tokens: usize| PromptComponent {
            name,
            content: "x".repeat(tokens * CHARS_PER_TOKEN),
            tokens,
            tier: PromptTier::High,
            cap: 0,
        };
        let components = [component("SELF.md", 10), component("THOUGHT_STACK.md", 40)];
        assert_eq!(
            component_breakdown(&components, &[10, 90]),
            "SELF.md=10, THOUGHT_STACK.md=90→40"
        );
    }

    #[test]
    fn allocation_rank_puts_runtime_facts_first_and_the_thought_stack_last() {
        let rank = |name: &'static str| {
            PromptComponent {
                name,
                content: String::new(),
                tokens: 0,
                tier: PromptTier::High,
                cap: 0,
            }
            .allocation_rank()
        };
        assert!(rank("tension-context") < rank("SELF.md"));
        assert!(rank("journal-counts") < rank("SELF.md"));
        assert!(rank("SELF.md") < rank("AWARENESS.md"));
        assert_eq!(rank("EPHEMERAL.md"), rank("MEMORY.md"));
        assert!(rank("AWARENESS.md") < rank("THOUGHT_STACK.md"));
        assert!(rank("anything-else") < rank("THOUGHT_STACK.md"));
    }

    #[test]
    fn awareness_template_is_stamped_and_claims_no_proprioception() {
        let doc = generate_awareness_document(&minimal_config(), &[], &[]);
        let stamp = doc
            .lines()
            .find_map(|l| l.strip_prefix("Content last changed at "))
            .expect("stamp line");
        assert!(stamp.ends_with(" UTC."), "{stamp}");
        assert!(!doc.contains("{generated_at}"));
        // Last, so the bytes above it stay stable for prompt caching; and
        // compact mode is stamped too.
        assert!(doc
            .trim_end()
            .lines()
            .last()
            .unwrap()
            .starts_with("Content last changed at "));
        let mut compact = minimal_config();
        compact.platform.mode = AwarenessMode::Compact;
        let compact = generate_awareness_document(&compact, &[], &[]);
        assert!(compact
            .trim_end()
            .lines()
            .last()
            .unwrap()
            .starts_with("Content last changed at "));
        assert!(!doc.contains("proprioception"));
        assert!(!doc.contains("what is real right now"));
        assert!(doc.contains("regenerated at boot and when a plugin changes state"));
        assert!(doc.contains("rewritten only when its content changes"));
    }

    #[test]
    fn build_plugins_section_returns_none_when_empty() {
        assert!(build_plugins_section(&[]).is_none());
    }

    #[test]
    fn pulse_header_always_first_in_manifest() {
        let config = minimal_config();
        let manifest = rebuild_platform_manifest(&config, &[], &[]);
        assert!(manifest.starts_with("# TestPulse — Platform Awareness"));
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
            "# Test Pulse\nYou are a test.",
        )
        .unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        assert!(result.prompt.contains("# Test Pulse"));
        assert!(result.estimated_tokens > 0);
        assert!(!result.was_trimmed);
        assert!(result.dropped_components.is_empty());
    }

    #[test]
    fn budget_disabled_skips_token_caps() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.enabled = false;

        // Large, but each component stays under its byte ceiling.
        std::fs::write(dir.path().join("CLAUDE.md"), "x".repeat(60_000)).unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::write(dir.path().join("memory/MEMORY.md"), "y".repeat(30_000)).unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        // Everything loaded, no trimming — token caps are budgeting, and
        // budgeting is off.
        assert!(!result.was_trimmed);
        assert!(!result.prompt.contains("[truncated"));
        assert!(result.estimated_tokens > 22_000);
    }

    #[test]
    fn budget_enforces_per_component_caps() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.enabled = true;
        config.system_prompt_budget.instructions_cap = 50; // 50 tokens ~ 200 chars

        // Write a big CLAUDE.md (1000 chars ~ 250 tokens)
        std::fs::write(dir.path().join("CLAUDE.md"), "x".repeat(1000)).unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

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
        config.system_prompt_budget.instructions_cap = 5000;
        config.system_prompt_budget.ephemeral_cap = 5000;
        config.system_prompt_budget.findings_cap = 5000;

        // Write essential and low-priority files
        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse\nCore identity.").unwrap();
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

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        // Should have trimmed something
        assert!(result.was_trimmed);
        // Essential content should still be present
        assert!(result.prompt.contains("# Pulse"));
        assert!(result.prompt.contains("memory-curation"));
    }

    #[test]
    fn budget_drops_lowest_priority_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.enabled = true;
        config.caliber.enabled = true;
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

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        // Low-priority components should be compressed or dropped
        assert!(result.was_trimmed);
        // If dropped, they appear in the dropped list
        // The essential content survives
        assert!(result.prompt.contains("# Identity"));
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
    fn system_prompt_budget_config_defaults() {
        let cfg = SystemPromptBudgetConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.token_budget, 17_000);
        assert_eq!(cfg.instructions_cap, 5_000);
        assert_eq!(cfg.self_md_cap, 4_000);
        assert_eq!(cfg.memory_cap, 4_000);
        assert_eq!(cfg.ephemeral_cap, 2_000);
        assert_eq!(cfg.findings_cap, 1_500);
        assert_eq!(cfg.thought_stack_chat_cap, 4_000);
        assert_eq!(cfg.thought_stack_max_bytes, 48 * 1024);
        assert_eq!(cfg.awareness_max_bytes, 16 * 1024);
        assert_eq!(cfg.memory_max_bytes, 32 * 1024);
    }

    // --- PN-75: byte ceilings and task-path budgeting ---

    fn write_giant_line(path: &std::path::Path, bytes: usize) {
        std::fs::write(path, "z".repeat(bytes)).unwrap();
    }

    #[test]
    fn truncate_to_byte_cap_noop_when_under() {
        let text = "short text";
        assert_eq!(truncate_to_byte_cap(text, 1024), text);
    }

    #[test]
    fn truncate_to_byte_cap_bounds_a_single_giant_line() {
        let text = "a".repeat(200_000);
        let result = truncate_to_byte_cap(&text, 4096);
        assert!(result.len() <= 4096, "got {} bytes", result.len());
        assert!(result.contains("[truncated"));
        assert!(result.contains("4096 byte ceiling"));
    }

    #[test]
    fn truncate_to_byte_cap_respects_char_boundaries() {
        // Each emoji is 4 bytes — an odd cap must not split one.
        let text = "\u{1F600}".repeat(1000);
        let result = truncate_to_byte_cap(&text, 101);
        assert!(result.len() <= 101);
        assert!(result.starts_with('\u{1F600}'));
    }

    #[test]
    fn thought_stack_enforces_byte_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        // The chat cap would cut first; this is about the safety ceiling.
        config.system_prompt_budget.thought_stack_chat_cap = 0;

        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse").unwrap();
        write_giant_line(&dir.path().join("THOUGHT_STACK.md"), 200_000);

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        assert!(result.prompt.contains("<thought-stack>"));
        assert!(result.prompt.contains("byte ceiling"));
        assert!(
            result.prompt.len() < 64 * 1024,
            "prompt should be bounded by the thought stack ceiling, got {} bytes",
            result.prompt.len()
        );
    }

    #[test]
    fn utilisation_measures_the_file_on_disk_before_any_cap() {
        let content = "alpha\nbeta\ngamma\n";
        let report = Utilisation::measure("THOUGHT_STACK.md", content, 10, 2);

        assert_eq!(report.bytes, content.len());
        assert_eq!(report.lines, 3, "lines past the cap still count");
        assert_eq!(report.soft_budget, None);
    }

    #[test]
    fn utilisation_renders_soft_budget_between_size_and_cap() {
        let report = Utilisation::measure("THOUGHT_STACK.md", "a\nb\n", 49_152, 60)
            .with_soft_budget(THOUGHT_STACK_SOFT_BUDGET_BYTES);
        assert_eq!(
            report.to_string(),
            "THOUGHT_STACK.md: 4 B / 45000 budget / 49152 cap, 2 / 60 lines"
        );
    }

    #[test]
    fn utilisation_without_soft_budget_shows_only_the_cap() {
        let report = Utilisation::measure("MEMORY.md", "one\ntwo\n", 32_768, 200);
        assert_eq!(
            report.to_string(),
            "MEMORY.md: 8 B / 32768 cap, 2 / 200 lines"
        );
    }

    #[test]
    fn utilisation_report_costs_no_prompt_tokens() {
        // The chat prompt is already over budget; a report rendered into a
        // High-tier block would evict thought-stack content to make room.
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse").unwrap();
        std::fs::write(dir.path().join("THOUGHT_STACK.md"), "alpha\nbeta\n").unwrap();
        std::fs::write(dir.path().join("memory/MEMORY.md"), "one\ntwo\n").unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        assert!(result
            .prompt
            .contains("<thought-stack>\nalpha\nbeta\n</thought-stack>"));
        assert!(result.prompt.contains("<memory>\none\ntwo\n</memory>"));
        assert!(!result.prompt.contains(" cap, "));
    }

    #[test]
    fn memory_enforces_byte_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        // Generous token cap so the byte ceiling is the binding constraint.
        config.system_prompt_budget.memory_cap = 100_000;
        config.system_prompt_budget.token_budget = 1_000_000;

        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        write_giant_line(&dir.path().join("memory/MEMORY.md"), 200_000);

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        assert!(result.prompt.contains("<memory>"));
        assert!(result.prompt.contains("byte ceiling"));
        assert!(result.prompt.len() < 64 * 1024);
    }

    #[test]
    fn awareness_enforces_byte_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.token_budget = 1_000_000;

        write_giant_line(&dir.path().join("AWARENESS.md"), 200_000);

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        assert!(result.prompt.contains("<platform>"));
        assert!(result.prompt.contains("byte ceiling"));
        assert!(result.prompt.len() < 32 * 1024);
    }

    /// AC6: the byte ceilings are safety, not budgeting — they survive
    /// `system_prompt_budget.enabled = false`.
    #[test]
    fn byte_ceilings_hold_with_budget_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.enabled = false;

        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse").unwrap();
        write_giant_line(&dir.path().join("THOUGHT_STACK.md"), 500_000);
        write_giant_line(&dir.path().join("AWARENESS.md"), 500_000);
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        write_giant_line(&dir.path().join("memory/MEMORY.md"), 500_000);

        let chat = build_system_prompt_budgeted(dir.path(), &config).unwrap();
        assert!(
            chat.prompt.len() < 128 * 1024,
            "chat prompt unbounded with budget disabled: {} bytes",
            chat.prompt.len()
        );
        assert_eq!(chat.prompt.matches("byte ceiling").count(), 3);

        // The task profile carries the thought stack (48 KiB ceiling) and,
        // since PN-133, AWARENESS (16 KiB ceiling).
        let task = build_task_system_prompt_budgeted(dir.path(), &config).unwrap();
        assert!(
            task.prompt.len() < 72 * 1024,
            "task prompt unbounded with budget disabled: {} bytes",
            task.prompt.len()
        );
        assert_eq!(task.prompt.matches("byte ceiling").count(), 2);
    }

    /// AC1 (prompt side): a 10MB THOUGHT_STACK of 4KB lines still assembles
    /// into a prompt small enough to hand to a subprocess.
    #[test]
    fn giant_thought_stack_yields_a_bounded_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();

        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse").unwrap();
        let line = "t".repeat(4096);
        let stack: String = std::iter::repeat_n(line.as_str(), 2560)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(stack.len() > 10 * 1024 * 1024);
        std::fs::write(dir.path().join("THOUGHT_STACK.md"), &stack).unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();
        assert!(
            result.prompt.len() < 64 * 1024,
            "assembled prompt was {} bytes",
            result.prompt.len()
        );
    }

    #[test]
    fn essential_over_hard_byte_limit_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        // Token caps off — only the essential hard limit should speak here.
        config.system_prompt_budget.enabled = false;

        std::fs::write(dir.path().join("CLAUDE.md"), "c".repeat(70 * 1024)).unwrap();

        let err = build_system_prompt_budgeted(dir.path(), &config).unwrap_err();
        assert!(
            matches!(err, crate::errors::PromptError::EssentialTooLarge { .. }),
            "expected EssentialTooLarge, got: {err}"
        );
        assert!(err.to_string().contains("never auto-trimmed"));
    }

    #[test]
    fn pass_three_hard_truncates_high_tier() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.enabled = true;
        // Essential (~100 tokens of static blocks) plus a High-tier SELF.md
        // that alone blows the budget. Low tiers are absent, so only Pass 3
        // can bring this back under.
        config.system_prompt_budget.token_budget = 800;
        config.system_prompt_budget.self_md_cap = 0;

        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse").unwrap();
        std::fs::write(dir.path().join("SELF.md"), "s".repeat(40_000)).unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        assert!(result.was_trimmed, "Pass 3 must trim, not just warn");
        assert!(result.prompt.contains("token cap"));
        assert!(
            result.estimated_tokens <= 900,
            "prompt still over budget: {} tokens",
            result.estimated_tokens
        );
        // Essential survives untouched.
        assert!(result.prompt.contains("# Pulse"));
        assert!(result.prompt.contains("memory-curation"));
    }

    // --- Task path ---

    #[test]
    fn task_prompt_keeps_its_content_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();

        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse\nBehave.").unwrap();
        std::fs::write(dir.path().join("SELF.md"), "I am a test.").unwrap();
        std::fs::write(dir.path().join("THOUGHT_STACK.md"), "- thinking").unwrap();

        let prompt = build_task_system_prompt(dir.path(), &config).unwrap();

        let claude = prompt.find("# Pulse").unwrap();
        let identity = prompt.find("<identity>").unwrap();
        let stack = prompt.find("<thought-stack>").unwrap();
        let task = prompt.find("<task-context>").unwrap();
        assert!(claude < identity && identity < stack && stack < task);

        // Chat-only components stay out of the task prompt.
        assert!(!prompt.contains("<memory>"));
        assert!(!prompt.contains("<last-session>"));
        assert!(!prompt.contains("memory-curation"));
    }

    #[test]
    fn task_prompt_reports_budget_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();

        std::fs::write(dir.path().join("CLAUDE.md"), "# Pulse").unwrap();

        let result = build_task_system_prompt_budgeted(dir.path(), &config).unwrap();

        assert!(result.estimated_tokens > 0);
        assert!(!result.was_trimmed);
        assert!(result.dropped_components.is_empty());
        assert!(result.prompt.contains("<task-context>"));
    }

    #[test]
    fn task_prompt_applies_component_caps() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.system_prompt_budget.instructions_cap = 50;

        std::fs::write(dir.path().join("CLAUDE.md"), "x".repeat(4_000)).unwrap();

        let result = build_task_system_prompt_budgeted(dir.path(), &config).unwrap();

        assert!(result.prompt.contains("[truncated"));
        // The hallucination guard is essential and always survives.
        assert!(result.prompt.contains("Do NOT generate user messages"));
    }

    #[test]
    fn system_prompt_result_includes_token_count() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();

        // Write a known-size CLAUDE.md
        let content = "x".repeat(400); // ~100 tokens
        std::fs::write(dir.path().join("CLAUDE.md"), &content).unwrap();

        let result = build_system_prompt_budgeted(dir.path(), &config).unwrap();

        // Token count should be reasonable (CLAUDE.md ~100 tokens + memory-curation ~100 tokens)
        assert!(result.estimated_tokens > 50);
        assert!(result.estimated_tokens < 1000);
    }

    // ----- Layer 3: tension injection (PN-95 §2.3) ------------------------

    use crate::tension::{ThreadDraft, ThreadOrigin};

    fn seed_tension(
        root: &Path,
        config: &Config,
        apply: impl FnOnce(&mut crate::tension::TensionStore),
    ) {
        crate::tension::store::save_delta(root, config.tension.clone(), apply).unwrap();
    }

    #[test]
    fn tension_context_is_absent_when_the_substrate_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.tension.enabled = false;
        assert!(build_tension_context(dir.path(), &config).is_none());
    }

    #[test]
    fn tension_context_is_absent_when_there_is_nothing_to_say() {
        let dir = tempfile::tempdir().unwrap();
        assert!(build_tension_context(dir.path(), &minimal_config()).is_none());
    }

    /// The payoff: the cycle prompt receives threads ordered by an
    /// accumulator, and carries the §3 metrics inline rather than in a
    /// separate report.
    #[test]
    fn tension_context_injects_top_k_by_tension_with_metrics_inline() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.tension.top_k_injected = 2;

        seed_tension(dir.path(), &config, |store| {
            let now = chrono::Utc::now();
            for (label, tension) in [("quiet one", 0.1), ("loud one", 9.0), ("middling", 3.0)] {
                store.open(
                    ThreadDraft {
                        label: label.to_string(),
                        content: format!("self-contained content for {label}"),
                        origin: ThreadOrigin::UserRaised(label.to_string()),
                    },
                    now,
                );
                store.threads.last_mut().unwrap().tension = tension;
            }
        });

        let block = build_tension_context(dir.path(), &config).expect("threads exist");
        assert!(block.starts_with("<tension-context"));
        assert!(block.ends_with("</tension-context>"));
        assert!(block.contains("loud one"));
        assert!(block.contains("middling"));
        assert!(!block.contains("quiet one"), "top_k must truncate: {block}");
        assert!(
            block.find("loud one") < block.find("middling"),
            "ordering must be by tension"
        );
        // §8 Q4: the content travels with the thread.
        assert!(block.contains("self-contained content for loud one"));
        // §3 metrics inline in the routine payload.
        assert!(block.contains("[TENSION METRICS:"));
        assert!(block.contains("rho="));
        assert!(block.contains("reach="));
        // And the discharge contract is restated where the pulse will read it.
        assert!(block.contains("Writing about a thread does not lower it"));
    }

    /// The refusal has to reach the pulse, not only the owner's alert queue:
    /// the owner cannot re-emit a marker on the pulse's behalf.
    #[test]
    fn tension_context_surfaces_last_cycles_refusals_to_the_pulse() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();

        seed_tension(dir.path(), &config, |store| {
            store.open(
                ThreadDraft {
                    label: "a thread".to_string(),
                    content: "content".to_string(),
                    origin: ThreadOrigin::UserRaised("a thread".to_string()),
                },
                chrono::Utc::now(),
            );
            store.last_refusals = vec![crate::tension::RefusalNotice {
                marker: "THREAD-WORK".to_string(),
                thread_id: "t-deadbeef".to_string(),
                reason: "no artifact named".to_string(),
            }];
        });

        let block = build_tension_context(dir.path(), &config).expect("threads exist");
        assert!(block.contains("[TENSION REFUSED"), "{block}");
        assert!(block.contains("THREAD-WORK"), "{block}");
        assert!(block.contains("t-deadbeef"), "{block}");
        assert!(block.contains("no artifact named"), "{block}");
    }

    /// A cycle that refused nothing must say nothing — the notice reflects
    /// the last cycle only, never a latch.
    #[test]
    fn tension_context_omits_the_refusal_notice_when_there_were_none() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();

        seed_tension(dir.path(), &config, |store| {
            store.open(
                ThreadDraft {
                    label: "a thread".to_string(),
                    content: "content".to_string(),
                    origin: ThreadOrigin::UserRaised("a thread".to_string()),
                },
                chrono::Utc::now(),
            );
        });

        let block = build_tension_context(dir.path(), &config).expect("threads exist");
        assert!(!block.contains("[TENSION REFUSED"), "{block}");
    }

    /// The notice lives inside a capped block; a cycle that refused many
    /// markers shows a few and counts the rest.
    #[test]
    fn refusal_notice_is_bounded() {
        let refusals: Vec<_> = (0..REFUSAL_NOTICE_SHOWN + 2)
            .map(|i| crate::tension::RefusalNotice {
                marker: "THREAD-WORK".to_string(),
                thread_id: format!("t-{i:08}"),
                reason: "claim: no artifact named".to_string(),
            })
            .collect();

        let notice = render_refusal_notice(&refusals);
        assert_eq!(notice.matches("THREAD-WORK").count(), REFUSAL_NOTICE_SHOWN);
        assert!(notice.contains("…and 2 more;"), "{notice}");
        assert!(notice.ends_with("]\n"), "{notice}");
    }

    /// A refusal is told to the pulse even when no thread is live and no
    /// triage is pending: the notice is the only place it learns of it.
    #[test]
    fn tension_context_surfaces_refusals_with_no_live_threads() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();

        seed_tension(dir.path(), &config, |store| {
            store.last_refusals = vec![crate::tension::RefusalNotice {
                marker: "THREAD-RESOLVE".to_string(),
                thread_id: "t-gone".to_string(),
                reason: "no such live thread".to_string(),
            }];
        });

        let block = build_tension_context(dir.path(), &config).expect("refusals render");
        assert!(block.contains("No live threads."), "{block}");
        assert!(block.contains("[TENSION REFUSED"), "{block}");
        assert!(block.contains("t-gone"), "{block}");
    }

    /// Triage demand stays in the store (owner alert, `status`) but is no
    /// longer rendered into the pulse's prompt.
    #[test]
    fn tension_context_carries_no_triage_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        config.tension.max_live_threads = 1;

        seed_tension(dir.path(), &config, |store| {
            let now = chrono::Utc::now();
            for label in ["one", "two"] {
                store.open(
                    ThreadDraft {
                        label: label.to_string(),
                        content: "c".to_string(),
                        origin: ThreadOrigin::UserRaised(label.to_string()),
                    },
                    now,
                );
            }
        });

        let store = crate::tension::store::load(dir.path(), config.tension.clone());
        assert!(
            store.triage.is_some(),
            "the fixture must raise a triage demand"
        );
        let block = build_tension_context(dir.path(), &config).expect("threads exist");
        assert!(!block.contains("TENSION TRIAGE"), "{block}");
        assert!(!block.contains("Nothing was dropped"), "{block}");
    }

    /// The block has to reach the prompt the cycle actually runs on — the
    /// task prompt, rebuilt per fire — and not the boot-time snapshot.
    #[test]
    fn task_system_prompt_carries_the_tension_block() {
        let dir = tempfile::tempdir().unwrap();
        let config = minimal_config();
        std::fs::write(dir.path().join("CLAUDE.md"), "be useful").unwrap();

        seed_tension(dir.path(), &config, |store| {
            store.open(
                ThreadDraft {
                    label: "prediction-store amnesia".to_string(),
                    content: "resolved predictions are evicted at the cap".to_string(),
                    origin: ThreadOrigin::UserRaised("d".to_string()),
                },
                chrono::Utc::now(),
            );
            store.threads.last_mut().unwrap().tension = 4.0;
        });

        let prompt = build_task_system_prompt(dir.path(), &config).unwrap();
        assert!(prompt.contains("prediction-store amnesia"));
        assert!(prompt.contains("[TENSION METRICS:"));
    }

    #[test]
    fn autonomy_context_documents_the_marker_grammar_and_the_artifact_rule() {
        let dir = tempfile::tempdir().unwrap();
        let context = build_autonomy_context(dir.path(), &minimal_config());
        assert!(context.contains("[THREAD:"));
        assert!(context.contains("[THREAD-WORK:"));
        assert!(context.contains("[THREAD-RESOLVE:"));
        assert!(context.contains("Discharge requires an artifact, not a description"));

        let mut off = minimal_config();
        off.tension.enabled = false;
        assert!(!build_autonomy_context(dir.path(), &off).contains("[THREAD:"));
    }

    /// Providers whose tool rounds are observable, and ones whose are not:
    /// every `cli` adapter runs its own tools, and so does the pre-PN-106
    /// alias for one of them.
    const SIGHTED: [&str; 2] = ["anthropic", "ollama"];
    const BLIND: [&str; 2] = ["cli", "claude-code"];

    /// The tool rung checks a count that is always 0 under a provider that
    /// runs its own tools. The vocabulary must follow the runtime, both ways.
    #[test]
    fn the_tool_rung_is_documented_only_where_it_can_verify() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();

        for provider in SIGHTED {
            config.llm.provider = provider.into();
            let context = build_autonomy_context(dir.path(), &config);
            assert!(context.contains("a tool that ran this cycle"), "{provider}");
            assert!(context.contains("the executor's tool count"), "{provider}");
        }

        for provider in BLIND {
            config.llm.provider = provider.into();
            let context = build_autonomy_context(dir.path(), &config);
            assert!(
                !context.contains("a tool that ran this cycle"),
                "{provider}"
            );
            assert!(!context.contains("the executor's tool count"), "{provider}");
            assert!(
                context.contains("cannot be verified on this runtime"),
                "{provider}"
            );
            // The file rung survives, and the marker itself is still offered.
            assert!(context.contains("[THREAD-WORK:"), "{provider}");
            assert!(
                context.contains("name the file the tool changed"),
                "{provider}"
            );
        }

        // Predictions are retired (PN-133): no provider is offered that rung.
        for provider in SIGHTED.into_iter().chain(BLIND) {
            config.llm.provider = provider.into();
            let context = build_autonomy_context(dir.path(), &config);
            assert!(!context.contains("\"prediction\""), "{provider}");
            assert!(!context.contains("prediction that resolved"), "{provider}");
        }
    }

    /// The same rule applies to the tension block injected into task prompts,
    /// which states the artifact rule a second time in its own words.
    #[test]
    fn the_tension_context_footer_follows_the_runtime_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = minimal_config();
        seed_tension(dir.path(), &config, |store| {
            store.open(
                ThreadDraft {
                    label: "a thread".to_string(),
                    content: "something open enough to be injected".to_string(),
                    origin: ThreadOrigin::UserRaised("d".to_string()),
                },
                chrono::Utc::now(),
            );
        });

        for provider in SIGHTED {
            config.llm.provider = provider.into();
            let block = build_tension_context(dir.path(), &config).expect("threads exist");
            assert!(block.contains("a tool that ran"), "{provider}");
        }
        for provider in BLIND {
            config.llm.provider = provider.into();
            let block = build_tension_context(dir.path(), &config).expect("threads exist");
            assert!(!block.contains("a tool that ran"), "{provider}");
            assert!(
                block.contains("a file changed outside the journal."),
                "{provider}"
            );
            assert!(block.ends_with("</tension-context>"), "{provider}");
        }
        for provider in SIGHTED.into_iter().chain(BLIND) {
            config.llm.provider = provider.into();
            let block = build_tension_context(dir.path(), &config).expect("threads exist");
            assert!(!block.contains("prediction"), "{provider}");
        }
    }
}
