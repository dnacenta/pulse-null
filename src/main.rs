use clap::{Parser, Subcommand};

mod cli;
mod config;
mod init;
mod llm;
mod memory;
mod scheduler;
mod server;

#[derive(Parser)]
#[command(name = "echo-system")]
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
    /// Stop the entity
    Down,
    /// Show entity status
    Status,
    /// Manage scheduled tasks
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "echo_system=info".into()),
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
    }
}
