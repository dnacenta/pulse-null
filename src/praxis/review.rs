use super::parser;
use super::state;
use super::PraxisConfig;

/// Run review with explicit config (plugin context).
pub fn run_with_config(config: &PraxisConfig) -> Result<(), String> {
    let mut st = state::load(config)?;
    let scan = parser::scan_with_config(config);

    let snapshot = match &st.session_start_snapshot {
        Some(s) => s.clone(),
        None => {
            // No pulse ran this session — nothing to diff
            return Ok(());
        }
    };

    // Compute deltas
    let learning_delta = scan.learning.active as i32 - snapshot.learning_threads as i32;
    let thoughts_delta = scan.thoughts.active as i32 - snapshot.active_thoughts as i32;
    let questions_delta = scan.curiosity.active as i32 - snapshot.open_questions as i32;
    let reflections_delta = scan.reflections.total as i32 - snapshot.observation_count as i32;
    let policies_delta = scan.praxis.active as i32 - snapshot.active_policies as i32;

    // Detect document changes via hash diffs
    let mut changed_docs: Vec<&str> = Vec::new();
    for (name, new_hash) in &scan.document_hashes {
        if let Some(old_hash) = snapshot.document_hashes.get(name) {
            if old_hash != new_hash {
                changed_docs.push(name);
            }
        }
    }

    let pipeline_active = !changed_docs.is_empty();

    // Detect graduations/dissolutions (approximate: compare counts)
    let new_graduations = (scan.thoughts.graduated as i32 - snapshot.active_thoughts as i32).max(0);
    let _new_dissolutions = scan.thoughts.dissolved; // Can't diff easily without baseline

    // Update pipeline state
    if pipeline_active {
        st.pipeline.last_movement = Some(state::today_iso());
        st.pipeline.frozen_session_count = 0;
    } else {
        st.pipeline.frozen_session_count += 1;
    }

    // Record session
    let record = state::SessionRecord {
        date: state::today_iso(),
        learning_delta,
        thoughts_touched: thoughts_delta,
        graduations: new_graduations,
        dissolutions: 0,
        reflections_added: reflections_delta,
        pipeline_active,
    };
    st.session_history.push(record);

    // Keep last 30 sessions
    if st.session_history.len() > 30 {
        let drain = st.session_history.len() - 30;
        st.session_history.drain(..drain);
    }

    // Clear session snapshot
    st.session_start_snapshot = None;
    st.last_review = Some(state::now_iso());
    state::save(&st, config)?;

    // Output summary
    println!("[PRAXIS — Session Review]");
    if pipeline_active {
        println!("  Documents changed: {}", changed_docs.join(", "));
    } else {
        println!("  No document changes this session.");
    }

    if learning_delta != 0 {
        println!("  Learning threads: {:+}", learning_delta);
    }
    if thoughts_delta != 0 {
        println!("  Active thoughts:  {:+}", thoughts_delta);
    }
    if questions_delta != 0 {
        println!("  Open questions:   {:+}", questions_delta);
    }
    if reflections_delta != 0 {
        println!("  Reflections:      {:+}", reflections_delta);
    }
    if policies_delta != 0 {
        println!("  Policies:         {:+}", policies_delta);
    }

    if st.pipeline.frozen_session_count >= config.freeze_threshold {
        println!(
            "  ⚠ Pipeline has been frozen for {} sessions.",
            st.pipeline.frozen_session_count
        );
    }
    println!("[END PRAXIS]");

    Ok(())
}
