//! Outcome record types for tracking task results.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A record of what happened during a task or intent execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// Unique task/intent ID
    pub task_id: String,
    /// When the task completed
    pub timestamp: DateTime<Utc>,
    /// Capability domain (e.g., "research", "reflection", "technical")
    pub domain: String,
    /// Type of task that produced this outcome
    pub task_type: TaskType,
    /// Brief description of what was attempted
    pub description: String,
    /// What actually happened
    pub outcome: Outcome,
    /// Tokens consumed (input + output)
    pub tokens_used: u32,
    /// Number of tool rounds used
    pub tool_rounds: u32,
}

/// Classification of the task type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    Research,
    Reflection,
    Technical,
    Conversation,
    HealthCheck,
    Intent,
    Synthesis,
    Orientation,
}

impl std::fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskType::Research => write!(f, "research"),
            TaskType::Reflection => write!(f, "reflection"),
            TaskType::Technical => write!(f, "technical"),
            TaskType::Conversation => write!(f, "conversation"),
            TaskType::HealthCheck => write!(f, "health_check"),
            TaskType::Intent => write!(f, "intent"),
            TaskType::Synthesis => write!(f, "synthesis"),
            TaskType::Orientation => write!(f, "orientation"),
        }
    }
}

/// What happened with the task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Task completed successfully
    Success,
    /// Task partially completed
    Partial,
    /// Task failed
    Failed,
    /// Result was unexpected (positive or negative)
    Surprising,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Success => write!(f, "success"),
            Outcome::Partial => write!(f, "partial"),
            Outcome::Failed => write!(f, "failed"),
            Outcome::Surprising => write!(f, "surprising"),
        }
    }
}

/// Infer task type from a task name or ID.
pub fn infer_task_type(task_id: &str) -> TaskType {
    let id = task_id.to_lowercase();
    if id.starts_with("intent-") || id.starts_with("chain-") {
        TaskType::Intent
    } else if id.contains("research") {
        TaskType::Research
    } else if id.contains("reflect") || id.contains("night") {
        TaskType::Reflection
    } else if id.contains("health") || id.contains("check") {
        TaskType::HealthCheck
    } else if id.contains("synth") || id.contains("weekly") {
        TaskType::Synthesis
    } else if id.contains("orient") || id.contains("morning") {
        TaskType::Orientation
    } else {
        TaskType::Technical
    }
}

/// Infer outcome from response text length and tool usage.
pub fn infer_outcome(response_text: &str, tool_rounds: u32) -> Outcome {
    if response_text.trim().is_empty() {
        return Outcome::Failed;
    }
    if response_text.len() < 50 && tool_rounds == 0 {
        return Outcome::Partial;
    }
    Outcome::Success
}

/// Infer domain from task type and task name.
pub fn infer_domain(task_type: &TaskType, task_id: &str) -> String {
    match task_type {
        TaskType::Research => "research_synthesis".to_string(),
        TaskType::Reflection => "philosophical_reflection".to_string(),
        TaskType::Technical => "rust_implementation".to_string(),
        TaskType::HealthCheck => "infrastructure_ops".to_string(),
        TaskType::Synthesis => "philosophical_reflection".to_string(),
        TaskType::Orientation => "autonomous_initiative".to_string(),
        TaskType::Conversation => "voice_conversation".to_string(),
        TaskType::Intent => {
            let id = task_id.to_lowercase();
            if id.contains("research") {
                "research_synthesis".to_string()
            } else if id.contains("reflect") {
                "philosophical_reflection".to_string()
            } else {
                "autonomous_initiative".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_research_task() {
        assert_eq!(infer_task_type("daily-research"), TaskType::Research);
        assert_eq!(infer_task_type("self-research-abc"), TaskType::Research);
    }

    #[test]
    fn infer_reflection_task() {
        assert_eq!(infer_task_type("night-reflection"), TaskType::Reflection);
        assert_eq!(infer_task_type("daily-reflect"), TaskType::Reflection);
    }

    #[test]
    fn infer_health_check_task() {
        assert_eq!(infer_task_type("health-check"), TaskType::HealthCheck);
    }

    #[test]
    fn infer_intent_task() {
        assert_eq!(infer_task_type("intent-research-abc"), TaskType::Intent);
        assert_eq!(infer_task_type("chain-intent-xyz"), TaskType::Intent);
    }

    #[test]
    fn infer_synthesis_task() {
        assert_eq!(infer_task_type("weekly-synthesis"), TaskType::Synthesis);
    }

    #[test]
    fn infer_orientation_task() {
        assert_eq!(
            infer_task_type("morning-orientation"),
            TaskType::Orientation
        );
    }

    #[test]
    fn infer_outcome_empty_is_failed() {
        assert_eq!(infer_outcome("", 0), Outcome::Failed);
        assert_eq!(infer_outcome("   ", 0), Outcome::Failed);
    }

    #[test]
    fn infer_outcome_short_no_tools_is_partial() {
        assert_eq!(infer_outcome("ok", 0), Outcome::Partial);
    }

    #[test]
    fn infer_outcome_normal_is_success() {
        let long_response = "This is a detailed response with meaningful content that indicates the task completed properly.";
        assert_eq!(infer_outcome(long_response, 2), Outcome::Success);
    }

    #[test]
    fn infer_domain_from_type() {
        assert_eq!(infer_domain(&TaskType::Research, "x"), "research_synthesis");
        assert_eq!(
            infer_domain(&TaskType::Reflection, "x"),
            "philosophical_reflection"
        );
        assert_eq!(
            infer_domain(&TaskType::Technical, "x"),
            "rust_implementation"
        );
        assert_eq!(
            infer_domain(&TaskType::HealthCheck, "x"),
            "infrastructure_ops"
        );
    }

    #[test]
    fn infer_domain_for_intent() {
        assert_eq!(
            infer_domain(&TaskType::Intent, "intent-research-memory"),
            "research_synthesis"
        );
        assert_eq!(
            infer_domain(&TaskType::Intent, "intent-reflect-on-identity"),
            "philosophical_reflection"
        );
        assert_eq!(
            infer_domain(&TaskType::Intent, "intent-something-else"),
            "autonomous_initiative"
        );
    }
}
