use crate::chat;
use crate::claude_provider::ClaudeProvider;
use crate::config::Config;
use crate::server::prompt;
use crate::tools::ToolRegistry;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    let api_key = config
        .resolve_api_key()
        .ok_or("No API key found. Set it in pulse-null.toml or PULSE_NULL_API_KEY env var.")?;

    let provider = ClaudeProvider::new(api_key, config.llm.model.clone());

    // Build system prompt from identity documents
    let root_dir = config.root_dir()?;
    let system_prompt = prompt::build_system_prompt(&root_dir, &config)?;

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
    chat::banner::render(&config, plugin_count);

    // Enter REPL
    chat::repl::run(&config, &provider, &tools, &system_prompt).await
}
