use crate::config::Config;

/// `pulse-null chat`: the TUI, attached to the running daemon (or one started
/// in-process). Opens straight into Talk once attached.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    crate::tui::run(config).await
}
