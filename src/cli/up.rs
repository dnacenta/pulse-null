use crate::config::Config;
use crate::server;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    tracing::info!(
        "Starting entity \"{}\" on {}:{}",
        config.entity.name,
        config.server.host,
        config.server.port
    );

    server::start(config).await
}
