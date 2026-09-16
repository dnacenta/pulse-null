use crate::config::Config;

/// `pulse-null chat`: the TUI, attached to the running daemon (or one started
/// in-process). With a daemon already up it opens straight into Talk.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    crate::tui::run_chat(config).await
}
