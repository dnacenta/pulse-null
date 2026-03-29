use std::io::{self, BufRead, Write};
use std::path::Path;

use pulse_system_types::llm::{
    ContentBlock, LmProvider, Message, MessageContent, Role, StopReason,
};

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

        // WAL: append user message BEFORE adding to conversation (write-ahead)
        if let Some(ref wal) = wal {
            wal_seq += 1;
            if let Err(e) = wal.append(
                session_key,
                wal_seq,
                Role::User,
                &user_content,
                Some(WalMeta {
                    channel: Some("repl".into()),
                    sender: Some("local".into()),
                }),
            ) {
                tracing::warn!("REPL WAL append failed for user message: {}", e);
            }
        }

        // Add user message to conversation
        conversation.push(Message {
            role: Role::User,
            content: user_content,
        });

        // Compact conversation if approaching context budget
        crate::context::compact_if_needed(
            &mut conversation,
            provider,
            config.llm.context_budget,
            config.llm.max_tokens,
            root_dir,
            entity_name,
            "repl",
            None,
        )
        .await;

        // Tool definitions
        let tool_defs = if provider.supports_tools() && !tools.is_empty() {
            Some(tools.definitions())
        } else {
            None
        };
        let tool_defs_ref = tool_defs.as_deref();

        let mut rounds: u32 = 0;
        println!();

        loop {
            // Invoke LLM
            let result = match provider
                .invoke(
                    system_prompt,
                    &conversation,
                    config.llm.max_tokens,
                    tool_defs_ref,
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("  \x1b[31merror\x1b[0m  {}", e);
                    println!();
                    break;
                }
            };

            // Build assistant content
            let assistant_content = MessageContent::Blocks(result.content.clone());

            // WAL: append assistant response
            if let Some(ref wal) = wal {
                wal_seq += 1;
                if let Err(e) =
                    wal.append(session_key, wal_seq, Role::Assistant, &assistant_content, None)
                {
                    tracing::warn!("REPL WAL append failed for assistant message: {}", e);
                }
            }

            // Add assistant response to conversation
            conversation.push(Message {
                role: Role::Assistant,
                content: assistant_content,
            });

            match result.stop_reason {
                StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence => {
                    // Print the response
                    let text = result.text();
                    if !text.is_empty() {
                        print_response(entity_name, &text);
                    }
                    break;
                }
                StopReason::ToolUse => {
                    rounds += 1;
                    if rounds > MAX_TOOL_ROUNDS {
                        let text = result.text();
                        if !text.is_empty() {
                            print_response(entity_name, &text);
                        }
                        break;
                    }

                    // Execute tools and show indicators
                    let mut tool_results = Vec::new();
                    for block in &result.content {
                        if let ContentBlock::ToolUse { id, name, input } = block {
                            // Print tool indicator
                            print_tool_indicator(name, input);

                            let tool_result = match tools.get(name) {
                                Some(tool) => match tool.execute(input.clone()).await {
                                    Ok(output) => ContentBlock::ToolResult {
                                        tool_use_id: id.clone(),
                                        content: output,
                                        is_error: None,
                                    },
                                    Err(e) => ContentBlock::ToolResult {
                                        tool_use_id: id.clone(),
                                        content: format!("Error: {}", e),
                                        is_error: Some(true),
                                    },
                                },
                                None => ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: format!("Error: Unknown tool '{}'", name),
                                    is_error: Some(true),
                                },
                            };
                            tool_results.push(tool_result);
                        }

                        // Also print any text blocks that come with tool use
                        if let ContentBlock::Text { text } = block {
                            if !text.is_empty() {
                                print_response(entity_name, text);
                            }
                        }
                    }

                    // Build tool results content
                    let tool_content = MessageContent::Blocks(tool_results);

                    // WAL: append tool results
                    if let Some(ref wal) = wal {
                        wal_seq += 1;
                        if let Err(e) =
                            wal.append(session_key, wal_seq, Role::User, &tool_content, None)
                        {
                            tracing::warn!("REPL WAL append failed for tool results: {}", e);
                        }
                    }

                    // Add tool results and loop
                    conversation.push(Message {
                        role: Role::User,
                        content: tool_content,
                    });
                }
                StopReason::Other(ref reason) => {
                    let text = result.text();
                    if !text.is_empty() {
                        print_response(entity_name, &text);
                    } else {
                        eprintln!("  \x1b[33mwarning\x1b[0m  unexpected stop: {}", reason);
                    }
                    break;
                }
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

/// Print a tool execution indicator (dimmed).
fn print_tool_indicator(name: &str, input: &serde_json::Value) {
    let detail = match name {
        "file_read" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| format!("reading {}...", p))
            .unwrap_or_else(|| "reading file...".into()),
        "file_write" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| format!("writing {}...", p))
            .unwrap_or_else(|| "writing file...".into()),
        "file_list" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| format!("listing {}...", p))
            .unwrap_or_else(|| "listing files...".into()),
        "grep" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| format!("searching for \"{}\"...", p))
            .unwrap_or_else(|| "searching...".into()),
        "web_fetch" => input
            .get("url")
            .and_then(|v| v.as_str())
            .map(|u| format!("fetching {}...", u))
            .unwrap_or_else(|| "fetching web page...".into()),
        _ => format!("{}...", name),
    };
    // Dim gray
    println!("  \x1b[2m[{}]\x1b[0m", detail);
}

/// Print entity response with name label.
fn print_response(entity_name: &str, text: &str) {
    // Cyan entity name, then wrapped text
    println!("  \x1b[36m{}\x1b[0m \u{203a} {}", entity_name, text);
    println!();
}
