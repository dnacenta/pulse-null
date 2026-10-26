//! SurrealDB server process manager.
//!
//! `spawn` mode starts a server when its endpoint is down and stops only the
//! process this daemon itself spawned. `system` mode never reaches that spawn
//! or this shutdown: the daemon attaches to `surrealdb.service`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::pidfile::ProcView;

use tokio::io::AsyncBufReadExt;
use tracing::{error, info, warn};

pub(crate) const SURREAL_PORT: u16 = 8787;
const SURREAL_BIND: &str = "127.0.0.1";
pub(crate) const NAMESPACE: &str = "nullarc";
const PID_FILE: &str = "surrealdb/surreal.pid";
const ROOT_PASSWORD_FILE: &str = "surrealdb/root-password";
const PASSWORD_LEN: usize = 32;
const STARTUP_TIMEOUT_SECS: u64 = 15;

/// Ensure the SurrealDB server is running. If not reachable, spawn it.
///
/// `data_dir` is the shared directory (e.g., `/opt/pulse-null/`).
/// `surreal_bin` is an absolute path, or empty to search `PATH` and then
/// `/usr/local/bin`. The root password is the child's `SURREAL_PASS` and is
/// not an argument.
pub async fn ensure_running(
    data_dir: &Path,
    surreal_bin: &str,
    addr: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if reachable(addr).await {
        info!("SurrealDB server already running on {addr}");
        return Ok(());
    }

    let surreal_bin = find_binary(surreal_bin)?;

    let db_data_dir = data_dir.join("surrealdb/data");
    std::fs::create_dir_all(&db_data_dir)?;
    std::fs::create_dir_all(data_dir.join("surrealdb"))?;

    let root_password = ensure_root_password(data_dir)?;
    let data_path = format!("surrealkv:{}", db_data_dir.display());
    info!("Starting SurrealDB server on {addr}");

    let mut cmd = surreal_command(&surreal_bin, addr, &data_path, &root_password);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Detach the process so it survives if this pulse shuts down.
    // SAFETY: `setsid` runs in the forked child, before exec, so the server
    // is not in this process's session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = crate::process_state::during_fork(|| cmd.spawn()).map_err(|e| {
        format!(
            "failed to spawn SurrealDB: {e}. Binary: {}",
            surreal_bin.display()
        )
    })?;

    let pid = child.id().ok_or("failed to get SurrealDB PID")?;
    if let Err(err) = remember_pid(pid) {
        child.start_kill().ok();
        let _ = child.wait().await;
        return Err(err);
    }

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
                "SurrealDB did not become reachable within {STARTUP_TIMEOUT_SECS}s on {addr}"
            )
            .into());
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if reachable(addr).await {
            info!("SurrealDB server ready on {addr}");
            return Ok(());
        }
    }
}

/// Provision a database and user for a pulse.
///
/// Connects as root, creates namespace/database/user if not exists,
/// writes the pulse password to `{pulse_dir}/secrets/graph-password`,
/// and updates the pulse's `.recall-echo.toml` with [graph] settings.
pub async fn provision_pulse(
    data_dir: &Path,
    pulse_name: &str,
    pulse_dir: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let secrets_dir = pulse_dir.join("secrets");
    let password_path = secrets_dir.join("graph-password");

    // If password file exists, the pulse is already provisioned
    if password_path.exists() {
        info!("Pulse '{pulse_name}' already provisioned (password file exists)");
        return Ok(());
    }

    info!("Provisioning SurrealDB database for pulse '{pulse_name}'");

    let root_password = read_root_password(data_dir)?;
    let url = format!("{SURREAL_BIND}:{SURREAL_PORT}");

    // Connect as root
    let db = surrealdb::Surreal::new::<surrealdb::engine::remote::ws::Ws>(&url).await?;
    db.signin(surrealdb::opt::auth::Root {
        username: "root".to_string(),
        password: root_password,
    })
    .await?;

    // Generate pulse password
    let pulse_password = generate_password();

    // Create namespace, database, and user
    let provision_sql = format!(
        r#"
        DEFINE NAMESPACE IF NOT EXISTS {NAMESPACE};
        USE NS {NAMESPACE};
        DEFINE DATABASE IF NOT EXISTS {pulse_name};
        USE DB {pulse_name};
        DEFINE USER IF NOT EXISTS {pulse_name} ON DATABASE
            PASSWORD '{pulse_password}' ROLES OWNER;
        "#
    );

    db.query(&provision_sql).await?.check()?;

    // Write password file
    std::fs::create_dir_all(&secrets_dir)?;
    std::fs::write(&password_path, &pulse_password)?;

    // Set file permissions to 0600 (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&password_path, std::fs::Permissions::from_mode(0o600))?;
    }

    // Update pulse's .recall-echo.toml with [graph] section
    let memory_dir = pulse_dir.join("memory");
    let config_path = memory_dir.join(".recall-echo.toml");
    update_recall_echo_config(&config_path, pulse_name, "secrets/graph-password")?;

    info!("Pulse '{pulse_name}' provisioned successfully");
    Ok(())
}

/// Stop the SurrealDB process this daemon spawned, when it is still that
/// process. A pid read from `surreal.pid` is never signalled. Nothing is
/// signalled when this process did not spawn a server.
pub async fn shutdown_if_owner(
    data_dir: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(owned) = take_owned() else {
        return Ok(());
    };
    let live = crate::pidfile::LiveProc;
    match live.start_time(owned.pid) {
        Ok(start) if start == owned.start => {
            info!("Sending SIGTERM to SurrealDB server (PID {})", owned.pid);
            if let Err(err) = crate::pidfile::terminate(&owned.fd) {
                warn!("SurrealDB shutdown signal: {err}");
                return Ok(());
            }
            let pid = owned.pid;
            let fd = owned.fd;
            match tokio::task::spawn_blocking(move || {
                crate::pidfile::wait_exit(&fd, std::time::Duration::from_secs(5))
            })
            .await
            {
                Ok(Ok(true)) => info!("SurrealDB server stopped cleanly"),
                Ok(Ok(false)) => warn!("SurrealDB server did not stop within 5s"),
                Ok(Err(err)) => warn!("SurrealDB shutdown wait: {err}"),
                Err(err) => warn!("SurrealDB shutdown wait: {err}"),
            }
            remove_pidfile_if_ours(data_dir, pid);
        }
        Ok(_) => {
            warn!(
                "SurrealDB PID {} start time changed; not signalling",
                owned.pid
            );
        }
        Err(err) => {
            warn!("SurrealDB PID {} is not running ({err})", owned.pid);
            remove_pidfile_if_ours(data_dir, owned.pid);
        }
    }
    Ok(())
}

// ── Internal helpers ────────────────────────────────────────────────────

/// The SurrealDB process this daemon spawned. The pid file is not consulted
/// when deciding what to signal.
struct OwnedServer {
    pid: u32,
    start: u64,
    fd: crate::pidfile::Pidfd,
}

static OWNED: Mutex<Option<OwnedServer>> = Mutex::new(None);

fn lock_owned() -> std::sync::MutexGuard<'static, Option<OwnedServer>> {
    OWNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn remember(server: OwnedServer) {
    let mut slot = lock_owned();
    if slot.is_some() {
        warn!("replacing a SurrealDB server this process already spawned");
    }
    *slot = Some(server);
}

fn take_owned() -> Option<OwnedServer> {
    lock_owned().take()
}

fn remember_pid(pid: u32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let live = crate::pidfile::LiveProc;
    let start = live.start_time(pid)?;
    let fd = live.open_pidfd(pid)?;
    remember(OwnedServer { pid, start, fd });
    Ok(())
}

fn remove_pidfile_if_ours(data_dir: &Path, pid: u32) {
    let pid_path = data_dir.join(PID_FILE);
    if std::fs::read_to_string(&pid_path).is_ok_and(|text| text.trim() == pid.to_string()) {
        let _ = std::fs::remove_file(&pid_path);
    }
}

/// `127.0.0.1:8787`, the endpoint production uses.
pub(crate) fn default_addr() -> String {
    format!("{SURREAL_BIND}:{SURREAL_PORT}")
}

pub(crate) async fn reachable(addr: &str) -> bool {
    tokio::net::TcpStream::connect(addr).await.is_ok()
}

/// `surreal start` with the root password in the child environment only.
pub(crate) fn surreal_command(
    bin: &Path,
    addr: &str,
    data_path: &str,
    password: &str,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("start")
        .arg("--bind")
        .arg(addr)
        .arg("--user")
        .arg("root")
        .arg(data_path)
        .env("SURREAL_PASS", password);
    cmd
}

/// The `surreal` binary: `surreal_bin` when set, otherwise `PATH`, then
/// `/usr/local/bin/surreal`. A binary under the data directory is never used.
pub(crate) fn find_binary(
    surreal_bin: &str,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    if !surreal_bin.is_empty() {
        let path = PathBuf::from(surreal_bin);
        if !path.is_absolute() {
            return Err("[graph] surreal_bin must be an absolute path".into());
        }
        if !path.is_file() {
            return Err(format!("SurrealDB binary not found at {}", path.display()).into());
        }
        return Ok(path);
    }

    if let Ok(path) = which::which("surreal") {
        return Ok(path);
    }

    let common = PathBuf::from("/usr/local/bin/surreal");
    if common.is_file() {
        return Ok(common);
    }

    Err(
        "SurrealDB binary not found. Install it or set [graph] surreal_bin to an absolute path"
            .into(),
    )
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

pub(crate) fn read_root_password(
    data_dir: &Path,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let pw_path = data_dir.join(ROOT_PASSWORD_FILE);
    let password = std::fs::read_to_string(&pw_path).map_err(|e| {
        format!(
            "failed to read SurrealDB root password at {}: {e}",
            pw_path.display()
        )
    })?;
    Ok(password.trim().to_string())
}

const PASSWORD_CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// One symbol from [`PASSWORD_CHARSET`], or `None` when `byte` sits in the
/// leftover residue. `256` is not a multiple of 62, so taking `byte % 62`
/// would make the first eight symbols more likely. Bytes at or above the
/// largest multiple of 62 that fits in a byte are discarded instead.
fn password_symbol(byte: u8) -> Option<u8> {
    let span = 256 - (256 % PASSWORD_CHARSET.len());
    if (byte as usize) >= span {
        None
    } else {
        Some(PASSWORD_CHARSET[(byte as usize) % PASSWORD_CHARSET.len()])
    }
}

pub(crate) fn generate_password() -> String {
    use std::io::Read;
    let mut out = String::with_capacity(PASSWORD_LEN);
    let mut file = std::fs::File::open("/dev/urandom").expect("failed to read /dev/urandom");
    let mut buf = [0u8; 64];
    while out.len() < PASSWORD_LEN {
        file.read_exact(&mut buf)
            .expect("failed to read /dev/urandom");
        for &byte in &buf {
            if let Some(symbol) = password_symbol(byte) {
                out.push(symbol as char);
                if out.len() == PASSWORD_LEN {
                    break;
                }
            }
        }
    }
    out
}

fn update_recall_echo_config(
    config_path: &Path,
    pulse_name: &str,
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
database = "{pulse_name}"
username = "{pulse_name}"
password_file = "{password_file}"
"#
    );

    content.push_str(&graph_section);
    std::fs::write(config_path, content.trim_start())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `<data_dir>/bin/surreal` is never the binary spawn mode runs.
    #[test]
    fn spawn_ignores_data_dir_binary() {
        let data_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data_dir.path().join("bin")).unwrap();
        let planted = data_dir.path().join("bin/surreal");
        std::fs::write(&planted, "").unwrap();
        match find_binary("") {
            Ok(found) => assert_ne!(found, planted),
            Err(err) => {
                assert!(
                    err.to_string().starts_with("SurrealDB binary not found"),
                    "{err}"
                );
            }
        }

        let chosen = data_dir.path().join("chosen-surreal");
        std::fs::write(&chosen, "").unwrap();
        let found = find_binary(chosen.to_str().unwrap()).unwrap();
        assert_eq!(found, chosen);
        assert_ne!(found, planted);
    }

    /// The spawned command has no `--pass` and takes the password from its
    /// own environment. The parent process does not gain that variable.
    #[test]
    fn spawn_keeps_root_password_off_argv() {
        let password = "root-secret-not-for-argv";
        let cmd = surreal_command(
            Path::new("/usr/bin/surreal"),
            "127.0.0.1:9",
            "surrealkv:/tmp/db",
            password,
        );
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!args
            .iter()
            .any(|arg| arg == "--pass" || arg.contains(password)));
        assert!(cmd
            .as_std()
            .get_envs()
            .any(|(key, value)| { key == "SURREAL_PASS" && value.is_some_and(|v| v == password) }));
        assert_ne!(
            std::env::var("SURREAL_PASS").ok().as_deref(),
            Some(password)
        );
    }

    /// A pid written into `surreal.pid` is left alone. Only the child this
    /// process recorded is signalled, and only when one was recorded.
    #[test]
    fn shutdown_signals_only_its_own_child() {
        if !crate::process_state::isolated_test("shutdown_signals_only_its_own_child") {
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let data = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(data.path().join("surrealdb")).unwrap();
            let mut ours = Reap::spawn();
            let mut other = Reap::spawn();
            let our_pid = ours.id();
            let other_pid = other.id();
            remember_pid(our_pid).unwrap();
            write_pid(data.path(), other_pid);

            shutdown_if_owner(data.path()).await.unwrap();
            assert!(ours.exited(), "the spawned child was not signalled");
            assert!(!other.exited(), "the pid in surreal.pid was signalled");

            let mut lone = Reap::spawn();
            let lone_pid = lone.id();
            write_pid(data.path(), lone_pid);
            shutdown_if_owner(data.path()).await.unwrap();
            assert!(
                !lone.exited(),
                "a pid file with nothing spawned was signalled"
            );
        });
    }

    fn write_pid(data_dir: &Path, pid: u32) {
        std::fs::write(data_dir.join(PID_FILE), pid.to_string()).unwrap();
    }

    /// A `/bin/sleep` this test spawned. Drop kills it only while it is still
    /// the unreaped child, so a pid reused after reap is not signalled.
    struct Reap {
        child: std::process::Child,
        reaped: bool,
    }

    impl Reap {
        fn spawn() -> Self {
            let child = crate::process_state::during_fork(|| {
                std::process::Command::new("/bin/sleep")
                    .arg("120")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("sleep")
            });
            Self {
                child,
                reaped: false,
            }
        }

        fn id(&self) -> u32 {
            self.child.id()
        }

        fn exited(&mut self) -> bool {
            if self.reaped {
                return true;
            }
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.reaped = true;
                    true
                }
                Ok(None) => false,
                Err(_) => false,
            }
        }
    }

    impl Drop for Reap {
        fn drop(&mut self) {
            if self.reaped {
                return;
            }
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Each alphabet symbol is hit the same number of times, and the
    /// leftover bytes (248..=255) contribute nothing.
    #[test]
    fn password_bytes_are_sampled_without_a_residue_bias() {
        let mut counts = [0usize; PASSWORD_CHARSET.len()];
        for byte in 0u16..256 {
            match password_symbol(byte as u8) {
                Some(symbol) => {
                    let index = PASSWORD_CHARSET.iter().position(|c| *c == symbol).unwrap();
                    counts[index] += 1;
                }
                None => assert!(byte >= 248, "{byte}"),
            }
        }
        assert!(counts.iter().all(|n| *n == 4), "{counts:?}");
        let password = generate_password();
        assert_eq!(password.len(), PASSWORD_LEN);
        assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    /// With no configured path: the one on `PATH`, else `/usr/local/bin/surreal`,
    /// else an error that says to set `[graph] surreal_bin`.
    #[test]
    fn without_its_own_binary_the_installed_one_is_found_or_refused() {
        let data_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data_dir.path().join("bin")).unwrap();
        std::fs::write(data_dir.path().join("bin/surreal"), "").unwrap();
        match find_binary("") {
            Ok(found) => {
                assert!(found.is_file(), "{}", found.display());
                assert_ne!(found, data_dir.path().join("bin/surreal"));
                assert_eq!(found.file_name().unwrap(), "surreal");
            }
            Err(err) => assert!(
                err.to_string().starts_with("SurrealDB binary not found"),
                "{err}"
            ),
        }
    }
}
