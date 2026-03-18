use crate::config::Config;
use crate::praxis::PraxisConfig;

fn build_config() -> Result<PraxisConfig, Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    let plugin_table = config.plugins.get("praxis-echo");

    let claude_dir = plugin_table
        .and_then(|v| v.get("claude_dir"))
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root_dir.join("monitoring"));

    let docs_dir = plugin_table
        .and_then(|v| v.get("docs_dir"))
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root_dir.clone());

    Ok(PraxisConfig {
        claude_dir,
        docs_dir,
        thoughts_staleness_days: config.pipeline.thoughts_staleness_days,
        curiosity_staleness_days: config.pipeline.curiosity_staleness_days,
        freeze_threshold: config.pipeline.freeze_threshold,
        ..Default::default()
    })
}

pub async fn pulse() -> Result<(), Box<dyn std::error::Error>> {
    let config = build_config()?;
    crate::praxis::pulse::run_with_config(&config).map_err(|e| e.into())
}

pub async fn checkpoint() -> Result<(), Box<dyn std::error::Error>> {
    let config = build_config()?;
    crate::praxis::checkpoint::run(&config).map_err(|e| e.into())
}

pub async fn review() -> Result<(), Box<dyn std::error::Error>> {
    let config = build_config()?;
    crate::praxis::review::run_with_config(&config).map_err(|e| e.into())
}

pub async fn status() -> Result<(), Box<dyn std::error::Error>> {
    let config = build_config()?;
    crate::praxis::status::run(&config).map_err(|e| e.into())
}

pub async fn scan(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let config = build_config()?;
    let format = if json { "json" } else { "text" };
    crate::praxis::scan::run(&config, format).map_err(|e| e.into())
}

pub async fn archive(dry_run: bool) -> Result<(), Box<dyn std::error::Error>> {
    let config = build_config()?;
    crate::praxis::archive::run(&config, dry_run).map_err(|e| e.into())
}

pub async fn init() -> Result<(), Box<dyn std::error::Error>> {
    let config = build_config()?;
    crate::praxis::init::run(&config).map_err(|e| e.into())
}
