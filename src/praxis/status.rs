use super::parser;
use super::state;
use super::PraxisConfig;

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn bar(count: usize, soft: usize, hard: usize) -> String {
    let max_width = 20;
    let filled = if hard > 0 {
        (count * max_width / hard).min(max_width)
    } else {
        0
    };
    let empty = max_width - filled;

    let color = if count >= hard {
        RED
    } else if count >= soft {
        YELLOW
    } else {
        GREEN
    };

    format!(
        "{color}{}{}  {count}/{hard}{RESET}",
        "█".repeat(filled),
        "░".repeat(empty)
    )
}

pub fn run(config: &PraxisConfig) -> Result<(), String> {
    let scan = parser::scan_with_config(config);
    let st = state::load(config)?;

    println!("\n{BOLD}praxis-echo{RESET} — Pipeline Health\n");

    // Document bars
    println!(
        "  {BOLD}LEARNING{RESET}     {}",
        bar(scan.learning.active, 5, 8)
    );
    println!(
        "  {BOLD}THOUGHTS{RESET}     {}",
        bar(scan.thoughts.active, 5, 10)
    );
    println!(
        "  {BOLD}CURIOSITY{RESET}    {}",
        bar(scan.curiosity.active, 3, 7)
    );
    println!(
        "  {BOLD}REFLECTIONS{RESET}  {}",
        bar(scan.reflections.total, 15, 20)
    );
    println!(
        "  {BOLD}PRAXIS{RESET}       {}",
        bar(scan.praxis.active, 5, 10)
    );

    // Reflection log
    if scan.session_log_entries > 0 {
        let range = match (&scan.session_log_oldest, &scan.session_log_newest) {
            (Some(old), Some(new)) => format!("  {DIM}({old} → {new}){RESET}"),
            _ => String::new(),
        };
        println!(
            "\n  {BOLD}Reflection Log{RESET}: {} entries{range}",
            scan.session_log_entries
        );
    }

    // Pipeline flow
    println!("\n  {BOLD}Pipeline Flow{RESET}");
    let last_movement = st.pipeline.last_movement.as_deref().unwrap_or("never");
    println!("    Last movement:  {last_movement}");
    println!("    Graduations:    {}", st.pipeline.total_graduations);
    println!("    Dissolutions:   {}", st.pipeline.total_dissolutions);
    println!("    Archival ops:   {}", st.pipeline.total_archival_ops);

    if st.pipeline.frozen_session_count >= 3 {
        println!(
            "    {RED}⚠ Frozen for {} sessions{RESET}",
            st.pipeline.frozen_session_count
        );
    }

    // Stale thoughts
    if !scan.stale_thoughts.is_empty() {
        println!("\n  {YELLOW}⚠ Stale Thoughts{RESET} (>7 days untouched):");
        for t in &scan.stale_thoughts {
            let date = t
                .last_touched
                .as_ref()
                .or(t.started.as_ref())
                .map(|s| s.as_str())
                .unwrap_or("?");
            println!(
                "    {YELLOW}•{RESET} {} {DIM}(last: {date}){RESET}",
                t.title
            );
        }
    }

    // Session history (last 5)
    if !st.session_history.is_empty() {
        println!("\n  {BOLD}Recent Sessions{RESET}");
        let start = if st.session_history.len() > 5 {
            st.session_history.len() - 5
        } else {
            0
        };
        for rec in &st.session_history[start..] {
            let active = if rec.pipeline_active {
                format!("{GREEN}active{RESET}")
            } else {
                format!("{DIM}quiet{RESET}")
            };
            println!(
                "    {} — L:{:+} T:{:+} G:{} D:{} R:{:+} [{}]",
                rec.date,
                rec.learning_delta,
                rec.thoughts_touched,
                rec.graduations,
                rec.dissolutions,
                rec.reflections_added,
                active
            );
        }
    }

    println!();
    Ok(())
}
