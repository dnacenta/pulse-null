use clap::{Parser, Subcommand};

mod chat;
mod claude_provider;
mod cli;
mod config;
mod context;
mod events;
mod init;
mod pidfile;
mod plugins;
mod scheduler;
mod server;
mod session;
mod tools;

#[derive(Parser)]
#[command(name = "pulse-null")]
#[command(about = "One binary. One command. Your own AI entity.")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new entity
    Init {
        /// Directory to create the entity in (defaults to current directory)
        #[arg(short, long)]
        dir: Option<String>,
    },
    /// Start the entity
    Up,
    /// Talk to your entity in the terminal
    Chat,
    /// Stop the entity
    Down,
    /// Show entity status
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
    /// Memory dashboard and tools
    Recall {
        #[command(subcommand)]
        action: Option<RecallAction>,
    },
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
enum RecallAction {
    /// Quick memory health check
    Status,
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pulse_null=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { dir } => {
            if let Err(e) = cli::init::run(dir).await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Up => {
            if let Err(e) = cli::up::run().await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Chat => {
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
                Some(RecallAction::Status) => cli::recall::status_cmd().await,
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
    }
}
