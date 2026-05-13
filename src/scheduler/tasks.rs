use super::ScheduledTask;

/// Task id of the reflection-window task. Lifted to a const because the
/// scheduler runner uses it to decide whether to inject prediction-pressure
/// context (`runner::execute_task`). Renaming the task without updating
/// both call sites would silently break spec 2c — `tests::reflection_window_task_id_matches`
/// pins the invariant.
pub const REFLECTION_WINDOW_TASK_ID: &str = "reflection-window";

/// Task id of the morning-orientation task. Spec 2a: predictions emitted
/// from this task carry the Session timescale. Resolved by night-reflection.
pub const MORNING_ORIENTATION_TASK_ID: &str = "morning-orientation";

/// Task id of the weekly-synthesis task. Spec 2a: predictions emitted from
/// this task carry the Weekly timescale. Resolved by next week's synthesis.
pub const WEEKLY_SYNTHESIS_TASK_ID: &str = "weekly-synthesis";

/// Map a scheduled-task id to the default timescale for any
/// `[PREDICT:{...}]` markers it emits (spec 2a). The runner uses this when
/// calling `prediction::resolve::process_task_output` so that morning-
/// orientation predictions are persisted as `Session` and weekly-synthesis
/// predictions as `Weekly`, not all stamped `Cycle`. Unknown ids fall back
/// to `Cycle` — the safe default for any cognitive-cycle-style task.
#[must_use]
pub fn default_timescale_for(task_id: &str) -> crate::prediction::Timescale {
    use crate::prediction::Timescale;
    match task_id {
        MORNING_ORIENTATION_TASK_ID => Timescale::Session,
        WEEKLY_SYNTHESIS_TASK_ID => Timescale::Weekly,
        _ => Timescale::Cycle,
    }
}

/// Create the default cognitive schedule for a new entity.
pub fn default_tasks() -> Vec<ScheduledTask> {
    vec![
        ScheduledTask {
            id: "thinking-loop".to_string(),
            name: "Thinking Loop".to_string(),
            cron: "0 10,30,50 * * * *".to_string(),
            channel: "system".to_string(),
            prompt: concat!(
                "You are in thinking mode. This is autonomous time — no one is talking to you.\n\n",
                "STEP 1 — PREDICTIONS:\n",
                "Read your prediction-context block above (if present, it lists recent surprises ",
                "and how many predictions are still awaiting resolution). Then emit one or more ",
                "predictions for this cycle as JSON-in-marker:\n",
                "  [PREDICT:{\"content\":\"I will focus on <topic/thread>\",\"confidence\":0.7}]\n",
                "Be honest — predict what you genuinely expect to focus on, not what you think ",
                "you should. Confidence is your own self-assessment in [0.0, 1.0].\n\n",
                "STEP 2 — THINK:\n",
                "Your thought stack shows what you've been working on. ",
                "Continue where you left off, or follow a new thread if something pulls you. ",
                "You have tools: you can search the web, read files, write to your journal.\n",
                "If your metacognitive state shows declining signals or calibration surprises, ",
                "consider addressing those — generate a goal if needed.\n\n",
                "STEP 3 — RESOLVE:\n",
                "For every pending prediction surfaced in your prediction-context, emit a resolve ",
                "marker comparing prediction to what actually happened this cycle:\n",
                "  [RESOLVE:{\"id\":\"<prediction id>\",\"outcome\":\"<what actually happened>\",",
                "\"surprise\":0.0,\"direction\":\"overconfident|underconfident|misdirected|novel\",",
                "\"insight\":\"<optional: what this reveals>\"}]\n",
                "Surprise is 0.0 (perfectly predicted) to 1.0 (maximally surprising). Direction is ",
                "your honest read on the kind of error: were you overconfident, underconfident, ",
                "predicted the wrong thing, or hit something entirely novel?\n\n",
                "STEP 4 — UPDATE:\n",
                "When you're done, update your THOUGHT_STACK.md (in your entity root) with:\n",
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
            enabled: true,
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
                "Update your LOGBOOK.md with today's entry.\n\n",
                "Emit one Session-timescale prediction for today as JSON-in-marker:\n",
                "  [PREDICT:{\"content\":\"By tonight's reflection I will have <expected progress>\",",
                "\"confidence\":0.6}]\n",
                "Night-reflection will compare this against what actually happened.",
            ).to_string(),
            output_routing: super::OutputRouting::Silent,
            enabled: true,
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
            enabled: true,
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
            enabled: true,
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
            enabled: true,
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
            enabled: true,
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
                "If you have a weekly insight worth sharing, prefix it with [SHARE:].\n\n",
                "Emit one Weekly-timescale prediction for the coming week as JSON-in-marker:\n",
                "  [PREDICT:{\"content\":\"In the coming week I expect <pattern/development>\",",
                "\"confidence\":0.5}]\n",
                "Next week's synthesis will compare this against what unfolded.",
            ).to_string(),
            output_routing: super::OutputRouting::Silent,
            enabled: true,
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

    /// Audit Q-H2: the runner's reflection-window pressure path matches a
    /// task id by const, not magic string. If the const ever drifts away
    /// from the task vector this fails — making the silent-break failure
    /// mode loud.
    #[test]
    fn reflection_window_task_id_matches() {
        assert!(
            default_tasks()
                .iter()
                .any(|t| t.id == REFLECTION_WINDOW_TASK_ID),
            "REFLECTION_WINDOW_TASK_ID const is not present in default_tasks()"
        );
    }

    /// Spec 2d: thinking-loop must follow PREDICTIONS / THINK / RESOLVE / UPDATE.
    /// Snapshot would be brittle; substring assertions pin the contract without
    /// breaking on copy edits inside each step.
    #[test]
    fn thinking_loop_prompt_has_four_step_structure() {
        let task = find_task("thinking-loop");
        let p = &task.prompt;
        assert!(p.contains("STEP 1 — PREDICTIONS"), "missing STEP 1");
        assert!(p.contains("STEP 2 — THINK"), "missing STEP 2");
        assert!(p.contains("STEP 3 — RESOLVE"), "missing STEP 3");
        assert!(p.contains("STEP 4 — UPDATE"), "missing STEP 4");
        assert!(
            p.contains("[PREDICT:{"),
            "thinking-loop must instruct JSON-in-marker PREDICT syntax"
        );
        assert!(
            p.contains("[RESOLVE:{"),
            "thinking-loop must instruct JSON-in-marker RESOLVE syntax"
        );
    }

    /// Spec 2a: morning-orientation must emit a Session-timescale prediction.
    #[test]
    fn morning_orientation_emits_session_prediction() {
        let task = find_task("morning-orientation");
        assert!(task.prompt.contains("Session-timescale prediction"));
        assert!(task.prompt.contains("[PREDICT:{"));
    }

    /// Spec 2a: weekly-synthesis must emit a Weekly-timescale prediction.
    #[test]
    fn weekly_synthesis_emits_weekly_prediction() {
        let task = find_task("weekly-synthesis");
        assert!(task.prompt.contains("Weekly-timescale prediction"));
        assert!(task.prompt.contains("[PREDICT:{"));
    }

    /// Spec 2a: predictions emitted by each task carry the right timescale.
    /// Verifies the mapping the runner uses when persisting predictions.
    #[test]
    fn default_timescale_for_known_tasks() {
        use crate::prediction::Timescale;
        assert_eq!(
            default_timescale_for(MORNING_ORIENTATION_TASK_ID),
            Timescale::Session
        );
        assert_eq!(
            default_timescale_for(WEEKLY_SYNTHESIS_TASK_ID),
            Timescale::Weekly
        );
        assert_eq!(
            default_timescale_for(REFLECTION_WINDOW_TASK_ID),
            Timescale::Cycle
        );
        assert_eq!(default_timescale_for("thinking-loop"), Timescale::Cycle);
        assert_eq!(default_timescale_for("some-unknown-task"), Timescale::Cycle);
    }

    /// MORNING_ORIENTATION_TASK_ID + WEEKLY_SYNTHESIS_TASK_ID must be wired
    /// into the `default_tasks()` definitions consumed by `default_timescale_for`.
    #[test]
    fn timescale_task_ids_present_in_default_tasks() {
        let ids: Vec<String> = default_tasks().iter().map(|t| t.id.clone()).collect();
        assert!(ids.iter().any(|i| i == MORNING_ORIENTATION_TASK_ID));
        assert!(ids.iter().any(|i| i == WEEKLY_SYNTHESIS_TASK_ID));
    }
}
