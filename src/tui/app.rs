use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use crate::config::Config;
use crate::peer::PeerClient;
use crate::streaming::StreamingProvider;
use crate::tools::ToolRegistry;

/// Shared state accessible to all screens.
pub struct AppContext {
    pub config: Option<Config>,
    pub entity_name: Option<String>,
    pub model_name: Option<String>,
    pub root_dir: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub provider: Option<Arc<dyn StreamingProvider>>,
    pub tools: Option<Arc<ToolRegistry>>,
    pub system_prompt: Option<String>,
    pub peer_client: Option<Arc<Mutex<PeerClient>>>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub session_start: Instant,
}

impl AppContext {
    pub fn new(
        config: Option<Config>,
        root_dir: Option<PathBuf>,
        provider: Option<Arc<dyn StreamingProvider>>,
        tools: Option<Arc<ToolRegistry>>,
        system_prompt: Option<String>,
    ) -> Self {
        let entity_name = config.as_ref().map(|c| c.entity.name.clone());
        let model_name = config.as_ref().map(|c| c.llm.model.clone());

        let peer_client = config
            .as_ref()
            .map(|c| Arc::new(Mutex::new(PeerClient::new(c.peers.clone()))));

        let config_path = Config::find_config().ok();

        Self {
            config,
            entity_name,
            model_name,
            root_dir,
            config_path,
            provider,
            tools,
            system_prompt,
            peer_client,
            tokens_in: 0,
            tokens_out: 0,
            session_start: Instant::now(),
        }
    }
}
