use crate::config::Config;

/// `pulse-null chat`: the TUI, attached to the running daemon (or one started
/// in-process). Inside an entity directory it opens straight into Talk;
/// anywhere else it opens Home, like `up`.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Config::load() {
        Ok(config) => crate::tui::run_chat(config).await,
        Err(_) => crate::tui::run_home().await,
    }
}
