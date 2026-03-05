use crate::config::Config;
use crate::pidfile;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    let pid = match pidfile::read(&root_dir) {
        Some(pid) => pid,
        None => {
            eprintln!("No running entity found (no PID file).");
            return Ok(());
        }
    };

    if !pidfile::is_alive(pid) {
        eprintln!("Entity is not running (stale PID file, pid {}).", pid);
        pidfile::remove(&root_dir);
        return Ok(());
    }

    println!("Stopping entity (pid {})...", pid);

    if !pidfile::kill(pid) {
        return Err(format!("Failed to send SIGTERM to pid {}", pid).into());
    }

    // Wait up to 10 seconds for the process to exit
    for _ in 0..100 {
        if !pidfile::is_alive(pid) {
            pidfile::remove(&root_dir);
            println!("Entity stopped.");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    eprintln!("Entity did not stop within 10 seconds (pid {}).", pid);
    Ok(())
}
