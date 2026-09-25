use clap::{Parser, Subcommand};

mod anthropic_provider;
mod caliber;
mod cli;
mod cli_provider;
mod config;
mod context;
mod context_buffer;
mod coordinator;
mod discovery;
mod errors;
mod events;
mod graph_context;
mod graph_feedback;
mod init;
mod intake_audit;
mod interaction;
mod ledger;
mod logbook;
mod ollama_provider;
mod outreach;
mod peer;
mod persist;
mod pidfile;
mod plugins;
mod praxis;
mod prediction;
mod provider_status;
mod providers;
mod registry;
mod scheduler;
mod server;
mod session;
mod session_health;
mod session_store;
mod streaming;
mod surrealdb_manager;
mod task_context;
mod tension;
mod tool_loop;
mod tools;
mod tui;
mod utils;
mod vigil;
mod wal;
mod wire;

#[derive(Parser)]
#[command(name = "pulse-null")]
#[command(about = "One binary. One command. Your own AI pulse.")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new pulse
    Init {
        /// Directory to create the pulse in (defaults to current directory)
        #[arg(short, long)]
        dir: Option<String>,
    },
    /// Start the pulse
    Up {
        /// Run in headless mode (HTTP server only, no TUI). Use for systemd services.
        #[arg(long)]
        headless: bool,
    },
    /// Talk to your pulse in the terminal
    Chat,
    /// Stop the pulse
    Down,
    /// Show pulse status
    Status,
    /// Manage scheduled tasks
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    /// Pipeline health and document tracking
    Pipeline {
        #[command(subcommand)]
        action: PipelineAction,
    },
    /// Manage document archives
    Archive {
        #[command(subcommand)]
        action: ArchiveAction,
    },
    /// Manage plugins
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Manage the intent queue
    Intent {
        #[command(subcommand)]
        action: IntentAction,
    },
    /// Interest-triggered outreach — caps, response rates, rejections
    Outreach {
        #[command(subcommand)]
        action: OutreachAction,
    },
    /// Memory dashboard and tools
    Recall {
        #[command(subcommand)]
        action: Option<RecallAction>,
    },
    /// Pipeline enforcement (praxis-echo)
    Praxis {
        #[command(subcommand)]
        action: PraxisAction,
    },
    /// Metacognitive monitoring (vigil-echo)
    Vigil {
        #[command(subcommand)]
        action: VigilAction,
    },
    /// Verify and repair the entity's agent-CLI integration (instruction file, hooks, config)
    Repair,
    /// Isolation mode — the minimal-core diagnostic retreat
    Isolate {
        #[command(subcommand)]
        action: IsolateAction,
    },
}

#[derive(Subcommand)]
enum IsolateAction {
    /// Enter isolation mode (sticky until 'off')
    On {
        /// Optional reason, recorded in the marker
        #[arg(long)]
        reason: Option<String>,
    },
    /// Exit isolation mode
    Off,
    /// Show isolation status
    Status,
}

#[derive(Subcommand)]
enum ScheduleAction {
    /// List all scheduled tasks
    List,
    /// Add a new scheduled task
    Add {
        /// Task name
        name: String,
        /// Cron expression (6-field: sec min hour dom month dow)
        #[arg(long)]
        cron: String,
        /// Prompt to send to the LLM
        #[arg(long)]
        prompt: String,
    },
    /// Remove a scheduled task
    Remove {
        /// Task ID
        id: String,
    },
    /// Enable a scheduled task
    Enable {
        /// Task ID
        id: String,
    },
    /// Disable a scheduled task
    Disable {
        /// Task ID
        id: String,
    },
    /// Pin a task to a specific model, overriding [llm] model for that task
    Model {
        /// Task ID
        id: String,
        /// Model name (omit with --clear to follow [llm] model again)
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        model: Option<String>,
        /// Remove the override so the task follows [llm] model
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Subcommand)]
enum PipelineAction {
    /// Show document counts and threshold status
    Health,
    /// List stale documents that need attention
    Stale,
}

#[derive(Subcommand)]
enum PluginAction {
    /// List available and installed plugins
    List,
    /// Add a plugin
    Add {
        /// Plugin name
        name: String,
    },
    /// Remove a plugin
    Remove {
        /// Plugin name
        name: String,
    },
}

#[derive(Subcommand)]
enum IntentAction {
    /// List queued intents
    List,
    /// Add a one-shot intent to the queue
    Add {
        /// Description of the intent
        description: String,
        /// Prompt to send to the LLM
        #[arg(long)]
        prompt: String,
        /// Priority: low, normal, high, urgent
        #[arg(long, default_value = "normal")]
        priority: String,
    },
    /// Remove an intent from the queue
    Remove {
        /// Intent ID
        id: String,
    },
    /// Clear all pending intents
    Clear,
}

#[derive(Subcommand)]
enum OutreachAction {
    /// Show caps, response rates, and recent gate rejections
    Status,
    /// Record that D responded to an outreach message
    Respond {
        /// Outreach message ID (from `outreach status`)
        id: String,
        /// Optional explicit rating: useful or noise
        #[arg(long)]
        rating: Option<String>,
    },
}

#[derive(Subcommand)]
enum RecallAction {
    /// Search conversation archives
    Search {
        /// Search query
        query: String,
        /// Use ranked scoring instead of line-level matches
        #[arg(long)]
        ranked: bool,
    },
    /// Analyze and auto-distill MEMORY.md
    Distill,
}

#[derive(Subcommand)]
enum PraxisAction {
    /// Inject pipeline state at session start
    Pulse,
    /// Save checkpoint before context compaction
    Checkpoint,
    /// Review pipeline changes at session end
    Review,
    /// Show pipeline health dashboard
    Status,
    /// Deep scan of all documents
    Scan {
        /// Output as JSON instead of human-readable
        #[arg(long)]
        json: bool,
    },
    /// Check archive thresholds
    Archive {
        /// Dry run — show what would be archived
        #[arg(long)]
        dry_run: bool,
    },
    /// Initialize praxis pipeline
    Init,
}

#[derive(Subcommand)]
enum VigilAction {
    /// Inject cognitive health assessment at session start
    Pulse,
    /// Extract signals from identity documents
    Collect {
        /// Trigger source for logging
        #[arg(long, default_value = "manual")]
        trigger: String,
    },
    /// Show vigil health dashboard
    Status {
        /// Output as JSON instead of human-readable
        #[arg(long)]
        json: bool,
    },
    /// Initialize vigil monitoring
    Init,
}

#[derive(Subcommand)]
enum ArchiveAction {
    /// List archived files
    List {
        /// Filter by document type (learning, thoughts, curiosity, reflections, praxis)
        #[arg(long)]
        document: Option<String>,
    },
    /// Manually archive a document
    Run {
        /// Document to archive (learning, thoughts, curiosity, reflections, praxis)
        document: String,
    },
}

/// Where the TUI sends its logs: stdout belongs to the screen while the TUI
/// runs, so `pulse-null up` (without `--headless`) and `pulse-null chat` log
/// to `logs/tui.log` under the pulse root (where `pulse-null.toml` lives;
/// the current directory when none is found). The file is owner-only and a
/// symlink in its place is refused, since the log can carry request details.
fn open_tui_log() -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    // Inside a pulse: its logs/. Elsewhere (Home): the user's state dir,
    // never the current directory.
    let dir = match config::Config::find_config()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    {
        Some(root) => root.join("logs"),
        None => std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
            })
            .ok_or_else(|| "no HOME to log under".to_string())?
            .join("pulse-null"),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("tui.log");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "pulse_null=info".into());
    let tui_mode = matches!(
        cli.command,
        Commands::Up { headless: false } | Commands::Chat
    );
    if tui_mode {
        match open_tui_log() {
            Ok(file) => tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Arc::new(file))
                .init(),
            Err(e) => {
                eprintln!("pulse-null: cannot open the TUI log ({e}); logging is off for this run");
                tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(std::io::sink)
                    .init();
            }
        }
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    match cli.command {
        Commands::Init { dir } => {
            if let Err(e) = cli::root_guard::refuse_root("init") {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
            if let Err(e) = cli::init::run(dir).await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Up { headless } => {
            if let Err(e) = cli::root_guard::refuse_root("up") {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
            if let Err(e) = cli::up::run(headless).await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Chat => {
            if let Err(e) = cli::root_guard::refuse_root("chat") {
                eprintln!("{e}");
                std::process::exit(1);
            }
            if let Err(e) = cli::chat::run().await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Down => {
            if let Err(e) = cli::down::run().await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Status => {
            if let Err(e) = cli::status::run().await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Schedule { action } => {
            let result = match action {
                ScheduleAction::List => cli::schedule::list().await,
                ScheduleAction::Add { name, cron, prompt } => {
                    cli::schedule::add(name, cron, prompt).await
                }
                ScheduleAction::Remove { id } => cli::schedule::remove(id).await,
                ScheduleAction::Enable { id } => cli::schedule::enable(id).await,
                ScheduleAction::Disable { id } => cli::schedule::disable(id).await,
                ScheduleAction::Model { id, model, clear } => {
                    cli::schedule::set_model(id, if clear { None } else { model }).await
                }
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Pipeline { action } => {
            let result = match action {
                PipelineAction::Health => cli::pipeline::health_cmd().await,
                PipelineAction::Stale => cli::pipeline::stale_cmd().await,
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Archive { action } => {
            let result = match action {
                ArchiveAction::List { document } => cli::archive::list(document).await,
                ArchiveAction::Run { document } => cli::archive::run(document).await,
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Plugin { action } => {
            let result = match action {
                PluginAction::List => cli::plugin::list().await,
                PluginAction::Add { name } => cli::plugin::add(name).await,
                PluginAction::Remove { name } => cli::plugin::remove(name).await,
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Recall { action } => {
            let result = match action {
                None => cli::recall::dashboard_cmd().await,
                Some(RecallAction::Search { query, ranked }) => {
                    cli::recall::search(query, ranked).await
                }
                Some(RecallAction::Distill) => cli::recall::distill().await,
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Intent { action } => {
            let result = match action {
                IntentAction::List => cli::intent::list().await,
                IntentAction::Add {
                    description,
                    prompt,
                    priority,
                } => cli::intent::add(description, prompt, priority).await,
                IntentAction::Remove { id } => cli::intent::remove(id).await,
                IntentAction::Clear => cli::intent::clear().await,
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Outreach { action } => {
            let result = match action {
                OutreachAction::Status => cli::outreach::status().await,
                OutreachAction::Respond { id, rating } => cli::outreach::respond(id, rating).await,
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Praxis { action } => {
            let result = match action {
                PraxisAction::Pulse => cli::praxis::pulse().await,
                PraxisAction::Checkpoint => cli::praxis::checkpoint().await,
                PraxisAction::Review => cli::praxis::review().await,
                PraxisAction::Status => cli::praxis::status().await,
                PraxisAction::Scan { json } => cli::praxis::scan(json).await,
                PraxisAction::Archive { dry_run } => cli::praxis::archive(dry_run).await,
                PraxisAction::Init => cli::praxis::init().await,
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Vigil { action } => {
            let result = match action {
                VigilAction::Pulse => cli::vigil::pulse().await,
                VigilAction::Collect { trigger } => cli::vigil::collect(trigger).await,
                VigilAction::Status { json } => cli::vigil::status(json).await,
                VigilAction::Init => cli::vigil::init().await,
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Repair => {
            if let Err(e) = cli::root_guard::refuse_root("repair") {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
            if let Err(e) = cli::repair::run().await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Isolate { action } => {
            let result = match action {
                IsolateAction::On { reason } => cli::isolate::on(reason).await,
                IsolateAction::Off => cli::isolate::off().await,
                IsolateAction::Status => cli::isolate::status().await,
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }
}
