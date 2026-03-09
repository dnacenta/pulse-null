use crate::chat;
use crate::config::Config;
use crate::providers;
use crate::server::prompt;
use crate::tools::ToolRegistry;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    let provider = providers::create_provider(&config)?;

    // Build system prompt from identity documents
    let root_dir = config.root_dir()?;
    let system_prompt = prompt::build_system_prompt(&root_dir, &config, None, None)?;

    // Register tools
    let mut tools = ToolRegistry::new();
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

    // Render banner
    let plugin_count = config.plugins.len();
    chat::banner::render(&config, &root_dir, plugin_count);

    // Enter REPL
    chat::repl::run(
        &config,
        &root_dir,
        provider.as_ref(),
        &tools,
        &system_prompt,
    )
    .await
}
