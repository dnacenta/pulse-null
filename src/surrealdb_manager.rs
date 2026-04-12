//! SurrealDB server process manager.
//!
//! Manages the SurrealDB server lifecycle: startup, health check, entity
//! provisioning, and graceful shutdown. The first entity to boot spawns the
//! server; the last to shut down stops it.

use std::path::{Path, PathBuf};

use tokio::io::AsyncBufReadExt;
use tracing::{error, info, warn};

const SURREAL_PORT: u16 = 8787;
const SURREAL_BIND: &str = "127.0.0.1";
const NAMESPACE: &str = "nullarc";
const PID_FILE: &str = "surrealdb/surreal.pid";
const ROOT_PASSWORD_FILE: &str = "surrealdb/root-password";
const PASSWORD_LEN: usize = 32;
const STARTUP_TIMEOUT_SECS: u64 = 15;

/// Ensure the SurrealDB server is running. If not reachable, spawn it.
///
/// `data_dir` is the shared directory (e.g., `/opt/pulse-null/`).
pub async fn ensure_running(
    data_dir: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{SURREAL_BIND}:{SURREAL_PORT}");

    // Check if already reachable
    if is_reachable(&url).await {
        info!("SurrealDB server already running on {url}");
        return Ok(());
    }

    // Find the surreal binary
    let surreal_bin = find_binary(data_dir)?;

    // Ensure data directories exist
    let db_data_dir = data_dir.join("surrealdb/data");
    std::fs::create_dir_all(&db_data_dir)?;
    std::fs::create_dir_all(data_dir.join("surrealdb"))?;

    // Generate root password if first boot
    let root_password = ensure_root_password(data_dir)?;

    // Spawn the server process (detached via setsid)
    let data_path = format!("surrealkv:{}", db_data_dir.display());
    info!("Starting SurrealDB server on {url}");

    let mut cmd = tokio::process::Command::new(&surreal_bin);
    cmd.arg("start")
        .arg("--bind")
        .arg(&url)
        .arg("--user")
        .arg("root")
        .arg("--pass")
        .arg(&root_password)
        .arg(&data_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Detach the process so it survives if this entity shuts down
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| {
        format!(
            "failed to spawn SurrealDB: {e}. Binary: {}",
            surreal_bin.display()
        )
    })?;

    let pid = child.id().ok_or("failed to get SurrealDB PID")?;

    // Write PID file
    let pid_path = data_dir.join(PID_FILE);
    std::fs::write(&pid_path, pid.to_string())?;
    info!("SurrealDB spawned with PID {pid}");

    // Log stderr in background
    if let Some(stderr) = child.stderr.take() {
        let reader = tokio::io::BufReader::new(stderr);
        tokio::spawn(async move {
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains("ERR") || line.contains("error") {
                    error!("[surrealdb] {line}");
                } else {
                    info!("[surrealdb] {line}");
                }
            }
        });
    }

    // Drop child handle without killing (process is detached via setsid)
    drop(child);

    // Wait for the server to become reachable
    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(STARTUP_TIMEOUT_SECS);

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(format!(
                "SurrealDB did not become reachable within {STARTUP_TIMEOUT_SECS}s on {url}"
            )
            .into());
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if is_reachable(&url).await {
            info!("SurrealDB server ready on {url}");
            return Ok(());
        }
    }
}

/// Provision a database and user for an entity.
///
/// Connects as root, creates namespace/database/user if not exists,
/// writes the entity password to `{entity_dir}/secrets/graph-password`,
/// and updates the entity's `.recall-echo.toml` with [graph] settings.
pub async fn provision_entity(
    data_dir: &Path,
    entity_name: &str,
    entity_dir: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let secrets_dir = entity_dir.join("secrets");
    let password_path = secrets_dir.join("graph-password");

    // If password file exists, entity is already provisioned
    if password_path.exists() {
        info!("Entity '{entity_name}' already provisioned (password file exists)");
        return Ok(());
    }

    info!("Provisioning SurrealDB database for entity '{entity_name}'");

    let root_password = read_root_password(data_dir)?;
    let url = format!("{SURREAL_BIND}:{SURREAL_PORT}");

    // Connect as root
    let db = surrealdb::Surreal::new::<surrealdb::engine::remote::ws::Ws>(&url).await?;
    db.signin(surrealdb::opt::auth::Root {
        username: "root".to_string(),
        password: root_password,
    })
    .await?;

    // Generate entity password
    let entity_password = generate_password();

    // Create namespace, database, and user
    let provision_sql = format!(
        r#"
        DEFINE NAMESPACE IF NOT EXISTS {NAMESPACE};
        USE NS {NAMESPACE};
        DEFINE DATABASE IF NOT EXISTS {entity_name};
        USE DB {entity_name};
        DEFINE USER IF NOT EXISTS {entity_name} ON DATABASE
            PASSWORD '{entity_password}' ROLES OWNER;
        "#
    );

    db.query(&provision_sql).await?.check()?;

    // Write password file
    std::fs::create_dir_all(&secrets_dir)?;
    std::fs::write(&password_path, &entity_password)?;

    // Set file permissions to 0600 (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&password_path, std::fs::Permissions::from_mode(0o600))?;
    }

    // Update entity's .recall-echo.toml with [graph] section
    let memory_dir = entity_dir.join("memory");
    let config_path = memory_dir.join(".recall-echo.toml");
    update_recall_echo_config(&config_path, entity_name, "secrets/graph-password")?;

    info!("Entity '{entity_name}' provisioned successfully");
    Ok(())
}

/// Shut down the SurrealDB server if this process owns it.
pub async fn shutdown_if_owner(
    data_dir: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pid_path = data_dir.join(PID_FILE);

    let pid = match std::fs::read_to_string(&pid_path) {
        Ok(s) => match s.trim().parse::<u32>() {
            Ok(p) => p,
            Err(_) => return Ok(()),
        },
        Err(_) => return Ok(()),
    };

    // Check if the process is still alive
    if !crate::pidfile::is_alive(pid) {
        warn!("SurrealDB PID {pid} is not running, cleaning up PID file");
        let _ = std::fs::remove_file(&pid_path);
        return Ok(());
    }

    // Send SIGTERM
    info!("Sending SIGTERM to SurrealDB server (PID {pid})");
    crate::pidfile::kill(pid);

    // Wait for clean exit
    for _ in 0..20 {
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        if !crate::pidfile::is_alive(pid) {
            info!("SurrealDB server stopped cleanly");
            let _ = std::fs::remove_file(&pid_path);
            return Ok(());
        }
    }

    warn!("SurrealDB server did not stop within 5s");
    let _ = std::fs::remove_file(&pid_path);
    Ok(())
}

// ── Internal helpers ────────────────────────────────────────────────────

async fn is_reachable(addr: &str) -> bool {
    tokio::net::TcpStream::connect(addr).await.is_ok()
}

fn find_binary(data_dir: &Path) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let local = data_dir.join("bin/surreal");
    if local.exists() {
        return Ok(local);
    }

    if let Ok(path) = which::which("surreal") {
        return Ok(path);
    }

    let common = PathBuf::from("/usr/local/bin/surreal");
    if common.exists() {
        return Ok(common);
    }

    Err("SurrealDB binary not found. Install it or place it at {data_dir}/bin/surreal".into())
}

fn ensure_root_password(
    data_dir: &Path,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let pw_path = data_dir.join(ROOT_PASSWORD_FILE);

    if pw_path.exists() {
        return Ok(std::fs::read_to_string(&pw_path)?.trim().to_string());
    }

    let password = generate_password();
    std::fs::write(&pw_path, &password)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pw_path, std::fs::Permissions::from_mode(0o600))?;
    }

    info!("Generated SurrealDB root password at {}", pw_path.display());
    Ok(password)
}

fn read_root_password(data_dir: &Path) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let pw_path = data_dir.join(ROOT_PASSWORD_FILE);
    let password = std::fs::read_to_string(&pw_path).map_err(|e| {
        format!(
            "failed to read SurrealDB root password at {}: {e}",
            pw_path.display()
        )
    })?;
    Ok(password.trim().to_string())
}

fn generate_password() -> String {
    use std::io::Read;
    let mut bytes = [0u8; PASSWORD_LEN];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("failed to read /dev/urandom");

    bytes
        .iter()
        .map(|b| {
            let charset = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
            charset[(*b as usize) % charset.len()] as char
        })
        .collect()
}

fn update_recall_echo_config(
    config_path: &Path,
    entity_name: &str,
    password_file: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut content = if config_path.exists() {
        std::fs::read_to_string(config_path)?
    } else {
        String::new()
    };

    // Remove existing [graph] section if present
    if let Some(start) = content.find("[graph]") {
        let end = content[start + 7..]
            .find("\n[")
            .map(|i| start + 7 + i)
            .unwrap_or(content.len());
        content.replace_range(start..end, "");
    }

    // Append new [graph] section
    let graph_section = format!(
        r#"
[graph]
mode = "server"
url = "{SURREAL_BIND}:{SURREAL_PORT}"
namespace = "{NAMESPACE}"
database = "{entity_name}"
username = "{entity_name}"
password_file = "{password_file}"
"#
    );

    content.push_str(&graph_section);
    std::fs::write(config_path, content.trim_start())?;

    Ok(())
}
