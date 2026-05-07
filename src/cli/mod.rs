pub mod archive;
pub mod chat;
pub mod down;
pub mod init;
pub mod intent;
pub mod pipeline;
pub mod plugin;
pub mod praxis;
pub mod recall;
pub mod repair;
pub mod schedule;
pub mod status;
pub mod up;
pub mod vigil;

use std::io;
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::EnvFilter;

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| "pulse_null=info".into())
}

/// Standard stdout tracing for non-interactive CLI commands.
pub fn init_stdout_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .init();
}

/// File-backed tracing for TUI commands. Logs land in `<log_dir>/logs/pulse-null.log`
/// so they don't bleed onto the alternate screen and corrupt ratatui's render.
/// Falls back to a sink (discards output) if the file can't be opened.
pub fn init_file_tracing(log_dir: &Path) {
    let logs_dir = log_dir.join("logs");
    let _ = std::fs::create_dir_all(&logs_dir);
    let log_path = logs_dir.join("pulse-null.log");

    let writer: BoxMakeWriter = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => BoxMakeWriter::new(Mutex::new(file)),
        Err(_) => BoxMakeWriter::new(io::sink),
    };

    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_ansi(false)
        .with_writer(writer)
        .init();
}
