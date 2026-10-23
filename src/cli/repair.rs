use console::style;

use std::path::Path;

use crate::cli_provider::adapter::{ReportedUserHook, UserHookScan};
use crate::config::Config;
use crate::init::agent_bootstrap::{self, printable, ItemStatus};

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    let Some(adapter) = config
        .llm
        .cli_adapter()
        .and_then(crate::cli_provider::adapters::by_name)
    else {
        println!(
            "  Provider is '{}', not an agent CLI. Nothing to repair.",
            config.llm.provider
        );
        return Ok(());
    };
    let integration = adapter.integration();
    let recall_bin = agent_bootstrap::find_recall_echo_bin();

    println!();
    println!(
        "  {}",
        style(format!(
            "Checking {} agent integration for {}...",
            adapter.name(),
            root_dir.display()
        ))
        .bold()
    );
    println!();

    let mut results = agent_bootstrap::ensure_common(&root_dir, integration.recall_echo_provider());
    results.extend(integration.ensure(&root_dir, &recall_bin));

    let mut changed = 0;
    let mut existing = 0;
    let mut skipped = 0;
    let mut user_level_concern = false;

    for item in &results {
        println!("  {item}");
        match &item.status {
            ItemStatus::Created | ItemStatus::Updated | ItemStatus::UpdatedWithWarning(_) => {
                changed += 1
            }
            ItemStatus::Exists => existing += 1,
            ItemStatus::Skipped(_) => skipped += 1,
            _ => {}
        }
    }

    // Leftovers of the pre-PN-104 layout: `$HOME/.claude` symlinks into this
    // pulse are removed (they are ours). User-level recall-echo hooks are
    // only reported — that file is the user's. Every `consume` hook is
    // reported: the path picks which file is printed and does not limit
    // which sessions the hook runs in.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if let Some(home) = home {
        let legacy = integration.legacy_home_links(&root_dir, &home);
        if !legacy.is_empty() {
            println!();
            println!(
                "  {}",
                style("Retiring legacy user-level links into this pulse:").bold()
            );
            for link in &legacy {
                match std::fs::remove_file(link) {
                    Ok(()) => {
                        changed += 1;
                        println!(
                            "    {} {} removed",
                            style("✓").green(),
                            printable(&link.display().to_string())
                        );
                    }
                    Err(e) => {
                        skipped += 1;
                        println!(
                            "    {} {} could not be removed: {e}",
                            style("⚠").yellow(),
                            printable(&link.display().to_string())
                        );
                    }
                }
            }
        }

        let scan = integration.user_hooks_missing_root(&home);
        user_level_concern = user_level_needs_attention(&scan);
        match scan {
            UserHookScan::NotInspected { path, reason } => {
                println!();
                println!(
                    "  {} {}",
                    style("⚠").yellow(),
                    user_level_not_inspected_line(&path, &reason)
                );
            }
            UserHookScan::Inspected(stale) if !stale.is_empty() => {
                let settings = integration
                    .user_hooks_location(&home)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "the user-level hook file".to_string());
                let warning = user_hook_warning(&settings, &stale);
                println!();
                println!("  {} {}", style("⚠").yellow(), warning.headline);
                for line in &warning.details {
                    println!("    {line}");
                }
                for command in &warning.commands {
                    println!("      {}", printable(command));
                }
            }
            UserHookScan::Inspected(_) => {}
        }
    }

    println!();
    let summary = repair_summary_line(changed, existing, user_level_concern);
    if changed > 0 {
        println!("  {} {summary}", style("✓").green().bold());
    } else if user_level_concern {
        println!("  {} {summary}", style("⚠").yellow().bold());
    } else {
        println!("  {} {summary}", style("✓").green().bold());
    }
    if skipped > 0 {
        println!(
            "  {} {} item(s) skipped — check warnings above.",
            style("⚠").yellow(),
            skipped
        );
    }
    println!();

    Ok(())
}

/// Final repair line. A user-level report, or a file that was not inspected,
/// means the run is not clean even when no pulse file changed.
fn repair_summary_line(changed: usize, existing: usize, user_level_concern: bool) -> String {
    if changed > 0 {
        format!("Repaired {changed} item(s). {existing} already ok.")
    } else if user_level_concern {
        format!("User-level hooks still need attention. {existing} item(s) verified.")
    } else {
        format!("Everything looks good. {existing} item(s) verified.")
    }
}

fn user_level_needs_attention(scan: &UserHookScan) -> bool {
    match scan {
        UserHookScan::NotInspected { .. } => true,
        UserHookScan::Inspected(hooks) => !hooks.is_empty(),
    }
}

struct UserHookWarning {
    headline: String,
    details: Vec<String>,
    commands: Vec<String>,
}

/// Wording for the user-level hook report. `consume` is always explained:
/// `recall-echo init` writes `<recall-bin> consume '<pulse-root>'`, and that
/// path picks which file is printed without limiting which sessions run the
/// hook. A hook that names no root gets the working-directory sentence.
fn user_level_not_inspected_line(path: &Path, reason: &str) -> String {
    format!("{} was not inspected: {reason}", path.display())
}

fn user_hook_warning(settings: &str, hooks: &[ReportedUserHook]) -> UserHookWarning {
    let mut details = Vec::new();
    if hooks.iter().any(|h| h.consume) {
        details.push(
            "recall-echo init writes `<recall-bin> consume '<pulse-root>'` into this file."
                .to_string(),
        );
    }
    if hooks.iter().any(|h| h.consume && h.carries_root) {
        details.push(
            "The path picks which file is printed but does not limit which sessions the hook runs in."
                .to_string(),
        );
    }
    if hooks.iter().any(|h| !h.carries_root) {
        details.push(
            "A hook with no pulse root still runs in every session; the working directory chooses the file."
                .to_string(),
        );
    }
    details.push("This file is left unchanged — remove these by hand:".to_string());
    UserHookWarning {
        headline: format!("{settings} has recall-echo hooks that run in every session."),
        details,
        commands: hooks.iter().map(|h| h.command.clone()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooted_consume_warning_names_init_shape_and_session_limit() {
        let warning = user_hook_warning(
            "/home/u/.claude/settings.json",
            &[ReportedUserHook {
                command: "/usr/local/bin/recall-echo consume '/srv/pulse-a'".into(),
                carries_root: true,
                consume: true,
            }],
        );
        let text = format!("{}\n{}", warning.headline, warning.details.join("\n"));
        assert!(
            text.contains(
                "The path picks which file is printed but does not limit which sessions the hook runs in."
            ),
            "{text}"
        );
        assert!(
            text.contains("recall-echo init writes `<recall-bin> consume '<pulse-root>'`"),
            "{text}"
        );
        assert!(text.contains("left unchanged"), "{text}");
        assert_eq!(
            warning.commands,
            vec!["/usr/local/bin/recall-echo consume '/srv/pulse-a'".to_string()]
        );
        assert!(
            !warning
                .details
                .iter()
                .any(|line| line.contains("working directory")),
            "{text}"
        );
    }

    #[test]
    fn rootless_consume_warning_names_the_working_directory() {
        let warning = user_hook_warning(
            "/home/u/.claude/settings.json",
            &[ReportedUserHook {
                command: "recall-echo consume 2>/dev/null".into(),
                carries_root: false,
                consume: true,
            }],
        );
        let text = warning.details.join("\n");
        assert!(text.contains("working directory"), "{text}");
        assert!(
            !text.contains("The path picks which file is printed"),
            "{text}"
        );
        assert!(
            text.contains("recall-echo init writes `<recall-bin> consume '<pulse-root>'`"),
            "{text}"
        );
    }

    #[test]
    fn not_inspected_line_names_the_file_and_the_reason() {
        let line = user_level_not_inspected_line(
            Path::new("/home/u/.claude/settings.json"),
            "symlink target is outside the home directory",
        );
        assert!(line.contains("/home/u/.claude/settings.json"), "{line}");
        assert!(
            line.contains("symlink target is outside the home directory"),
            "{line}"
        );
        assert!(line.contains("was not inspected"), "{line}");
    }

    #[test]
    fn summary_line_is_not_clean_after_a_user_level_warning() {
        let reported = UserHookScan::Inspected(vec![ReportedUserHook {
            command: "recall-echo consume '/e'".into(),
            carries_root: true,
            consume: true,
        }]);
        let unread = UserHookScan::NotInspected {
            path: Path::new("/home/u/.claude/settings.json").to_path_buf(),
            reason: "not valid JSON: expected value".into(),
        };
        let quiet = UserHookScan::Inspected(Vec::new());
        assert!(user_level_needs_attention(&reported));
        assert!(user_level_needs_attention(&unread));
        assert!(!user_level_needs_attention(&quiet));

        let warned = repair_summary_line(0, 3, true);
        assert!(!warned.contains("Everything looks good."), "{warned}");
        assert_eq!(
            warned,
            "User-level hooks still need attention. 3 item(s) verified."
        );
        let clean = repair_summary_line(0, 3, false);
        assert_eq!(clean, "Everything looks good. 3 item(s) verified.");
        let repaired = repair_summary_line(2, 3, true);
        assert_eq!(repaired, "Repaired 2 item(s). 3 already ok.");
    }
}
