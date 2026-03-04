use std::io::{self, BufRead, Write};
use std::path::Path;

use echo_system_types::llm::{ContentBlock, LmProvider, Message, MessageContent, Role, StopReason};

use crate::chat;
use crate::config::Config;
use crate::tools::ToolRegistry;

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

        // Add user message to conversation
        conversation.push(Message {
            role: Role::User,
            content: MessageContent::Text(input.to_string()),
        });

        // Compact conversation if approaching context budget
        crate::context::compact_if_needed(
            &mut conversation,
            provider,
            config.llm.context_budget,
            config.llm.max_tokens,
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

            // Add assistant response to conversation
            conversation.push(Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(result.content.clone()),
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

                    // Add tool results and loop
                    conversation.push(Message {
                        role: Role::User,
                        content: MessageContent::Blocks(tool_results),
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

    // Save session to EPHEMERAL.md
    save_session(root_dir, entity_name, &conversation);

    Ok(())
}

/// Save a brief session summary to EPHEMERAL.md.
fn save_session(root_dir: &Path, entity_name: &str, conversation: &[Message]) {
    // Only save if there was actual conversation
    let message_count = conversation.len();
    if message_count == 0 {
        return;
    }

    let ephemeral_path = root_dir.join("EPHEMERAL.md");

    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");

    // Collect user messages for the summary
    let user_messages: Vec<&str> = conversation
        .iter()
        .filter_map(|m| {
            if matches!(m.role, Role::User) {
                if let MessageContent::Text(ref t) = m.content {
                    Some(t.as_str())
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    let topics: Vec<&str> = user_messages.iter().take(5).copied().collect();

    let mut content = format!("## CLI Chat Session — {}\n\n", now);
    content.push_str(&format!(
        "Conversation with {} ({} messages)\n\n",
        entity_name, message_count
    ));
    content.push_str("### Topics discussed\n\n");
    for topic in &topics {
        // Truncate long messages
        let display = if topic.len() > 80 {
            format!("{}...", &topic[..77])
        } else {
            topic.to_string()
        };
        content.push_str(&format!("- {}\n", display));
    }
    if user_messages.len() > 5 {
        content.push_str(&format!("- ...and {} more\n", user_messages.len() - 5));
    }

    if let Err(e) = std::fs::write(&ephemeral_path, content) {
        eprintln!("  \x1b[33mwarning\x1b[0m  could not save session: {}", e);
    } else {
        println!("  \x1b[2msession saved to EPHEMERAL.md\x1b[0m");
    }
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
