pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Phase 1: simple — just tell the user to Ctrl+C
    // Future: PID file, graceful shutdown signal
    eprintln!("Use Ctrl+C to stop a running entity, or kill the process.");
    Ok(())
}
