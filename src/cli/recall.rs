use recall_echo::RecallEcho;

use crate::config::Config;

/// Build a RecallEcho instance with pulse-null's directory layout.
fn build_recall(config: &Config) -> Result<RecallEcho, Box<dyn std::error::Error>> {
    let root_dir = config.root_dir()?;
    Ok(RecallEcho::new(root_dir))
}

pub async fn dashboard_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let recall = build_recall(&config)?;
    let status = recall.health();
    println!("Memory health: {:?}", status);
    Ok(())
}

pub async fn search(query: String, ranked: bool) -> Result<(), Box<dyn std::error::Error>> {
    if ranked {
        recall_echo::search::run_ranked(&query, 10).map_err(|e| e.into())
    } else {
        recall_echo::search::run(&query, 2).map_err(|e| e.into())
    }
}

pub async fn distill() -> Result<(), Box<dyn std::error::Error>> {
    recall_echo::distill::run().map_err(|e| e.into())
}
