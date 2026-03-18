use super::parser;
use super::PraxisConfig;

pub fn run(config: &PraxisConfig, format: &str) -> Result<(), String> {
    let scan = parser::scan_with_config(config);

    match format {
        "json" => {
            let output = serde_json::json!({
                "learning": {
                    "active": scan.learning.active,
                },
                "thoughts": {
                    "active": scan.thoughts.active,
                    "graduated": scan.thoughts.graduated,
                    "dissolved": scan.thoughts.dissolved,
                },
                "curiosity": {
                    "open": scan.curiosity.active,
                    "explored": scan.curiosity.explored,
                },
                "reflections": {
                    "observations": scan.reflections.active,
                    "total": scan.reflections.total,
                },
                "praxis": {
                    "active": scan.praxis.active,
                    "retired": scan.praxis.graduated,
                },
                "session_log": {
                    "entries": scan.session_log_entries,
                    "oldest": scan.session_log_oldest,
                    "newest": scan.session_log_newest,
                },
                "stale_thoughts": scan.stale_thoughts.iter().map(|t| {
                    serde_json::json!({
                        "title": t.title,
                        "last_touched": t.last_touched,
                        "started": t.started,
                    })
                }).collect::<Vec<_>>(),
                "document_hashes": scan.document_hashes,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&output)
                    .map_err(|e| format!("Failed to serialize: {e}"))?
            );
        }
        _ => {
            // Human-readable output
            println!("praxis-echo — Document Scan\n");
            println!("  LEARNING.md");
            println!("    Active threads: {}", scan.learning.active);

            println!("  THOUGHTS.md");
            println!("    Active:    {}", scan.thoughts.active);
            println!("    Graduated: {}", scan.thoughts.graduated);
            println!("    Dissolved: {}", scan.thoughts.dissolved);

            println!("  CURIOSITY.md");
            println!("    Open:     {}", scan.curiosity.active);
            println!("    Explored: {}", scan.curiosity.explored);

            println!("  REFLECTIONS.md");
            println!("    Observations: {}", scan.reflections.active);
            println!("    Total:        {}", scan.reflections.total);

            println!("  PRAXIS.md");
            println!("    Active:  {}", scan.praxis.active);
            println!("    Retired: {}", scan.praxis.graduated);

            if scan.session_log_entries > 0 {
                println!("  SESSION-LOG.md");
                println!("    Entries: {}", scan.session_log_entries);
                if let Some(ref old) = scan.session_log_oldest {
                    println!("    Oldest:  {old}");
                }
                if let Some(ref new) = scan.session_log_newest {
                    println!("    Newest:  {new}");
                }
            }

            if !scan.stale_thoughts.is_empty() {
                println!("\n  ⚠ Stale Thoughts (>7 days):");
                for t in &scan.stale_thoughts {
                    let date = t
                        .last_touched
                        .as_ref()
                        .or(t.started.as_ref())
                        .map(|s| s.as_str())
                        .unwrap_or("?");
                    println!("    • {} (last: {date})", t.title);
                }
            }
        }
    }

    Ok(())
}
