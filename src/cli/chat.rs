use crate::config::Config;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    // Build chat-echo config from [plugins.chat-echo] or defaults
    let chat_config = build_chat_config(&config);

    tracing::info!(
        "Starting chat UI on http://{}:{}",
        chat_config.host,
        chat_config.port
    );

    println!(
        "\n  Chat UI available at http://{}:{}\n",
        chat_config.host, chat_config.port
    );

    let mut chat = chat_echo::ChatEcho::new(chat_config);
    chat.start()
        .await
        .map_err(|e| format!("chat-echo failed to start: {e}"))?;

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;

    tracing::info!("Shutting down chat UI...");
    chat.stop()
        .await
        .map_err(|e| format!("chat-echo failed to stop: {e}"))?;

    Ok(())
}

fn build_chat_config(config: &Config) -> chat_echo::config::Config {
    match config.plugins.get("chat-echo") {
        Some(toml_val) => {
            let table = toml_val.as_table();
            let t = table.cloned().unwrap_or_default();
            chat_echo::config::Config {
                host: t
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("127.0.0.1")
                    .to_string(),
                port: t.get("port").and_then(|v| v.as_integer()).unwrap_or(8080) as u16,
                bridge_url: t
                    .get("bridge_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&format!(
                        "http://{}:{}",
                        config.server.host, config.server.port
                    ))
                    .to_string(),
                bridge_secret: t
                    .get("bridge_secret")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| config.security.secret.clone()),
                static_dir: t
                    .get("static_dir")
                    .and_then(|v| v.as_str())
                    .unwrap_or("./static")
                    .to_string(),
            }
        }
        None => {
            // No explicit config — derive from server config
            chat_echo::config::Config {
                host: "127.0.0.1".to_string(),
                port: 8080,
                bridge_url: format!("http://{}:{}", config.server.host, config.server.port),
                bridge_secret: config.security.secret.clone(),
                static_dir: "./static".to_string(),
            }
        }
    }
}
