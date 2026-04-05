use std::io::{self, BufRead, Write};
use std::path::Path;

use pulse_system_types::llm::{LmProvider, Message, MessageContent, MessageSource, Role};

use crate::chat;
use crate::config::Config;
use crate::tools::ToolRegistry;
use crate::wal::{WalMeta, WalWriter};

/// Maximum tool-use round trips per user message.
const MAX_TOOL_ROUNDS: u32 = 25;

/// Run the interactive REPL loop.
pub async fn run(
    config: &Config,
    root_dir: &Path,
    provider: &dyn LmProvider,
    tools: &ToolRegistry,
    system_prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut conversation: Vec<Message> = Vec::new();
    let stdin = io::stdin();
    let entity_name = &config.entity.name;
    let plugin_count = config.plugins.len();

    // Initialize WAL for the REPL session
    let sessions_dir = root_dir.join("sessions");
    let wal = if config.sessions.wal_enabled {
        match WalWriter::new(&sessions_dir, config.sessions.wal_fsync) {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!(
                    "  \x1b[33mwarning\x1b[0m  WAL init failed: {} (continuing without WAL)",
                    e
                );
                None
            }
        }
    } else {
        None
    };

    let session_key = "repl:local";
    let mut wal_seq: u64 = 0;

    loop {
        // Prompt
        print!("  you \u{203a} ");
        io::stdout().flush()?;

        // Read input
        let mut input = String::new();
        let bytes = stdin.lock().read_line(&mut input)?;
        if bytes == 0 {
            // EOF (Ctrl+D)
            println!();
            break;
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        // Exit commands
        if matches!(input, "/exit" | "/quit" | "/q") {
            break;
        }

        // Status command — re-render dashboard
        if input == "/status" {
            println!();
            chat::banner::render(config, root_dir, plugin_count);
            continue;
        }

        // Build user message content
        let user_content = MessageContent::Text(input.to_string());

        // WAL: append user message BEFORE adding to conversation (write-ahead).
        // Only increment wal_seq on success to keep it in sync with the WAL file.
        if let Some(ref wal) = wal {
            let next_seq = wal_seq + 1;
            match wal.append(
                session_key,
                next_seq,
                Role::User,
                &user_content,
                Some(WalMeta {
                    channel: Some("repl".into()),
                    sender: Some("local".into()),
                }),
            ) {
                Ok(()) => wal_seq = next_seq,
                Err(e) => tracing::warn!("REPL WAL append failed for user message: {}", e),
            }
        }

        // Add user message to conversation
        conversation.push(Message {
            role: Role::User,
            content: user_content,
            source: Some(MessageSource::Human {
                channel: "repl".into(),
                sender: "local".into(),
            }),
        });

        // Compact conversation if approaching context budget
        // REPL doesn't track compaction failures or recent files — pass defaults
        let empty_files: Vec<crate::session_store::RecentFile> = Vec::new();
        crate::context::compact_if_needed(
            &mut conversation,
            provider,
            config.llm.context_budget,
            config.llm.max_tokens,
            root_dir,
            entity_name,
            "repl",
            None,
            0,
            &empty_files,
            None, // REPL doesn't track active plans
        )
        .await;

        println!();

        // Invoke LLM with shared tool loop (hallucination guard, action claim
        // validation, consecutive failure tracking, micro-compact — all included).
        let msg_count_before = conversation.len();
        let result = crate::tool_loop::invoke_with_tool_loop(
            provider,
            tools,
            system_prompt,
            &mut conversation,
            config.llm.max_tokens,
            MAX_TOOL_ROUNDS,
        )
        .await;

        match result {
            Ok(result) => {
                // WAL: log all new messages added by the tool loop
                if let Some(ref wal) = wal {
                    for msg in &conversation[msg_count_before..] {
                        let next_seq = wal_seq + 1;
                        match wal.append(
                            session_key,
                            next_seq,
                            msg.role.clone(),
                            &msg.content,
                            None,
                        ) {
                            Ok(()) => wal_seq = next_seq,
                            Err(e) => tracing::warn!("REPL WAL append failed: {}", e),
                        }
                    }
                }

                // Print the response
                if !result.text.is_empty() {
                    print_response(entity_name, &result.text);
                }

                // Surface hallucination guard warnings to the terminal
                if result.was_truncated {
                    eprintln!("  \x1b[33mwarning\x1b[0m  response truncated (hallucination guard)");
                }
                if result.circuit_breaker_fired {
                    eprintln!(
                        "  \x1b[33mwarning\x1b[0m  tool loop stopped after {} rounds (circuit breaker)",
                        result.tool_rounds
                    );
                }
            }
            Err(e) => {
                eprintln!("  \x1b[31merror\x1b[0m  {}", e);
                println!();
            }
        }
    }

    // Archive full conversation + write EPHEMERAL summary
    if let Some(archive_path) = crate::session::end_session(
        root_dir,
        entity_name,
        &conversation,
        "repl",
        "session-end",
        None,
    ) {
        if config.graph.enabled && config.graph.auto_ingest {
            let root = root_dir.to_path_buf();
            tokio::task::spawn_blocking(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!("graph ingest: failed to create runtime: {}", e);
                        return;
                    }
                };
                rt.block_on(async {
                    crate::session::graph_ingest_archive(&root, &archive_path, None).await;
                });
            });
        }
    }

    // Clean up WAL on clean exit (conversation is archived, WAL no longer needed)
    if let Some(ref wal) = wal {
        if let Err(e) = wal.remove(session_key) {
            tracing::warn!("REPL: failed to remove WAL: {}", e);
        }
        if let Err(e) = wal.remove_checkpoint(session_key) {
            tracing::warn!("REPL: failed to remove checkpoint: {}", e);
        }
    }

    Ok(())
}

/// Print entity response with name label.
fn print_response(entity_name: &str, text: &str) {
    // Cyan entity name, then wrapped text
    println!("  \x1b[36m{}\x1b[0m \u{203a} {}", entity_name, text);
    println!();
}
