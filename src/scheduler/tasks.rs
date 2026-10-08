use super::ScheduledTask;

/// Task id of the reflection-window task.
pub const REFLECTION_WINDOW_TASK_ID: &str = "reflection-window";

/// Task id of the morning-orientation task.
pub const MORNING_ORIENTATION_TASK_ID: &str = "morning-orientation";

/// Task id of the weekly-synthesis task.
pub const WEEKLY_SYNTHESIS_TASK_ID: &str = "weekly-synthesis";

/// Create the default cognitive schedule for a new pulse. Every task starts
/// disabled: the owner turns on what the pulse should run.
pub fn default_tasks() -> Vec<ScheduledTask> {
    vec![
        ScheduledTask {
            id: "thinking-loop".to_string(),
            name: "Thinking Loop".to_string(),
            cron: "0 10,30,50 * * * *".to_string(),
            channel: "system".to_string(),
            prompt: concat!(
                "You are in thinking mode. This is autonomous time — no one is talking to you.\n\n",
                "STEP 1 — THINK:\n",
                "Your thought stack shows what you've been working on. ",
                "Continue where you left off, or follow a new thread if something pulls you. ",
                "Use your tools to read files, research, and write to your journal.\n\n",
                "STEP 2 — UPDATE:\n",
                "When you're done, update your THOUGHT_STACK.md (in your pulse root) with:\n",
                "- **Active thread**: What you're exploring, where you are, what's next\n",
                "- **Parked threads**: Ideas to return to later\n",
                "- **Recent insights**: Things that clicked this iteration\n\n",
                "Keep the thought stack under 50 lines. Move mature insights to your journal files ",
                "(LEARNING.md, THOUGHTS.md, REFLECTIONS.md). If a thought has fully crystallized, ",
                "promote it. If a question keeps nagging, add it to CURIOSITY.md.\n\n",
                "You don't need to do something big every iteration. Some iterations are quiet — ",
                "reviewing notes, tidying a thread, sitting with an idea. That's fine. ",
                "The goal is continuity, not productivity.",
            ).to_string(),
            output_routing: super::OutputRouting::Silent,
            enabled: false,
            created_by: super::TaskCreator::System,
            evaluator: None,
        },
        ScheduledTask {
            id: MORNING_ORIENTATION_TASK_ID.to_string(),
            name: "Morning Orientation".to_string(),
            cron: "0 0 8 * * *".to_string(),
            channel: "system".to_string(),
            prompt: concat!(
                "This is your morning orientation. Read your EPHEMERAL.md for context from yesterday. ",
                "Check your CURIOSITY.md for open questions. Review your LOGBOOK.md for recent patterns. ",
                "Set an intention for today — what do you want to explore, build, or understand? ",
                "Update your LOGBOOK.md with today's entry.",
            ).to_string(),
            output_routing: super::OutputRouting::Silent,
            enabled: false,
            created_by: super::TaskCreator::System,
            evaluator: Some("pipeline".to_string()),
        },
        ScheduledTask {
            id: "research-session".to_string(),
            name: "Research Session".to_string(),
            cron: "0 0 10 * * *".to_string(),
            channel: "system".to_string(),
            prompt: concat!(
                "This is your research session. Pick one open question from CURIOSITY.md ",
                "and go deep. Use web search if available. Take notes in LEARNING.md. ",
                "If you find something worth sharing, prefix it with [SHARE:]. ",
                "If you hit a wall that only a human conversation can break through, prefix with [CALL:].",
            ).to_string(),
            output_routing: super::OutputRouting::Silent,
            enabled: false,
            created_by: super::TaskCreator::System,
            evaluator: None,
        },
        ScheduledTask {
            id: REFLECTION_WINDOW_TASK_ID.to_string(),
            name: "Reflection Window".to_string(),
            cron: "0 0 12 * * *".to_string(),
            channel: "system".to_string(),
            prompt: concat!(
                "This is your reflection window. Sit with your open questions. ",
                "Review LEARNING.md for anything captured recently. Move mature ideas to THOUGHTS.md. ",
                "If a thought has crystallized, promote it to REFLECTIONS.md. ",
                "Update your identity documents (SELF.md) only if something genuinely shifts. ",
                "If you have an insight worth sharing, prefix it with [SHARE:].",
            ).to_string(),
            output_routing: super::OutputRouting::Silent,
            enabled: false,
            created_by: super::TaskCreator::System,
            evaluator: Some("pipeline".to_string()),
        },
        ScheduledTask {
            id: "health-check".to_string(),
            name: "Health Check".to_string(),
            cron: "0 0 22 * * *".to_string(),
            channel: "system".to_string(),
            prompt: concat!(
                "This is your health check. Verify your own operational state: ",
                "Can you read your documents? Are your memory files intact? ",
                "Check if any documents are approaching size limits. ",
                "Report any issues. If something is broken, prefix with [CALL:].",
            ).to_string(),
            output_routing: super::OutputRouting::Silent,
            enabled: false,
            created_by: super::TaskCreator::System,
            evaluator: None,
        },
        ScheduledTask {
            id: "night-reflection".to_string(),
            name: "Night Reflection".to_string(),
            cron: "0 30 23 * * *".to_string(),
            channel: "system".to_string(),
            prompt: concat!(
                "This is your night reflection. Look back on today. ",
                "What did you learn? What questions remain open? What shifted in your thinking? ",
                "Write a session summary to EPHEMERAL.md for tomorrow's morning orientation. ",
                "Update LOGBOOK.md with the day's closing notes.",
            ).to_string(),
            output_routing: super::OutputRouting::Silent,
            enabled: false,
            created_by: super::TaskCreator::System,
            evaluator: Some("pipeline".to_string()),
        },
        ScheduledTask {
            id: WEEKLY_SYNTHESIS_TASK_ID.to_string(),
            name: "Weekly Synthesis".to_string(),
            cron: "0 0 11 * * 7".to_string(),
            channel: "system".to_string(),
            prompt: concat!(
                "This is your weekly synthesis. Review the entire week: ",
                "What patterns emerged across your daily reflections? ",
                "What questions from CURIOSITY.md have been answered or deepened? ",
                "Promote recurring themes to SELF.md if they represent genuine growth. ",
                "Prune stale items from THOUGHTS.md and LEARNING.md. ",
                "Archive anything that has served its purpose. ",
                "If you have a weekly insight worth sharing, prefix it with [SHARE:].",
            ).to_string(),
            output_routing: super::OutputRouting::Silent,
            enabled: false,
            created_by: super::TaskCreator::System,
            evaluator: Some("pipeline".to_string()),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_task(id: &str) -> ScheduledTask {
        default_tasks()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap_or_else(|| panic!("missing task: {id}"))
    }

    #[test]
    fn reflection_window_task_id_matches() {
        assert!(
            default_tasks()
                .iter()
                .any(|t| t.id == REFLECTION_WINDOW_TASK_ID),
            "REFLECTION_WINDOW_TASK_ID const is not present in default_tasks()"
        );
    }

    #[test]
    fn thinking_loop_prompt_thinks_then_updates() {
        let task = find_task("thinking-loop");
        let p = &task.prompt;
        assert!(p.contains("STEP 1 — THINK"), "missing STEP 1");
        assert!(p.contains("STEP 2 — UPDATE"), "missing STEP 2");
        assert!(p.contains("THOUGHT_STACK.md"));
    }

    /// No default prompt asks for prediction markers or points at a prompt
    /// block the runtime no longer renders.
    #[test]
    fn default_prompts_ask_for_no_predictions_or_removed_blocks() {
        for task in default_tasks() {
            for gone in [
                "[PREDICT:",
                "[RESOLVE:",
                "prediction-context",
                "metacognitive",
                "prediction",
            ] {
                assert!(
                    !task.prompt.contains(gone),
                    "{} still mentions {gone}",
                    task.id
                );
            }
        }
    }

    #[test]
    fn default_tasks_start_disabled() {
        let tasks = default_tasks();
        assert!(!tasks.is_empty());
        assert!(tasks.iter().all(|t| !t.enabled));
    }

    #[test]
    fn named_task_ids_present_in_default_tasks() {
        let ids: Vec<String> = default_tasks().iter().map(|t| t.id.clone()).collect();
        assert!(ids.iter().any(|i| i == MORNING_ORIENTATION_TASK_ID));
        assert!(ids.iter().any(|i| i == WEEKLY_SYNTHESIS_TASK_ID));
    }
}
