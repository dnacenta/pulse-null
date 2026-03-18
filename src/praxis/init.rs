use std::fs;
use std::path::PathBuf;

use super::state;
use super::PraxisConfig;

const PROTOCOL_TEMPLATE: &str = include_str!("../../templates/praxis-echo.md");

const PULSE_COMMAND: &str = "praxis-echo pulse";
const CHECKPOINT_COMMAND: &str = "praxis-echo checkpoint";
const REVIEW_COMMAND: &str = "praxis-echo review";

// ANSI color helpers
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

enum Status {
    Created,
    Exists,
    Error,
}

fn print_status(status: Status, msg: &str) {
    match status {
        Status::Created => println!("  {GREEN}✓{RESET} {msg}"),
        Status::Exists => println!("  {YELLOW}~{RESET} {msg}"),
        Status::Error => println!("  {RED}✗{RESET} {msg}"),
    }
}

fn ensure_dir(path: &PathBuf, label: &str) {
    if path.exists() {
        print_status(Status::Exists, &format!("{label} already exists"));
    } else {
        match fs::create_dir_all(path) {
            Ok(()) => print_status(Status::Created, &format!("Created {label}")),
            Err(e) => print_status(Status::Error, &format!("Failed to create {label}: {e}")),
        }
    }
}

fn write_if_not_exists(path: &PathBuf, content: &str, label: &str) {
    if path.exists() {
        print_status(
            Status::Exists,
            &format!("{label} already exists — preserved"),
        );
    } else {
        match fs::write(path, content) {
            Ok(()) => print_status(Status::Created, &format!("Created {label}")),
            Err(e) => print_status(Status::Error, &format!("Failed to create {label}: {e}")),
        }
    }
}

fn write_protocol(path: &PathBuf) {
    if path.exists() {
        let existing = fs::read_to_string(path).unwrap_or_default();
        if existing == PROTOCOL_TEMPLATE {
            print_status(Status::Exists, "Pipeline protocol already up to date");
            return;
        }
        // Overwrite silently — the protocol should always match the binary version
        match fs::write(path, PROTOCOL_TEMPLATE) {
            Ok(()) => print_status(Status::Created, "Updated pipeline protocol"),
            Err(e) => print_status(
                Status::Error,
                &format!("Failed to write pipeline protocol: {e}"),
            ),
        }
        return;
    }
    match fs::write(path, PROTOCOL_TEMPLATE) {
        Ok(()) => print_status(
            Status::Created,
            "Created pipeline protocol (~/.claude/rules/praxis-echo.md)",
        ),
        Err(e) => print_status(
            Status::Error,
            &format!("Failed to write pipeline protocol: {e}"),
        ),
    }
}

/// Check if a hook event already contains a command substring.
fn hook_has_command(settings: &serde_json::Value, event: &str, needle: &str) -> bool {
    if let Some(hooks) = settings.get("hooks") {
        if let Some(event_hooks) = hooks.get(event) {
            if let Some(arr) = event_hooks.as_array() {
                for entry in arr {
                    if let Some(inner_hooks) = entry.get("hooks") {
                        if let Some(inner_arr) = inner_hooks.as_array() {
                            for hook in inner_arr {
                                if let Some(cmd) = hook.get("command") {
                                    if let Some(s) = cmd.as_str() {
                                        if s.contains(needle) {
                                            return true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

/// Add a hook entry to a given event, creating the hooks/event structure if needed.
fn add_hook_entry(settings: &mut serde_json::Value, event: &str, command: &str) {
    let hook_entry = serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": command
        }]
    });

    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let event_arr = hooks
        .as_object_mut()
        .unwrap()
        .entry(event)
        .or_insert_with(|| serde_json::json!([]));
    event_arr.as_array_mut().unwrap().push(hook_entry);
}

fn merge_hooks(settings_path: &PathBuf) {
    let mut settings: serde_json::Value = if settings_path.exists() {
        match fs::read_to_string(settings_path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => {
                    print_status(
                        Status::Error,
                        "Could not parse settings.json — add hooks manually",
                    );
                    return;
                }
            },
            Err(_) => {
                print_status(
                    Status::Error,
                    "Could not read settings.json — add hooks manually",
                );
                return;
            }
        }
    } else {
        serde_json::json!({})
    };

    let has_pulse = hook_has_command(&settings, "PreToolUse", PULSE_COMMAND);
    let has_checkpoint = hook_has_command(&settings, "PreCompact", CHECKPOINT_COMMAND);
    let has_review = hook_has_command(&settings, "SessionEnd", REVIEW_COMMAND);

    let mut changed = false;

    // --- PreToolUse hook (pulse: inject pipeline state) ---
    if has_pulse {
        print_status(Status::Exists, "PreToolUse hook already up to date");
    } else {
        add_hook_entry(&mut settings, "PreToolUse", PULSE_COMMAND);
        print_status(Status::Created, "Added PreToolUse hook (pipeline pulse)");
        changed = true;
    }

    // --- PreCompact hook (checkpoint: snapshot before compaction) ---
    if has_checkpoint {
        print_status(Status::Exists, "PreCompact hook already up to date");
    } else {
        add_hook_entry(&mut settings, "PreCompact", CHECKPOINT_COMMAND);
        print_status(Status::Created, "Added PreCompact hook (checkpoint)");
        changed = true;
    }

    // --- SessionEnd hook (review: post-session pipeline diff) ---
    if has_review {
        print_status(Status::Exists, "SessionEnd hook already up to date");
    } else {
        add_hook_entry(&mut settings, "SessionEnd", REVIEW_COMMAND);
        print_status(Status::Created, "Added SessionEnd hook (review)");
        changed = true;
    }

    if changed {
        match serde_json::to_string_pretty(&settings) {
            Ok(json) => match fs::write(settings_path, format!("{json}\n")) {
                Ok(()) => {}
                Err(e) => print_status(
                    Status::Error,
                    &format!("Failed to write settings.json: {e}"),
                ),
            },
            Err(e) => print_status(
                Status::Error,
                &format!("Failed to serialize settings.json: {e}"),
            ),
        }
    }
}

pub fn run(config: &PraxisConfig) -> Result<(), String> {
    let claude = &config.claude_dir;

    // Pre-flight check
    if !claude.exists() {
        return Err(
            "Config directory not found. Ensure the entity root is properly configured.\n  \
             Check your entity configuration, then run this again."
                .to_string(),
        );
    }

    println!("\n{BOLD}praxis-echo{RESET} — initializing pipeline enforcement\n");

    // Create directories
    let rules_dir = super::rules_dir(claude);
    let praxis_dir = super::praxis_dir(claude);
    let checkpoints_dir = super::checkpoints_dir(claude);

    ensure_dir(&rules_dir, "rules directory");
    ensure_dir(&praxis_dir, "praxis state directory");
    ensure_dir(&checkpoints_dir, "checkpoints directory");

    // Create archive directories
    let archives = super::archives_dir(&config.docs_dir);
    for sub in &["reflections", "learning", "curiosity", "thoughts", "logs"] {
        ensure_dir(&archives.join(sub), &format!("archives/{sub}"));
    }

    // Write protocol rules file
    write_protocol(&super::protocol_file(claude));

    // Initialize state file
    let state_path = super::state_file(claude);
    let initial_state = state::State {
        version: 1,
        ..Default::default()
    };
    let initial_json = serde_json::to_string_pretty(&initial_state)
        .map_err(|e| format!("Failed to serialize initial state: {e}"))?;
    write_if_not_exists(&state_path, &format!("{initial_json}\n"), "state.json");

    // Merge hooks into settings.json
    merge_hooks(&super::settings_file(claude));

    // Summary
    println!(
        "\n{BOLD}Setup complete.{RESET} Pipeline enforcement is ready.\n\n\
         \x20 Documents tracked:\n\
         \x20   LEARNING.md    — Research capture\n\
         \x20   THOUGHTS.md    — Incubation\n\
         \x20   CURIOSITY.md   — Open questions\n\
         \x20   REFLECTIONS.md — Crystallized observations\n\
         \x20   PRAXIS.md      — Active policies\n\
         \x20   SELF.md        — Integrated identity\n\n\
         \x20 Hooks installed:\n\
         \x20   PreToolUse → praxis-echo pulse      (inject pipeline state)\n\
         \x20   PreCompact → praxis-echo checkpoint  (snapshot before compaction)\n\
         \x20   SessionEnd → praxis-echo review      (post-session pipeline diff)\n\n\
         \x20 Commands:\n\
         \x20   praxis-echo status   — Pipeline health dashboard\n\
         \x20   praxis-echo scan     — Deep document inspection\n\
         \x20   praxis-echo archive  — Enforce thresholds, move overflow\n\
         \x20   praxis-echo nudge    — Queue curiosity-driven intent\n"
    );

    Ok(())
}
