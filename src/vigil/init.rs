use std::fs;
use std::path::PathBuf;

use owo_colors::OwoColorize;

const PROTOCOL_TEMPLATE: &str = include_str!("../../templates/vigil-echo.md");

const PULSE_COMMAND: &str = "vigil-echo pulse";
const COLLECT_COMMAND: &str = "vigil-echo collect --trigger session-end";

enum Status {
    Created,
    Exists,
    Error,
}

fn print_status(status: Status, msg: &str) {
    match status {
        Status::Created => println!("  {} {msg}", "✓".green()),
        Status::Exists => println!("  {} {msg}", "~".yellow()),
        Status::Error => println!("  {} {msg}", "✗".red()),
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
            print_status(Status::Exists, "Protocol rules already up to date");
            return;
        }
        match fs::write(path, PROTOCOL_TEMPLATE) {
            Ok(()) => print_status(Status::Created, "Updated protocol rules"),
            Err(e) => print_status(
                Status::Error,
                &format!("Failed to write protocol rules: {e}"),
            ),
        }
        return;
    }
    match fs::write(path, PROTOCOL_TEMPLATE) {
        Ok(()) => print_status(
            Status::Created,
            "Created protocol rules (~/.claude/rules/vigil-echo.md)",
        ),
        Err(e) => print_status(
            Status::Error,
            &format!("Failed to write protocol rules: {e}"),
        ),
    }
}

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
    let has_collect = hook_has_command(&settings, "SessionEnd", COLLECT_COMMAND);

    let mut changed = false;

    if has_pulse {
        print_status(Status::Exists, "PreToolUse hook already registered");
    } else {
        add_hook_entry(&mut settings, "PreToolUse", PULSE_COMMAND);
        print_status(Status::Created, "Added PreToolUse hook (cognitive pulse)");
        changed = true;
    }

    if has_collect {
        print_status(Status::Exists, "SessionEnd hook already registered");
    } else {
        add_hook_entry(&mut settings, "SessionEnd", COLLECT_COMMAND);
        print_status(Status::Created, "Added SessionEnd hook (signal collection)");
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

pub fn run() -> Result<(), String> {
    let claude = super::claude_dir()?;

    if !claude.exists() {
        return Err(
            "~/.claude directory not found. Is Claude Code installed?\n  \
             Install Claude Code first, then run this again."
                .to_string(),
        );
    }

    println!(
        "\n{} — initializing metacognitive monitoring\n",
        "vigil-echo".bold()
    );

    // Create directories
    let rules_dir = super::rules_dir()?;
    let vigil_dir = super::vigil_dir()?;

    ensure_dir(&rules_dir, "rules directory");
    ensure_dir(&vigil_dir, "vigil state directory");

    // Write protocol rules
    write_protocol(&super::protocol_file()?);

    // Write default config
    let config_path = super::config_file()?;
    let default_config = super::state::Config::default();
    let config_json = serde_json::to_string_pretty(&default_config)
        .map_err(|e| format!("Failed to serialize config: {e}"))?;
    write_if_not_exists(&config_path, &format!("{config_json}\n"), "config.json");

    // Initialize empty signals history
    let signals_path = super::signals_file()?;
    write_if_not_exists(&signals_path, "[]\n", "signals.json");

    // Merge hooks into settings.json
    merge_hooks(&super::settings_file()?);

    // Summary
    println!(
        "\n{} Metacognitive monitoring is ready.\n\n\
         \x20 Signals tracked (Phase 1):\n\
         \x20   vocabulary_diversity  — Lexical variety in reflections\n\
         \x20   question_generation   — Active curiosity level\n\
         \x20   thought_lifecycle     — Thought turnover health\n\
         \x20   evidence_grounding    — Concrete reference density\n\n\
         \x20 Hooks installed:\n\
         \x20   PreToolUse → vigil-echo pulse    (inject cognitive health)\n\
         \x20   SessionEnd → vigil-echo collect   (extract signals)\n\n\
         \x20 Commands:\n\
         \x20   vigil-echo status    — Cognitive health dashboard\n\
         \x20   vigil-echo collect   — Manual signal collection\n\
         \x20   vigil-echo pulse     — Manual pulse injection\n",
        "Setup complete.".bold()
    );

    Ok(())
}
