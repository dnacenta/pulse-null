use crate::config::Config;
use crate::pidfile;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    println!("Entity: {}", config.entity.name);
    println!(
        "Owner: {} ({})",
        config.entity.owner_name, config.entity.owner_alias
    );
    println!("LLM: {}", config.llm.provider);
    println!("Server: {}:{}", config.server.host, config.server.port);

    // Plugins
    if config.plugins.is_empty() {
        println!("Plugins: none");
    } else {
        let names: Vec<&String> = config.plugins.keys().collect();
        println!(
            "Plugins: {}",
            names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Check PID file first, then fall back to health endpoint
    let status = match pidfile::read(&root_dir) {
        Some(pid) if pidfile::is_alive(pid) => format!("RUNNING (pid {})", pid),
        Some(pid) => {
            pidfile::remove(&root_dir);
            format!("STOPPED (stale pid {})", pid)
        }
        None => {
            // No PID file — try health endpoint as fallback
            let url = format!(
                "http://{}:{}/health",
                config.server.host, config.server.port
            );
            match reqwest::get(&url).await {
                Ok(resp) if resp.status().is_success() => "RUNNING".to_string(),
                _ => "STOPPED".to_string(),
            }
        }
    };

    println!("Status: {}", status);
    Ok(())
}
