use clap::{Parser, Subcommand};

mod anthropic_provider;
mod caliber;
mod cli;
mod cli_provider;
mod comms;
mod config;
mod context;
mod context_buffer;
mod coordinator;
mod discovery;
mod errors;
mod events;
mod fs_safe;
mod graph_context;
mod graph_extract;
mod graph_feedback;
mod init;
mod intake_audit;
mod interaction;
mod ledger;
mod logbook;
mod ollama_provider;
mod oneself;
mod outreach;
mod peer;
mod persist;
mod pidfile;
mod plugins;
mod praxis;
mod prediction;
mod process_state;
mod provider_status;
mod providers;
mod registry;
mod scheduler;
mod server;
mod session;
mod session_health;
mod session_key;
mod session_store;
mod speech_gate;
mod streaming;
mod surrealdb_manager;
mod task_context;
mod tension;
mod tool_loop;
mod tool_surface;
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

/// Every subcommand. The grouped variants are flattened, so the command line
/// (names, order, help) is the flat list it always was; the groups only keep
/// each dispatcher small.
#[derive(Subcommand)]
enum Commands {
    #[command(flatten)]
    Lifecycle(LifecycleCommand),
    #[command(flatten)]
    Work(WorkCommand),
    #[command(flatten)]
    Cognition(CognitionCommand),
    /// Verify and repair the entity's agent-CLI integration (instruction file, hooks, config)
    Repair,
    /// Isolation mode — the minimal-core diagnostic retreat
    Isolate {
        #[command(subcommand)]
        action: IsolateAction,
    },
    /// Growth-record probes, run in sandboxed scratch copies of the pulse
    Probe {
        #[command(subcommand)]
        action: ProbeAction,
    },
    /// Read a receipt file
    Receipts {
        #[command(subcommand)]
        action: ReceiptsAction,
    },
}

/// `pulse-null receipts`.
#[derive(Subcommand)]
enum ReceiptsAction {
    /// Check the hash chain
    Verify {
        /// Receipt file. Default: `[receipts] path`, or
        /// `/var/lib/pulse-null/<pulse>/receipts.jsonl`.
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Print one JSON object
        #[arg(long)]
        json: bool,
        /// Compare anchored `(seq, head)` values. Publishes nothing.
        #[arg(long)]
        anchors: bool,
        /// Anchor config. Default: `/etc/pulse-null/anchor.toml`.
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// Token file. Overrides the config file and the environment.
        #[arg(long)]
        token_file: Option<std::path::PathBuf>,
    },
    /// Print rows
    Show {
        /// Receipt file. Default: `[receipts] path`, or
        /// `/var/lib/pulse-null/<pulse>/receipts.jsonl`.
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Keep rows whose `ts` is at least this string
        #[arg(long)]
        since: Option<String>,
        /// Keep rows of this `e.kind`
        #[arg(long)]
        kind: Option<String>,
        /// Print at most this many matching rows, the latest ones
        #[arg(long)]
        limit: Option<u64>,
        /// Print one JSON object
        #[arg(long)]
        json: bool,
    },
    /// Open intervals that overlap
    Overlaps {
        /// Receipt file. Default: `[receipts] path`, or
        /// `/var/lib/pulse-null/<pulse>/receipts.jsonl`.
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Drop an overlap that ended before this instant
        #[arg(long)]
        since: Option<String>,
        /// Print at most this many overlaps. 0 lists every overlap.
        #[arg(long, default_value_t = crate::cli::receipts::DEFAULT_OVERLAP_LIMIT)]
        limit: u64,
        /// Print one JSON object
        #[arg(long)]
        json: bool,
    },
    /// Provider, adapter, or model changes between boots or closes
    Swaps {
        /// Receipt file. Default: `[receipts] path`, or
        /// `/var/lib/pulse-null/<pulse>/receipts.jsonl`.
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Print one JSON object
        #[arg(long)]
        json: bool,
    },
    /// Append one anchor line when this receipt file is ahead
    Anchor {
        /// Pulse name. Must match the genesis row.
        #[arg(long)]
        pulse: String,
        /// Receipt file. Default: `[receipts] path`, or
        /// `/var/lib/pulse-null/<pulse>/receipts.jsonl`.
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Anchor config. Default: `/etc/pulse-null/anchor.toml`.
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// Print the line and publish nothing.
        #[arg(long)]
        dry_run: bool,
        /// Token file. Overrides the config file and the environment.
        #[arg(long)]
        token_file: Option<std::path::PathBuf>,
    },
    /// Create the receipt directory, the receipt file, and anchors.log
    Install {
        /// Pulse name. Letters, digits, `-` or `_`.
        #[arg(long)]
        pulse: String,
        /// Group that may read the receipt file. Letters, digits, `-` or `_`.
        #[arg(long)]
        group: String,
        /// Parent of the pulse directory.
        #[arg(long, default_value = "/var/lib/pulse-null")]
        dir: std::path::PathBuf,
        /// Copy a non-root receipt file onto a new root-owned inode when it
        /// verifies for this pulse. A root-owned file is not replaced; its
        /// owner, mode, and append-only flag are corrected. Stop the pulse
        /// first.
        #[arg(long)]
        adopt: bool,
    },
}

/// Creating, starting, talking to and stopping a pulse.
#[derive(Subcommand)]
enum LifecycleCommand {
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
}

/// The pulse's work: its schedule, document pipeline, plugins, intents and outreach.
#[derive(Subcommand)]
enum WorkCommand {
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
}

/// The cognitive subsystems: memory (recall), pipeline enforcement (praxis)
/// and metacognitive monitoring (vigil).
#[derive(Subcommand)]
enum CognitionCommand {
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
}

#[derive(Subcommand)]
enum ProbeFactsAction {
    /// Draw the next fact to mention to the pulse (the runtime picks the pair and side)
    Draw,
    /// Record the value you mentioned; it must appear in your conversation since the draw
    Plant {
        /// The fact id `draw` printed
        fact: String,
        /// The value you said, as you said it
        value: String,
    },
    /// List drawn facts and their asks
    List,
}

#[derive(Subcommand)]
enum ProbeSetsAction {
    /// Install the reviewed, sealed item sets into the held-out record
    /// (every file must match the digest this binary was built with)
    Install {
        /// The directory holding the reviewed set files
        #[arg(long)]
        from: std::path::PathBuf,
        /// The pulse the sets are for (its directory name: echo, synth)
        #[arg(long)]
        pulse: String,
        /// The install root (default /etc/pulse-null-probe/sets)
        #[arg(long)]
        dest: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum ProbeAction {
    /// Run the probe sets in sandboxed scratch copies and record the results
    Run {
        /// Run only if a scheduled run is due (the systemd timer's mode)
        #[arg(long)]
        if_due: bool,
        /// Run only these sets (by name or prefix)
        #[arg(long, num_args = 1..)]
        only: Vec<String>,
        /// Leave the scratch directory on disk for inspection
        #[arg(long)]
        keep_scratch: bool,
    },
    /// Turns and tokens spent against the budget, cost per run, results by lineage
    Status,
    /// Paired facts: draw one, record its plant, list them
    Facts {
        #[command(subcommand)]
        action: ProbeFactsAction,
    },
    /// The sealed item sets: install the reviewed copies
    Sets {
        #[command(subcommand)]
        action: ProbeSetsAction,
    },
    /// Export the live graph for the next probe run (the export unit's step)
    ExportGraph {
        /// Export only when a scheduled run would go ahead. A refusal exits 0
        /// and does not contact the live server (the export unit's mode).
        #[arg(long)]
        if_due: bool,
    },
    /// Show that a probe item writes nowhere live (sandbox self-test; --turn adds two real turns)
    VerifyIsolation {
        /// Also run a canned turn in the scratch copy and a fresh-arm date check (two turns)
        #[arg(long)]
        turn: bool,
        /// Leave the scratch directory on disk for inspection
        #[arg(long)]
        keep_scratch: bool,
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

/// What a command returns: the error is printed after `Error: `.
type CommandResult = Result<(), Box<dyn std::error::Error>>;

/// A routed command that has not started: building one runs nothing, so the
/// routing can be checked without touching a pulse.
type CommandFuture = std::pin::Pin<Box<dyn std::future::Future<Output = CommandResult>>>;

fn main() {
    let listen = oneself::store::take_listen_env();
    oneself::store::retain_listen(listen.clone());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async_main(listen));
}

async fn async_main(listen: oneself::store::ListenEnv) {
    let cli = Cli::parse();
    init_tracing(&cli.command);
    if let Err(message) = run_command(cli.command, cli::root_guard::refuse_root, listen).await {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

/// Logging: the TUI commands write to the TUI log (stdout is the screen),
/// everything else to stderr so a `--json` report on stdout stays intact.
fn init_tracing(command: &Commands) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "pulse_null=info".into());
    if writes_tui_log(command) {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(tui_log_writer(open_tui_log()))
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }
}

/// Whether `command` runs the TUI, which owns stdout while it runs.
fn writes_tui_log(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Lifecycle(LifecycleCommand::Up { headless: false } | LifecycleCommand::Chat)
    )
}

/// Where TUI logs go: the opened log file, or nowhere (with a warning) when
/// it could not be opened — never the screen.
fn tui_log_writer(
    log: Result<std::fs::File, String>,
) -> tracing_subscriber::fmt::writer::BoxMakeWriter {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;
    match log {
        Ok(file) => BoxMakeWriter::new(std::sync::Arc::new(file)),
        Err(e) => {
            eprintln!("pulse-null: cannot open the TUI log ({e}); logging is off for this run");
            BoxMakeWriter::new(std::io::sink)
        }
    }
}

/// Run `command` once its root check passes. The error is the whole message
/// to print before exiting 1.
async fn run_command(
    command: Commands,
    refuse_root: impl FnOnce(&str) -> Result<(), String>,
    listen: oneself::store::ListenEnv,
) -> Result<(), String> {
    if let Some(refusal) = root_check(&command).and_then(|check| check.refusal(refuse_root)) {
        return Err(refusal);
    }
    dispatch(command, listen).await.map_err(|err| {
        if let Some(exit) = err.downcast_ref::<cli::receipts::PrintedExit>() {
            let code = exit.0;
            std::process::exit(code);
        }
        format!("Error: {err}")
    })
}

/// A command's refusal to run as root, checked before it does any work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RootCheck {
    /// The name the refusal message gives the command.
    command: &'static str,
    /// What the refusal is printed after: `Error: `, or nothing for the
    /// commands whose message stands alone.
    prefix: &'static str,
}

impl RootCheck {
    const fn prefixed(command: &'static str) -> Self {
        Self {
            command,
            prefix: "Error: ",
        }
    }

    const fn bare(command: &'static str) -> Self {
        Self {
            command,
            prefix: "",
        }
    }

    /// The message to exit with when `refuse_root` refuses this command.
    fn refusal(self, refuse_root: impl FnOnce(&str) -> Result<(), String>) -> Option<String> {
        refuse_root(self.command)
            .err()
            .map(|e| format!("{}{e}", self.prefix))
    }
}

/// The root check `command` makes, if it makes one.
fn root_check(command: &Commands) -> Option<RootCheck> {
    match command {
        Commands::Lifecycle(LifecycleCommand::Init { .. }) => Some(RootCheck::prefixed("init")),
        Commands::Lifecycle(LifecycleCommand::Up { .. }) => Some(RootCheck::prefixed("up")),
        Commands::Lifecycle(LifecycleCommand::Chat) => Some(RootCheck::bare("chat")),
        Commands::Repair => Some(RootCheck::prefixed("repair")),
        // Every probe command runs as the pulse's user, except the
        // sealed-set install, which writes a root-only directory.
        Commands::Probe {
            action: ProbeAction::Sets { .. },
        } => None,
        Commands::Probe { .. } => Some(RootCheck::bare("probe")),
        _ => None,
    }
}

fn dispatch(command: Commands, listen: oneself::store::ListenEnv) -> CommandFuture {
    match command {
        Commands::Lifecycle(command) => lifecycle_command(command, listen),
        Commands::Work(command) => {
            drop(listen);
            work_command(command)
        }
        Commands::Cognition(command) => {
            drop(listen);
            cognition_command(command)
        }
        Commands::Repair => {
            drop(listen);
            Box::pin(cli::repair::run())
        }
        Commands::Isolate { action } => {
            drop(listen);
            isolate_command(action)
        }
        Commands::Probe { action } => {
            drop(listen);
            probe_command(action)
        }
        Commands::Receipts { action } => {
            drop(listen);
            receipts_command(action)
        }
    }
}

fn receipts_command(action: ReceiptsAction) -> CommandFuture {
    match action {
        ReceiptsAction::Verify {
            file,
            json,
            anchors,
            config,
            token_file,
        } => {
            if !anchors && config.is_none() && token_file.is_none() {
                Box::pin(cli::receipts::verify(file, json))
            } else {
                Box::pin(cli::receipts::verify_at(
                    file, json, anchors, config, token_file,
                ))
            }
        }
        ReceiptsAction::Anchor {
            pulse,
            file,
            config,
            dry_run,
            token_file,
        } => Box::pin(cli::receipts::anchor(
            pulse, file, config, token_file, dry_run,
        )),
        ReceiptsAction::Show {
            file,
            since,
            kind,
            limit,
            json,
        } => Box::pin(cli::receipts::show(file, since, kind, limit, json)),
        ReceiptsAction::Overlaps {
            file,
            since,
            limit,
            json,
        } => Box::pin(cli::receipts::overlaps(file, since, limit, json)),
        ReceiptsAction::Swaps { file, json } => Box::pin(cli::receipts::swaps(file, json)),
        ReceiptsAction::Install {
            pulse,
            group,
            dir,
            adopt,
        } => Box::pin(cli::receipts::install(pulse, group, dir, adopt)),
    }
}

fn lifecycle_command(
    command: LifecycleCommand,
    listen: oneself::store::ListenEnv,
) -> CommandFuture {
    match command {
        LifecycleCommand::Init { dir } => {
            drop(listen);
            Box::pin(cli::init::run(dir))
        }
        LifecycleCommand::Up { headless } => Box::pin(cli::up::run(headless, listen)),
        LifecycleCommand::Chat => Box::pin(cli::chat::run(listen)),
        LifecycleCommand::Down => {
            drop(listen);
            Box::pin(cli::down::run())
        }
        LifecycleCommand::Status => {
            drop(listen);
            Box::pin(cli::status::run())
        }
    }
}

fn work_command(command: WorkCommand) -> CommandFuture {
    match command {
        WorkCommand::Schedule { action } => schedule_command(action),
        WorkCommand::Pipeline { action } => pipeline_command(action),
        WorkCommand::Archive { action } => archive_command(action),
        WorkCommand::Plugin { action } => plugin_command(action),
        WorkCommand::Intent { action } => intent_command(action),
        WorkCommand::Outreach { action } => outreach_command(action),
    }
}

fn cognition_command(command: CognitionCommand) -> CommandFuture {
    match command {
        CognitionCommand::Recall { action } => recall_command(action),
        CognitionCommand::Praxis { action } => praxis_command(action),
        CognitionCommand::Vigil { action } => vigil_command(action),
    }
}

fn schedule_command(action: ScheduleAction) -> CommandFuture {
    match action {
        ScheduleAction::List => Box::pin(cli::schedule::list()),
        ScheduleAction::Add { name, cron, prompt } => {
            Box::pin(cli::schedule::add(name, cron, prompt))
        }
        ScheduleAction::Remove { id } => Box::pin(cli::schedule::remove(id)),
        ScheduleAction::Enable { id } => Box::pin(cli::schedule::enable(id)),
        ScheduleAction::Disable { id } => Box::pin(cli::schedule::disable(id)),
        ScheduleAction::Model { id, model, clear } => Box::pin(cli::schedule::set_model(
            id,
            if clear { None } else { model },
        )),
    }
}

fn pipeline_command(action: PipelineAction) -> CommandFuture {
    match action {
        PipelineAction::Health => Box::pin(cli::pipeline::health_cmd()),
        PipelineAction::Stale => Box::pin(cli::pipeline::stale_cmd()),
    }
}

fn archive_command(action: ArchiveAction) -> CommandFuture {
    match action {
        ArchiveAction::List { document } => Box::pin(cli::archive::list(document)),
        ArchiveAction::Run { document } => Box::pin(cli::archive::run(document)),
    }
}

fn plugin_command(action: PluginAction) -> CommandFuture {
    match action {
        PluginAction::List => Box::pin(cli::plugin::list()),
        PluginAction::Add { name } => Box::pin(cli::plugin::add(name)),
        PluginAction::Remove { name } => Box::pin(cli::plugin::remove(name)),
    }
}

fn intent_command(action: IntentAction) -> CommandFuture {
    match action {
        IntentAction::List => Box::pin(cli::intent::list()),
        IntentAction::Add {
            description,
            prompt,
            priority,
        } => Box::pin(cli::intent::add(description, prompt, priority)),
        IntentAction::Remove { id } => Box::pin(cli::intent::remove(id)),
        IntentAction::Clear => Box::pin(cli::intent::clear()),
    }
}

fn outreach_command(action: OutreachAction) -> CommandFuture {
    match action {
        OutreachAction::Status => Box::pin(cli::outreach::status()),
        OutreachAction::Respond { id, rating } => Box::pin(cli::outreach::respond(id, rating)),
    }
}

fn recall_command(action: Option<RecallAction>) -> CommandFuture {
    match action {
        None => Box::pin(cli::recall::dashboard_cmd()),
        Some(RecallAction::Search { query, ranked }) => {
            Box::pin(cli::recall::search(query, ranked))
        }
        Some(RecallAction::Distill) => Box::pin(cli::recall::distill()),
    }
}

fn praxis_command(action: PraxisAction) -> CommandFuture {
    match action {
        PraxisAction::Pulse => Box::pin(cli::praxis::pulse()),
        PraxisAction::Checkpoint => Box::pin(cli::praxis::checkpoint()),
        PraxisAction::Review => Box::pin(cli::praxis::review()),
        PraxisAction::Status => Box::pin(cli::praxis::status()),
        PraxisAction::Scan { json } => Box::pin(cli::praxis::scan(json)),
        PraxisAction::Archive { dry_run } => Box::pin(cli::praxis::archive(dry_run)),
        PraxisAction::Init => Box::pin(cli::praxis::init()),
    }
}

fn vigil_command(action: VigilAction) -> CommandFuture {
    match action {
        VigilAction::Pulse => Box::pin(cli::vigil::pulse()),
        VigilAction::Collect { trigger } => Box::pin(cli::vigil::collect(trigger)),
        VigilAction::Status { json } => Box::pin(cli::vigil::status(json)),
        VigilAction::Init => Box::pin(cli::vigil::init()),
    }
}

fn isolate_command(action: IsolateAction) -> CommandFuture {
    match action {
        IsolateAction::On { reason } => Box::pin(cli::isolate::on(reason)),
        IsolateAction::Off => Box::pin(cli::isolate::off()),
        IsolateAction::Status => Box::pin(cli::isolate::status()),
    }
}

fn probe_command(action: ProbeAction) -> CommandFuture {
    match action {
        ProbeAction::Run {
            if_due,
            only,
            keep_scratch,
        } => Box::pin(cli::probe::run(if_due, only, keep_scratch)),
        ProbeAction::Status => Box::pin(cli::probe::status()),
        ProbeAction::Facts { action } => probe_facts_command(action),
        ProbeAction::ExportGraph { if_due } => Box::pin(cli::probe::export_graph(if_due)),
        ProbeAction::Sets { action } => probe_sets_command(action),
        ProbeAction::VerifyIsolation { turn, keep_scratch } => {
            Box::pin(cli::probe::verify_isolation(turn, keep_scratch))
        }
    }
}

fn probe_facts_command(action: ProbeFactsAction) -> CommandFuture {
    match action {
        ProbeFactsAction::Draw => Box::pin(cli::probe::facts_draw()),
        ProbeFactsAction::Plant { fact, value } => Box::pin(cli::probe::facts_plant(fact, value)),
        ProbeFactsAction::List => Box::pin(cli::probe::facts_list()),
    }
}

fn probe_sets_command(action: ProbeSetsAction) -> CommandFuture {
    match action {
        ProbeSetsAction::Install { from, pulse, dest } => {
            Box::pin(cli::probe::sets_install(from, pulse, dest))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tracing_subscriber::fmt::MakeWriter as _;

    fn parse(args: &[&str]) -> Commands {
        let argv = std::iter::once("pulse-null").chain(args.iter().copied());
        match Cli::try_parse_from(argv) {
            Ok(cli) => cli.command,
            Err(e) => panic!("{args:?} does not parse: {e}"),
        }
    }

    /// Every command line the binary accepts, one per dispatcher arm.
    const EVERY_COMMAND: &[&[&str]] = &[
        &["init"],
        &["init", "--dir", "somewhere"],
        &["up"],
        &["up", "--headless"],
        &["chat"],
        &["down"],
        &["status"],
        &["schedule", "list"],
        &[
            "schedule",
            "add",
            "t",
            "--cron",
            "0 0 * * * *",
            "--prompt",
            "p",
        ],
        &["schedule", "remove", "id"],
        &["schedule", "enable", "id"],
        &["schedule", "disable", "id"],
        &["schedule", "model", "id", "a-model"],
        &["schedule", "model", "id", "--clear"],
        &["pipeline", "health"],
        &["pipeline", "stale"],
        &["archive", "list"],
        &["archive", "run", "learning"],
        &["plugin", "list"],
        &["plugin", "add", "x"],
        &["plugin", "remove", "x"],
        &["intent", "list"],
        &["intent", "add", "d", "--prompt", "p"],
        &["intent", "remove", "id"],
        &["intent", "clear"],
        &["outreach", "status"],
        &["outreach", "respond", "id", "--rating", "useful"],
        &["recall"],
        &["recall", "search", "q", "--ranked"],
        &["recall", "distill"],
        &["praxis", "pulse"],
        &["praxis", "checkpoint"],
        &["praxis", "review"],
        &["praxis", "status"],
        &["praxis", "scan", "--json"],
        &["praxis", "archive", "--dry-run"],
        &["praxis", "init"],
        &["vigil", "pulse"],
        &["vigil", "collect"],
        &["vigil", "status", "--json"],
        &["vigil", "init"],
        &["repair"],
        &["isolate", "on", "--reason", "r"],
        &["isolate", "off"],
        &["isolate", "status"],
        &["probe", "run", "--if-due", "--only", "a", "b"],
        &["probe", "status"],
        &["probe", "facts", "draw"],
        &["probe", "facts", "plant", "fact-1", "value"],
        &["probe", "facts", "list"],
        &["probe", "export-graph"],
        &["probe", "export-graph", "--if-due"],
        &[
            "probe", "sets", "install", "--from", "dir", "--pulse", "echo",
        ],
        &["probe", "verify-isolation", "--turn", "--keep-scratch"],
    ];

    #[test]
    fn every_command_parses_and_routes_without_running() {
        for args in EVERY_COMMAND {
            // Building the future runs nothing; dropping it unpolled
            // leaves no trace.
            drop(dispatch(parse(args), oneself::store::ListenEnv::empty()));
        }
    }

    #[test]
    fn the_flattened_groups_keep_the_flat_command_list() {
        use clap::CommandFactory as _;
        let names: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "init", "up", "chat", "down", "status", "schedule", "pipeline", "archive",
                "plugin", "intent", "outreach", "recall", "praxis", "vigil", "repair", "isolate",
                "probe", "receipts",
            ]
        );
    }

    #[test]
    fn only_the_tui_commands_log_to_the_tui_log() {
        assert!(writes_tui_log(&parse(&["up"])));
        assert!(writes_tui_log(&parse(&["chat"])));
        assert!(!writes_tui_log(&parse(&["up", "--headless"])));
        assert!(!writes_tui_log(&parse(&["status"])));
        assert!(!writes_tui_log(&parse(&["probe", "status"])));
    }

    #[test]
    fn root_checks_match_the_commands_that_must_not_run_as_root() {
        let cases: &[(&[&str], Option<RootCheck>)] = &[
            (&["init"], Some(RootCheck::prefixed("init"))),
            (&["up", "--headless"], Some(RootCheck::prefixed("up"))),
            (&["chat"], Some(RootCheck::bare("chat"))),
            (&["repair"], Some(RootCheck::prefixed("repair"))),
            (&["probe", "status"], Some(RootCheck::bare("probe"))),
            (&["probe", "run"], Some(RootCheck::bare("probe"))),
            (
                &["probe", "sets", "install", "--from", "d", "--pulse", "echo"],
                None,
            ),
            (&["receipts", "verify", "--file", "x"], None),
            (&["down"], None),
            (&["status"], None),
            (&["schedule", "list"], None),
            (&["isolate", "status"], None),
        ];
        for (args, expected) in cases {
            assert_eq!(root_check(&parse(args)), *expected, "{args:?}");
        }
    }

    #[test]
    fn a_refusal_is_printed_with_or_without_the_error_prefix() {
        let refuse = |name: &str| Err(format!("refused {name}"));
        assert_eq!(
            RootCheck::prefixed("init").refusal(refuse),
            Some("Error: refused init".to_string())
        );
        assert_eq!(
            RootCheck::bare("chat").refusal(refuse),
            Some("refused chat".to_string())
        );
        assert_eq!(RootCheck::bare("probe").refusal(|_| Ok(())), None);
    }

    #[tokio::test]
    async fn a_refused_command_stops_before_it_runs() {
        let refused = run_command(
            parse(&["chat"]),
            |name| Err(format!("no {name}")),
            oneself::store::ListenEnv::empty(),
        )
        .await;
        assert_eq!(refused, Err("no chat".to_string()));
        let refused = run_command(
            parse(&["init"]),
            |name| Err(format!("no {name}")),
            oneself::store::ListenEnv::empty(),
        )
        .await;
        assert_eq!(refused, Err("Error: no init".to_string()));
    }

    #[test]
    fn the_tui_log_receives_what_the_writer_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui.log");
        let file = std::fs::File::create(&path).unwrap();
        let writer = tui_log_writer(Ok(file));
        writer.make_writer().write_all(b"a log line\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a log line\n");
    }

    #[test]
    fn an_unopenable_tui_log_discards_logs() {
        let writer = tui_log_writer(Err("no HOME to log under".to_string()));
        writer.make_writer().write_all(b"dropped\n").unwrap();
    }
}
