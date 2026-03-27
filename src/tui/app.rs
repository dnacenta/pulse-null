use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, RwLock};

use crate::config::Config;
use crate::events::EventBus;
use crate::peer::PeerClient;
use crate::registry::EntityRegistry;
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
    pub event_bus: Option<Arc<EventBus>>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub session_start: Instant,

    // Multi-entity state (used by comms tab for local peer auto-discovery)
    #[allow(dead_code)]
    pub registry: Option<Arc<RwLock<EntityRegistry>>>,
    #[allow(dead_code)]
    pub entity_home: Option<PathBuf>,
}

impl AppContext {
    /// Create context for single-entity mode (existing behavior).
    pub fn new(
        config: Option<Config>,
        root_dir: Option<PathBuf>,
        provider: Option<Arc<dyn StreamingProvider>>,
        tools: Option<Arc<ToolRegistry>>,
        system_prompt: Option<String>,
        event_bus: Option<Arc<EventBus>>,
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
            event_bus,
            tokens_in: 0,
            tokens_out: 0,
            session_start: Instant::now(),
            registry: None,
            entity_home: None,
        }
    }

    /// Create context for multi-entity mode (no entity selected yet).
    pub fn new_multi(registry: Arc<RwLock<EntityRegistry>>, entity_home: PathBuf) -> Self {
        Self {
            config: None,
            entity_name: None,
            model_name: None,
            root_dir: None,
            config_path: None,
            provider: None,
            tools: None,
            system_prompt: None,
            peer_client: None,
            event_bus: None,
            tokens_in: 0,
            tokens_out: 0,
            session_start: Instant::now(),
            registry: Some(registry),
            entity_home: Some(entity_home),
        }
    }

    /// Load entity-specific state into the context.
    pub fn load_entity(
        &mut self,
        config: &Config,
        root_dir: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let provider = crate::providers::create_streaming_provider(config)?;
        let provider: Arc<dyn StreamingProvider> = Arc::from(provider);

        let system_prompt =
            crate::server::prompt::build_system_prompt(root_dir, config, None, None)?;

        let mut tools = ToolRegistry::new();
        tools.register(Box::new(crate::tools::file_read::FileReadTool::new(
            root_dir.to_path_buf(),
        )));
        tools.register(Box::new(crate::tools::file_write::FileWriteTool::new(
            root_dir.to_path_buf(),
        )));
        tools.register(Box::new(crate::tools::file_list::FileListTool::new(
            root_dir.to_path_buf(),
        )));
        tools.register(Box::new(crate::tools::grep::GrepTool::new(
            root_dir.to_path_buf(),
        )));
        tools.register(Box::new(crate::tools::web_fetch::WebFetchTool::new()));
        #[cfg(feature = "graph")]
        if config.graph.enabled {
            tools.register(Box::new(crate::tools::graph_query::GraphQueryTool::new(
                root_dir.to_path_buf(),
            )));
        }

        let peer_client = Arc::new(Mutex::new(PeerClient::new(config.peers.clone())));

        self.config = Some(config.clone());
        self.entity_name = Some(config.entity.name.clone());
        self.model_name = Some(config.llm.model.clone());
        self.root_dir = Some(root_dir.to_path_buf());
        self.config_path = Some(root_dir.join("pulse-null.toml"));
        self.provider = Some(provider);
        self.tools = Some(Arc::new(tools));
        self.system_prompt = Some(system_prompt);
        self.peer_client = Some(peer_client);
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.session_start = Instant::now();

        Ok(())
    }

    /// Clear entity-specific state (when returning to welcome screen).
    pub fn unload_entity(&mut self) {
        self.config = None;
        self.entity_name = None;
        self.model_name = None;
        self.root_dir = None;
        self.config_path = None;
        self.provider = None;
        self.tools = None;
        self.system_prompt = None;
        self.peer_client = None;
        self.event_bus = None;
        self.tokens_in = 0;
        self.tokens_out = 0;
    }
}
