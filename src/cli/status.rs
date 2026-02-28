use crate::config::Config;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    println!("Entity: {}", config.entity.name);
    println!(
        "Owner: {} ({})",
        config.entity.owner_name, config.entity.owner_alias
    );
    println!("LLM: {}", config.llm.provider);
    println!("Server: {}:{}", config.server.host, config.server.port);

    // Check if server is running
    let url = format!(
        "http://{}:{}/health",
        config.server.host, config.server.port
    );
    match reqwest::get(&url).await {
        Ok(resp) if resp.status().is_success() => {
            println!("Status: RUNNING");
        }
        _ => {
            println!("Status: STOPPED");
        }
    }

    Ok(())
}
