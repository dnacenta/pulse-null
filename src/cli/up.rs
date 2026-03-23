use std::sync::Arc;

use crate::config::Config;
use crate::server;

pub async fn run(headless: bool) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    if headless {
        // Headless mode: HTTP server only (for systemd services)
        tracing::info!(
            "Starting entity \"{}\" on {}:{} (headless)",
            config.entity.name,
            config.server.host,
            config.server.port
        );
        return server::start(config).await;
    }

    // TUI mode: launch full terminal application with splash screen
    let provider = crate::providers::create_streaming_provider(&config)?;
    let provider: Arc<dyn crate::streaming::StreamingProvider> = Arc::from(provider);

    let root_dir = config.root_dir()?;
    let system_prompt = crate::server::prompt::build_system_prompt(&root_dir, &config, None, None)?;

    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Box::new(crate::tools::file_read::FileReadTool::new(
        root_dir.clone(),
    )));
    tools.register(Box::new(crate::tools::file_write::FileWriteTool::new(
        root_dir.clone(),
    )));
    tools.register(Box::new(crate::tools::file_list::FileListTool::new(
        root_dir.clone(),
    )));
    tools.register(Box::new(crate::tools::grep::GrepTool::new(
        root_dir.clone(),
    )));
    tools.register(Box::new(crate::tools::web_fetch::WebFetchTool::new()));
    let tools = Arc::new(tools);

    crate::tui::run(
        Some(&config),
        Some(&root_dir),
        Some(provider),
        Some(tools),
        Some(&system_prompt),
    )
    .await
}
