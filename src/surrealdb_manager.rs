//! SurrealDB server process manager.
//!
//! `spawn` mode starts a server when its endpoint is down and stops only the
//! process this daemon itself spawned. `system` mode never reaches that spawn
//! or this shutdown: the daemon attaches to `surrealdb.service`.

use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
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
/// A password file larger than this is refused as garbage.
const READ_CAP: u64 = 8192;
/// `.recall-echo.toml` may hold a user-edited config. Password files stay at [`READ_CAP`].
pub(crate) const TOML_CAP: u64 = 1024 * 1024;
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

/// What [`provision_pulse`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// `secrets/graph-password` was written.
    Created,
    /// A usable `secrets/graph-password` was already there and was kept.
    Already,
}

/// Whether `secrets/graph-password` can be used as the pulse password.
#[derive(Debug)]
pub(crate) enum GraphPassword {
    /// Regular, owned by the caller, private, non-empty UTF-8, no NUL.
    Ready,
    /// The path does not exist.
    Missing,
    /// The path exists and cannot be used. The string says why.
    Fault(String),
}

const PASSWORD_NAME: &str = "graph-password";
const PASSWORD_NEW: &str = "graph-password.new";
const RECALL_NAME: &str = ".recall-echo.toml";
const RECALL_TEMP: &str = ".recall-echo.toml.tmp";

/// `host:port` on `127.0.0.1`, `localhost`, or `[::1]`, port 1-65535.
///
/// A scheme, userinfo, path, query, whitespace, or any other host is refused.
/// The error names the rule and shows only a short escaped prefix of `raw`.
pub(crate) fn require_loopback(raw: &str) -> Result<(), String> {
    const RULE: &str = "graph endpoint must be host:port on 127.0.0.1, localhost, or [::1], \
         with port 1-65535 and no scheme, userinfo, path, or query";
    let refuse = || format!("{RULE} (got {})", brief_endpoint(raw));
    if raw.is_empty()
        || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        || raw.contains("://")
        || raw.contains('/')
        || raw.contains('?')
        || raw.contains('#')
        || raw.contains('@')
    {
        return Err(refuse());
    }
    let Some((host, port)) = split_host_port(raw) else {
        return Err(refuse());
    };
    let local = host == "127.0.0.1" || host == "localhost" || host == "::1";
    let Ok(port_n) = port.parse::<u32>() else {
        return Err(refuse());
    };
    if !local || !(1..=65535).contains(&port_n) {
        return Err(refuse());
    }
    Ok(())
}

/// The address a connector must dial. `localhost` and `127.0.0.1` become
/// `127.0.0.1:<port>`, and `[::1]` stays `[::1]:<port>`, so the system
/// resolver cannot send the root password anywhere else.
pub(crate) fn dial_addr(raw: &str) -> Result<String, String> {
    require_loopback(raw)?;
    let (host, port) = split_host_port(raw).expect("loopback endpoint has a host and port");
    if host == "::1" {
        Ok(format!("[::1]:{port}"))
    } else {
        Ok(format!("127.0.0.1:{port}"))
    }
}

fn split_host_port(raw: &str) -> Option<(&str, &str)> {
    if let Some(rest) = raw.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        if host.is_empty() || port.is_empty() || port.contains(':') {
            return None;
        }
        return Some((host, port));
    }
    let (host, port) = raw.split_once(':')?;
    if host.is_empty() || port.is_empty() || host.contains(':') || port.contains(':') {
        return None;
    }
    Some((host, port))
}

fn brief_endpoint(raw: &str) -> String {
    let mut out = String::new();
    for (index, ch) in raw.chars().enumerate() {
        if index == 24 {
            out.push_str("...");
            break;
        }
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\\' | '"' => {
                out.push('\\');
                out.push(ch);
            }
            other if other.is_control() => {
                out.push_str(&format!("\\u{{{:04x}}}", other as u32));
            }
            other => out.push(other),
        }
    }
    out
}

/// Create the pulse's graph user, or keep a usable password file that is already there.
///
/// `root_password` is the SurrealDB root credential. It is sent only to a
/// loopback graph endpoint and is not written, logged, or returned. The
/// generated pulse password is written only to `secrets/graph-password`.
/// `rotate` replaces that file and the database user's password: the new
/// password is stored in `secrets/graph-password.new` before the DEFINE, and
/// renamed into place only after the DEFINE succeeds.
pub async fn provision_pulse(
    endpoint: &str,
    root_password: &str,
    pulse_name: &str,
    pulse_dir: &Path,
    rotate: bool,
) -> Result<ProvisionOutcome, Box<dyn std::error::Error + Send + Sync>> {
    provision_inner(
        endpoint,
        root_password,
        pulse_name,
        pulse_dir,
        rotate,
        false,
    )
    .await
}

/// Like [`provision_pulse`] with `rotate`, stopping after a successful DEFINE
/// and before the side file is renamed. The side file is the password the
/// database now expects.
#[cfg(test)]
pub async fn provision_pulse_crash_after_define(
    endpoint: &str,
    root_password: &str,
    pulse_name: &str,
    pulse_dir: &Path,
) -> Result<ProvisionOutcome, Box<dyn std::error::Error + Send + Sync>> {
    provision_inner(endpoint, root_password, pulse_name, pulse_dir, true, true).await
}

fn provision_sql(pulse_name: &str, pulse_password: &str, rotate: bool) -> String {
    // A plain DEFINE USER fails when the user already exists. OVERWRITE is the
    // rotate form that replaces that password. The first provision uses the
    // plain form so a lost file cannot be reported as a new password.
    let user = if rotate {
        format!(
            "DEFINE USER OVERWRITE {pulse_name} ON DATABASE PASSWORD '{pulse_password}' ROLES OWNER;"
        )
    } else {
        format!("DEFINE USER {pulse_name} ON DATABASE PASSWORD '{pulse_password}' ROLES OWNER;")
    };
    format!(
        "DEFINE NAMESPACE IF NOT EXISTS {NAMESPACE};\n\
         USE NS {NAMESPACE};\n\
         DEFINE DATABASE IF NOT EXISTS {pulse_name};\n\
         USE DB {pulse_name};\n\
         {user}\n"
    )
}

fn redact(text: &str, root_password: &str, pulse_password: &str) -> String {
    let mut secrets = [root_password, pulse_password];
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    let mut out = text.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            out = out.replace(secret, "[redacted]");
        }
    }
    out
}

async fn apply_statements(
    endpoint: &str,
    root_password: &str,
    sql: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let endpoint = dial_addr(endpoint)?;
    #[cfg(test)]
    if try_mock(&endpoint, sql).await? {
        return Ok(());
    }
    let db = surrealdb::Surreal::new::<surrealdb::engine::remote::ws::Ws>(&endpoint).await?;
    db.signin(surrealdb::opt::auth::Root {
        username: "root".to_string(),
        password: root_password.to_string(),
    })
    .await?;
    db.query(sql).await?.check()?;
    Ok(())
}

/// A test listener that writes `MOCK\n` then reads the SurrealQL. The root
/// password is not sent. Anything else is a real graph endpoint.
#[cfg(test)]
async fn try_mock(
    endpoint: &str,
    sql: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(mut stream) = tokio::net::TcpStream::connect(endpoint).await else {
        return Ok(false);
    };
    let mut banner = [0u8; 5];
    let read = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        stream.read_exact(&mut banner),
    )
    .await;
    if !matches!(read, Ok(Ok(_))) || &banner != b"MOCK\n" {
        return Ok(false);
    }
    stream.write_all(sql.as_bytes()).await?;
    stream.shutdown().await?;
    let mut buf = Vec::new();
    if let Err(err) = stream.read_to_end(&mut buf).await {
        return Err(format!("connection reset: {err}").into());
    }
    let text = String::from_utf8_lossy(&buf);
    let text = text.trim();
    if text == "OK" {
        return Ok(true);
    }
    if text.is_empty() {
        return Err("connection reset before the graph reply".into());
    }
    Err(text.to_string().into())
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
        if surreal_bin.contains('\0') {
            return Err("[graph] surreal_bin must not contain a NUL byte".into());
        }
        if surreal_bin.trim().is_empty() {
            return Err("[graph] surreal_bin must not be empty".into());
        }
        let path = PathBuf::from(surreal_bin);
        if !path.is_absolute() {
            return Err("[graph] surreal_bin must be an absolute path".into());
        }
        let meta = std::fs::metadata(&path)
            .map_err(|err| format!("SurrealDB binary not found at {}: {err}", path.display()))?;
        if !meta.is_file() {
            return Err(format!(
                "[graph] surreal_bin {} is not a regular file",
                path.display()
            )
            .into());
        }
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(format!("[graph] surreal_bin {} is not executable", path.display()).into());
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

type BoxError = Box<dyn std::error::Error + Send + Sync>;

async fn provision_inner(
    endpoint: &str,
    root_password: &str,
    pulse_name: &str,
    pulse_dir: &Path,
    rotate: bool,
    crash_after_define: bool,
) -> Result<ProvisionOutcome, BoxError> {
    if !crate::config::validate::is_plain_pulse_name(pulse_name) {
        return Err(format!("'{pulse_name}' is not a plain pulse name").into());
    }
    let endpoint = dial_addr(endpoint)?;
    let secrets_dir = pulse_dir.join("secrets");
    if !rotate {
        return provision_new(
            &endpoint,
            root_password,
            pulse_name,
            pulse_dir,
            &secrets_dir,
        )
        .await;
    }
    rotate_password(
        &endpoint,
        root_password,
        pulse_name,
        pulse_dir,
        &secrets_dir,
        crash_after_define,
    )
    .await
}

async fn provision_new(
    endpoint: &str,
    root_password: &str,
    pulse_name: &str,
    pulse_dir: &Path,
    secrets_dir: &Path,
) -> Result<ProvisionOutcome, BoxError> {
    if let Some(dir) = open_secrets(secrets_dir)? {
        if child_present(&dir, PASSWORD_NEW) {
            return Err(pending_rotation_message().into());
        }
        match password_from_dir(&dir) {
            GraphPassword::Ready => {
                info!("Pulse '{pulse_name}' already provisioned (password file exists)");
                return Ok(ProvisionOutcome::Already);
            }
            GraphPassword::Fault(reason) => return Err(reason.into()),
            GraphPassword::Missing => {}
        }
    }
    recall_preflight(pulse_dir)?;
    info!("Provisioning SurrealDB database for pulse '{pulse_name}'");
    let pulse_password = generate_password();
    let sql = provision_sql(pulse_name, &pulse_password, false);
    if let Err(err) = apply_statements(endpoint, root_password, &sql).await {
        let err = redact(&err.to_string(), root_password, &pulse_password);
        if user_already_exists(&err) {
            return Err(user_exists_message().into());
        }
        return Err(err.into());
    }
    let dir = ensure_secrets_dir(secrets_dir)?;
    if let Err(err) = write_secret(&dir, PASSWORD_NAME, pulse_password.as_bytes(), 0o600) {
        let err = redact(&err.to_string(), root_password, &pulse_password);
        return Err(format!("{err}; {}", unsaved_password_message()).into());
    }
    write_recall(pulse_dir, endpoint, pulse_name)
        .map_err(|err| redact(&err.to_string(), root_password, &pulse_password))?;
    info!("Pulse '{pulse_name}' provisioned successfully");
    Ok(ProvisionOutcome::Created)
}

async fn rotate_password(
    endpoint: &str,
    root_password: &str,
    pulse_name: &str,
    pulse_dir: &Path,
    secrets_dir: &Path,
    crash_after_define: bool,
) -> Result<ProvisionOutcome, BoxError> {
    let dir = ensure_secrets_dir(secrets_dir)?;
    recall_preflight(pulse_dir)?;
    let pulse_password = match read_side_password(&dir) {
        Side::Absent => {
            let password = generate_password();
            write_secret(&dir, PASSWORD_NEW, password.as_bytes(), 0o600)
                .map_err(|err| redact(&err.to_string(), root_password, &password))?;
            password
        }
        Side::Ready(password) => password,
        Side::Fault(reason) => return Err(reason.into()),
    };
    let sql = provision_sql(pulse_name, &pulse_password, true);
    if let Err(err) = apply_statements(endpoint, root_password, &sql).await {
        let err = redact(&err.to_string(), root_password, &pulse_password);
        if reply_may_have_applied(&err) {
            return Err(format!("{err}; {}", pending_rotation_message()).into());
        }
        remove_side_file(&dir);
        return Err(err.into());
    }
    if crash_after_define {
        return Err(
            "stopped after the database user was defined and before the password file was replaced"
                .into(),
        );
    }
    rename_side_into_place(&dir)
        .map_err(|err| redact(&err.to_string(), root_password, &pulse_password))?;
    verify_installed_password(&dir, &pulse_password)
        .map_err(|err| redact(&err.to_string(), root_password, &pulse_password))?;
    write_recall(pulse_dir, endpoint, pulse_name)
        .map_err(|err| redact(&err.to_string(), root_password, &pulse_password))?;
    info!("Pulse '{pulse_name}' provisioned successfully");
    Ok(ProvisionOutcome::Created)
}

pub(crate) fn pending_rotation_message() -> &'static str {
    "a rotation is pending in secrets/graph-password.new; \
     run `pulse-null graph provision --rotate` as the owner to finish it"
}

pub(crate) fn missing_password_message() -> &'static str {
    "secrets/graph-password is missing; graph features are unavailable. \
     Run `pulse-null graph provision` as the owner. If the database user already exists, \
     run `pulse-null graph provision --rotate`"
}

pub(crate) fn user_exists_message() -> &'static str {
    "the database user already exists but secrets/graph-password is missing or invalid; \
     run `pulse-null graph provision --rotate` as the owner to set a new password"
}

fn unsaved_password_message() -> &'static str {
    "the database now holds a password that was not saved; \
     run `pulse-null graph provision --rotate` as the owner to set a new password"
}

fn user_already_exists(err: &str) -> bool {
    err.to_ascii_lowercase().contains("already exists")
}

/// A dropped connection can mean the server applied the DEFINE and the reply
/// was lost. An explicit server rejection is anything else.
fn reply_may_have_applied(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    [
        "connection reset",
        "reset by peer",
        "broken pipe",
        "timed out",
        "timeout",
        "unexpected eof",
        "early eof",
        "transport",
        "connection aborted",
        "connection closed",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// `secrets/graph-password` is absent and `secrets/graph-password.new` is present.
pub(crate) fn rotation_is_pending(secrets_dir: &Path) -> bool {
    let Ok(dir) = open_dir_nofollow(secrets_dir) else {
        return false;
    };
    matches!(password_from_dir(&dir), GraphPassword::Missing) && child_present(&dir, PASSWORD_NEW)
}

/// A usable `secrets/graph-password`, or why the file that is there cannot be used.
pub(crate) fn inspect_graph_password(path: &Path) -> GraphPassword {
    inspect_graph_password_as(path, current_euid())
}

pub(crate) fn inspect_graph_password_as(path: &Path, euid: u32) -> GraphPassword {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return GraphPassword::Fault("secrets/graph-password is not a regular file".into());
    };
    let dir = match open_dir_nofollow(parent) {
        Ok(dir) => dir,
        Err(err) if not_found(&err) => return GraphPassword::Missing,
        Err(err) => return GraphPassword::Fault(err.to_string()),
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return GraphPassword::Fault("secrets/graph-password is not a regular file".into());
    };
    let Ok(name) = CString::new(name) else {
        return GraphPassword::Fault("secrets/graph-password is not a regular file".into());
    };
    password_at(&dir, &name, euid)
}

fn password_from_dir(dir: &OwnedFd) -> GraphPassword {
    let Ok(name) = CString::new(PASSWORD_NAME) else {
        return GraphPassword::Fault("secrets/graph-password is not a regular file".into());
    };
    password_at(dir, &name, current_euid())
}

fn password_at(dir: &OwnedFd, name: &CString, euid: u32) -> GraphPassword {
    match open_child(dir, name) {
        OpenedChild::Missing => GraphPassword::Missing,
        OpenedChild::Symlink => {
            GraphPassword::Fault("secrets/graph-password is a symlink, not a regular file".into())
        }
        OpenedChild::Other => {
            GraphPassword::Fault("secrets/graph-password is not a regular file".into())
        }
        OpenedChild::File(file) => classify_password_fd(&file, euid),
    }
}

fn named_password(named: Named, euid: u32) -> GraphPassword {
    match named {
        Named::Missing => GraphPassword::Missing,
        Named::Symlink => {
            GraphPassword::Fault("secrets/graph-password is a symlink, not a regular file".into())
        }
        Named::Other => GraphPassword::Fault("secrets/graph-password is not a regular file".into()),
        Named::TooLarge => GraphPassword::Fault("secrets/graph-password is too large".into()),
        Named::File { mode, uid, bytes } => classify_password(mode, uid, &bytes, euid),
    }
}

/// Classify an already-open password fd. Type, owner, mode, and size come from
/// `fstat` on this fd, and the bytes come from the same fd.
pub(crate) fn classify_password_fd(file: &File, euid: u32) -> GraphPassword {
    named_password(named_from_fd(file.as_raw_fd(), READ_CAP), euid)
}

fn classify_password(mode: u32, uid: u32, bytes: &[u8], euid: u32) -> GraphPassword {
    if uid != euid {
        return GraphPassword::Fault("secrets/graph-password is not owned by the caller".into());
    }
    let mode = mode & 0o777;
    if mode & 0o077 != 0 {
        return GraphPassword::Fault(format!(
            "secrets/graph-password has mode {mode:04o}; chmod 600 secrets/graph-password"
        ));
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return GraphPassword::Fault("secrets/graph-password is not valid UTF-8".into());
    };
    if text.as_bytes().contains(&0) {
        return GraphPassword::Fault("secrets/graph-password contains a NUL byte".into());
    }
    if text.trim().is_empty() {
        return GraphPassword::Fault("secrets/graph-password is empty".into());
    }
    GraphPassword::Ready
}

fn not_found(err: &BoxError) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound)
}

enum Side {
    Absent,
    Ready(String),
    Fault(String),
}

fn read_side_password(dir: &OwnedFd) -> Side {
    let Ok(name) = CString::new(PASSWORD_NEW) else {
        return Side::Fault("secrets/graph-password.new is not a regular file".into());
    };
    match read_named(dir, &name) {
        Named::Missing => Side::Absent,
        Named::Symlink => {
            Side::Fault("secrets/graph-password.new is a symlink, not a regular file".into())
        }
        Named::Other => Side::Fault("secrets/graph-password.new is not a regular file".into()),
        Named::TooLarge => Side::Fault("secrets/graph-password.new is too large".into()),
        Named::File { mode, uid, bytes } => {
            if uid != current_euid() {
                return Side::Fault("secrets/graph-password.new is not owned by the caller".into());
            }
            let mode = mode & 0o777;
            if mode & 0o077 != 0 {
                return Side::Fault(format!(
                    "secrets/graph-password.new has mode {mode:04o}; chmod 600 secrets/graph-password.new"
                ));
            }
            match std::str::from_utf8(&bytes) {
                Ok(text) if is_password_text(text) => Side::Ready(text.to_string()),
                Ok(_) => Side::Fault("secrets/graph-password.new is not a graph password".into()),
                Err(_) => Side::Fault("secrets/graph-password.new is not valid UTF-8".into()),
            }
        }
    }
}

fn is_password_text(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| PASSWORD_CHARSET.contains(&byte))
}

fn remove_side_file(dir: &OwnedFd) {
    let Ok(name) = CString::new(PASSWORD_NEW) else {
        return;
    };
    unlink_child(dir, &name);
}

fn rename_side_into_place(dir: &OwnedFd) -> Result<(), BoxError> {
    let src = CString::new(PASSWORD_NEW).map_err(|_| "side file name contains a NUL byte")?;
    let dest = CString::new(PASSWORD_NAME).expect("fixed name");
    // SAFETY: both names are NUL-terminated children of this directory fd.
    // renameat replaces the destination entry and does not follow a symlink.
    let rc = unsafe {
        libc::renameat(
            dir.as_raw_fd(),
            src.as_ptr(),
            dir.as_raw_fd(),
            dest.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    fsync_dir(dir)?;
    Ok(())
}

fn verify_installed_password(dir: &OwnedFd, password: &str) -> Result<(), BoxError> {
    let name = CString::new(PASSWORD_NAME).expect("fixed name");
    match read_named(dir, &name) {
        Named::File { mode, uid, bytes }
            if uid == current_euid()
                && mode & 0o777 & 0o077 == 0
                && bytes == password.as_bytes() =>
        {
            Ok(())
        }
        Named::File { .. } => Err(
            "secrets/graph-password does not match the password that was defined; \
             run `pulse-null graph provision --rotate` as the owner"
                .into(),
        ),
        _ => Err(
            "secrets/graph-password is not a private regular file after rotation; \
             run `pulse-null graph provision --rotate` as the owner"
                .into(),
        ),
    }
}

enum Named {
    Missing,
    Symlink,
    Other,
    TooLarge,
    File { mode: u32, uid: u32, bytes: Vec<u8> },
}

fn child_present(dir: &OwnedFd, name: &str) -> bool {
    let Ok(name) = CString::new(name) else {
        return true;
    };
    !matches!(open_child(dir, &name), OpenedChild::Missing)
}

enum OpenedChild {
    Missing,
    Symlink,
    Other,
    File(File),
}

fn open_child(dir: &OwnedFd, name: &CString) -> OpenedChild {
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: `name` is a child of this directory fd. `O_NOFOLLOW` refuses a
    // symlink. `O_NONBLOCK` keeps a FIFO from blocking the open.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOENT) => OpenedChild::Missing,
            Some(libc::ELOOP) if nofollow_is_symlink(dir, name) => OpenedChild::Symlink,
            _ => OpenedChild::Other,
        };
    }
    // SAFETY: `openat` succeeded, so this process owns `fd`.
    OpenedChild::File(unsafe { File::from_raw_fd(fd) })
}

fn read_named(dir: &OwnedFd, name: &CString) -> Named {
    read_capped(dir, name, READ_CAP)
}

fn read_capped(dir: &OwnedFd, name: &CString, cap: u64) -> Named {
    match open_child(dir, name) {
        OpenedChild::Missing => Named::Missing,
        OpenedChild::Symlink => Named::Symlink,
        OpenedChild::Other => Named::Other,
        OpenedChild::File(file) => named_from_fd(file.as_raw_fd(), cap),
    }
}

fn named_from_fd(fd: i32, cap: u64) -> Named {
    let info = match fstat_fd(fd) {
        Ok(info) => info,
        Err(_) => return Named::Other,
    };
    if (info.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Named::Other;
    }
    match read_fd_capped(fd, info.st_size, cap) {
        Ok(bytes) => Named::File {
            mode: info.st_mode,
            uid: info.st_uid,
            bytes,
        },
        Err(ReadFail::TooLarge) => Named::TooLarge,
        Err(ReadFail::Io) => Named::Other,
    }
}

enum ReadFail {
    TooLarge,
    Io,
}

fn read_fd_capped(fd: i32, size: i64, cap: u64) -> Result<Vec<u8>, ReadFail> {
    if size < 0 || size as u64 > cap {
        return Err(ReadFail::TooLarge);
    }
    let cap_usize = cap as usize;
    let mut buf = vec![0u8; cap_usize + 1];
    let mut got = 0usize;
    while got <= cap_usize {
        // SAFETY: `fd` is open and `buf` is writable for `got..`. `pread` does
        // not change the file offset and does not follow a new path.
        let n = unsafe {
            libc::pread(
                fd,
                buf[got..].as_mut_ptr().cast::<libc::c_void>(),
                buf.len() - got,
                got as i64,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ReadFail::Io);
        }
        if n == 0 {
            break;
        }
        got += n as usize;
        if got > cap_usize {
            return Err(ReadFail::TooLarge);
        }
    }
    buf.truncate(got);
    Ok(buf)
}

fn fstat_fd(fd: i32) -> Result<libc::stat, std::io::Error> {
    let mut info = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `info` is a stat this function owns. `fd` is a descriptor this process owns.
    if unsafe { libc::fstat(fd, &mut info) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info)
}

/// `lstat` of one directory entry, used only to choose a message after `openat` returned `ELOOP`.
fn nofollow_is_symlink(dir: &OwnedFd, name: &CString) -> bool {
    let mut info = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `name` is NUL-terminated. `AT_SYMLINK_NOFOLLOW` does not follow a symlink.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut info,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    rc == 0 && (info.st_mode & libc::S_IFMT) == libc::S_IFLNK
}

/// `lstat` of a path, used only to choose a message after `open` returned `ELOOP`.
fn path_is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

fn current_euid() -> u32 {
    // SAFETY: geteuid takes no pointer and cannot fail.
    unsafe { libc::geteuid() }
}

/// Open `secrets/` when it exists. Does not change its mode.
///
/// `Ok(None)` means the directory is absent. A symlink is refused from the
/// `ELOOP` of this open; `lstat` only chooses that message.
fn open_secrets(dir: &Path) -> Result<Option<OwnedFd>, BoxError> {
    match open_nofollow_dir(dir) {
        Ok(fd) => Ok(Some(fd)),
        Err(err) if err.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) && path_is_symlink(dir) => {
            Err("refusing a symlink at secrets/".into())
        }
        Err(err) if err.raw_os_error() == Some(libc::ENOTDIR) => {
            Err("secrets is not a directory".into())
        }
        Err(err) => Err(err.into()),
    }
}

fn ensure_secrets_dir(dir: &Path) -> Result<OwnedFd, BoxError> {
    match open_secrets(dir)? {
        Some(fd) => {
            let info = fstat_fd(fd.as_raw_fd())?;
            if (info.st_mode & libc::S_IFMT) != libc::S_IFDIR {
                return Err("secrets is not a directory".into());
            }
            if info.st_uid != current_euid() {
                return Err("secrets is not owned by the caller".into());
            }
            fchmod_fd(fd.as_raw_fd(), 0o700)?;
            Ok(fd)
        }
        None => mkdir_private(dir),
    }
}

fn mkdir_private(dir: &Path) -> Result<OwnedFd, BoxError> {
    let parent = dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("secrets has no parent")?;
    let parent_fd = open_dir_nofollow(parent)?;
    let name = CString::new(
        dir.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("secrets"),
    )
    .map_err(|_| "secrets path contains a NUL byte")?;
    // SAFETY: `parent_fd` is a directory and `name` is NUL-terminated.
    // mkdirat's mode is masked by umask, so fchmod sets 0700 on the fd.
    let rc = unsafe { libc::mkdirat(parent_fd.as_raw_fd(), name.as_ptr(), 0o700) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(err.into());
        }
    }
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: `O_NOFOLLOW` refuses a symlink that replaced the new directory.
    let fd = unsafe { libc::openat(parent_fd.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ELOOP) && nofollow_is_symlink(&parent_fd, &name) {
            return Err("refusing a symlink at secrets/".into());
        }
        return Err(err.into());
    }
    // SAFETY: `openat` succeeded, so this process owns `fd`.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let info = fstat_fd(owned.as_raw_fd())?;
    if (info.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        return Err("secrets is not a directory".into());
    }
    if info.st_uid != current_euid() {
        return Err("secrets is not owned by the caller".into());
    }
    fchmod_fd(owned.as_raw_fd(), 0o700)?;
    Ok(owned)
}

fn fchmod_fd(fd: i32, mode: u32) -> Result<(), BoxError> {
    // SAFETY: `fd` is a descriptor this process owns.
    if unsafe { libc::fchmod(fd, mode) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn write_secret(dir: &OwnedFd, dest_name: &str, bytes: &[u8], mode: u32) -> Result<(), BoxError> {
    let tmp = format!(
        ".{dest_name}.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|stamp| stamp.as_nanos())
            .unwrap_or(0)
    );
    write_temp(dir, &tmp, dest_name, bytes, mode, true)
}

fn write_temp(
    dir: &OwnedFd,
    temp_name: &str,
    dest_name: &str,
    bytes: &[u8],
    mode: u32,
    unique: bool,
) -> Result<(), BoxError> {
    let temp = CString::new(temp_name).map_err(|_| "temporary name contains a NUL byte")?;
    let dest = CString::new(dest_name).map_err(|_| "destination name contains a NUL byte")?;
    let mut file = match open_exclusive(dir, &temp) {
        Ok(file) => file,
        Err(err) if !unique && err.raw_os_error() == Some(libc::ELOOP) => {
            return Err(temp_open_error(err, dir, &temp, temp_name).into());
        }
        Err(err) if !unique && err.raw_os_error() == Some(libc::EEXIST) => {
            // `O_EXCL|O_NOFOLLOW` on a symlink returns `EEXIST` on Linux.
            // `lstat` only chooses the message; the symlink is not removed.
            if nofollow_is_symlink(dir, &temp) {
                return Err(format!("refusing a symlink at memory/{temp_name}").into());
            }
            unlink_child(dir, &temp);
            match open_exclusive(dir, &temp) {
                Ok(file) => file,
                Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
                    return Err(temp_open_error(err, dir, &temp, temp_name).into());
                }
                Err(err) => return Err(err.into()),
            }
        }
        Err(err) => return Err(err.into()),
    };
    let wrote = (|| -> std::io::Result<()> {
        // Create mode is masked by umask, so set the mode on the inode itself.
        if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = wrote {
        unlink_child(dir, &temp);
        return Err(err.into());
    }
    // SAFETY: both names are NUL-terminated children of this directory fd.
    // renameat replaces the destination entry and does not follow a symlink.
    let rc = unsafe {
        libc::renameat(
            dir.as_raw_fd(),
            temp.as_ptr(),
            dir.as_raw_fd(),
            dest.as_ptr(),
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        unlink_child(dir, &temp);
        return Err(err.into());
    }
    fsync_dir(dir)
}

fn open_exclusive(dir: &OwnedFd, name: &CString) -> Result<File, std::io::Error> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open directory. `O_EXCL|O_NOFOLLOW` refuses a symlink
    // at the temporary name.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: `openat` succeeded, so this process owns `fd`.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn temp_open_error(err: std::io::Error, dir: &OwnedFd, name: &CString, temp_name: &str) -> String {
    if nofollow_is_symlink(dir, name) {
        format!("refusing a symlink at memory/{temp_name}")
    } else {
        err.to_string()
    }
}

fn unlink_child(dir: &OwnedFd, name: &CString) {
    // SAFETY: `name` is a child of this directory fd. The final component is
    // removed even when it is a symlink; the target is not followed.
    unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
}

fn fsync_dir(dir: &OwnedFd) -> Result<(), BoxError> {
    // SAFETY: `dir` is a directory descriptor this process owns.
    if unsafe { libc::fsync(dir.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn open_dir_nofollow(dir: &Path) -> Result<OwnedFd, BoxError> {
    open_nofollow_dir(dir).map_err(Into::into)
}

fn open_nofollow_dir(dir: &Path) -> Result<OwnedFd, std::io::Error> {
    let path = CString::new(dir.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte")
    })?;
    // `O_DIRECTORY` would report a symlink as `ENOTDIR`. Without it, `O_NOFOLLOW`
    // fails that open with `ELOOP`. `O_NONBLOCK` keeps a FIFO from blocking;
    // `fstat` then rejects anything that is not a directory.
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: `path` is NUL-terminated. `O_NOFOLLOW` refuses a symlink.
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `open` succeeded, so this process owns `fd`.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let info = fstat_fd(owned.as_raw_fd())?;
    if (info.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    Ok(owned)
}

fn ensure_memory_dir(dir: &Path) -> Result<(), BoxError> {
    match open_nofollow_dir(dir) {
        Ok(_) => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::ENOENT) => {
            let parent = dir
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or("memory has no parent")?;
            let parent_fd = open_dir_nofollow(parent)?;
            let name = CString::new("memory").expect("fixed name");
            // SAFETY: `parent_fd` is a directory and `name` is NUL-terminated.
            let rc = unsafe { libc::mkdirat(parent_fd.as_raw_fd(), name.as_ptr(), 0o755) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(err.into());
                }
            }
            let created = open_nofollow_dir(dir)?;
            let info = fstat_fd(created.as_raw_fd())?;
            if (info.st_mode & libc::S_IFMT) != libc::S_IFDIR {
                return Err("memory is not a directory".into());
            }
            Ok(())
        }
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) && path_is_symlink(dir) => {
            Err("refusing a symlink at memory/".into())
        }
        Err(err) if err.raw_os_error() == Some(libc::ENOTDIR) => {
            Err("memory is not a directory".into())
        }
        Err(err) => Err(err.into()),
    }
}

fn write_recall(pulse_dir: &Path, endpoint: &str, pulse_name: &str) -> Result<(), BoxError> {
    write_recall_with(pulse_dir, endpoint, pulse_name, "secrets/graph-password")
}

/// Refuse an unusable `memory/` or `.recall-echo.toml` before any DEFINE.
fn recall_preflight(pulse_dir: &Path) -> Result<(), BoxError> {
    let memory = pulse_dir.join("memory");
    ensure_memory_dir(&memory)?;
    let dir = open_dir_nofollow(&memory)?;
    let name = CString::new(RECALL_NAME).expect("fixed name");
    match read_capped(&dir, &name, TOML_CAP) {
        Named::Missing => Ok(()),
        Named::Symlink => Err(format!(
            "refusing a symlink at {}",
            memory.join(RECALL_NAME).display()
        )
        .into()),
        Named::TooLarge => {
            Err(format!("{} is too large", memory.join(RECALL_NAME).display()).into())
        }
        Named::Other => Err(format!(
            "{} is not a regular file",
            memory.join(RECALL_NAME).display()
        )
        .into()),
        Named::File { bytes, .. } => {
            String::from_utf8(bytes).map_err(|_| {
                format!("{} is not valid UTF-8", memory.join(RECALL_NAME).display())
            })?;
            Ok(())
        }
    }
}

fn write_recall_with(
    pulse_dir: &Path,
    endpoint: &str,
    pulse_name: &str,
    password_file: &str,
) -> Result<(), BoxError> {
    let memory = pulse_dir.join("memory");
    ensure_memory_dir(&memory)?;
    let dir = open_dir_nofollow(&memory)?;
    let final_name = CString::new(RECALL_NAME).expect("fixed name");
    let (existing, mode) = match read_capped(&dir, &final_name, TOML_CAP) {
        Named::Missing => (String::new(), 0o600),
        Named::Symlink => {
            return Err(format!(
                "refusing a symlink at {}",
                memory.join(RECALL_NAME).display()
            )
            .into());
        }
        Named::TooLarge => {
            return Err(format!("{} is too large", memory.join(RECALL_NAME).display()).into());
        }
        Named::Other => {
            return Err(format!(
                "{} is not a regular file",
                memory.join(RECALL_NAME).display()
            )
            .into());
        }
        Named::File { mode, bytes, .. } => {
            let text = String::from_utf8(bytes).map_err(|_| {
                format!("{} is not valid UTF-8", memory.join(RECALL_NAME).display())
            })?;
            (text, preserved_mode(mode))
        }
    };
    let body = render_recall(&existing, endpoint, pulse_name, password_file);
    write_temp(&dir, RECALL_TEMP, RECALL_NAME, body.as_bytes(), mode, false)
}

fn preserved_mode(mode: u32) -> u32 {
    let mode = mode & 0o777;
    if mode & !0o644 == 0 && mode != 0o644 {
        mode
    } else {
        0o600
    }
}

fn render_recall(existing: &str, endpoint: &str, pulse_name: &str, password_file: &str) -> String {
    let mut content = existing.to_string();
    if let Some(start) = content.find("[graph]") {
        let end = content[start + 7..]
            .find("\n[")
            .map(|index| start + 7 + index)
            .unwrap_or(content.len());
        content.replace_range(start..end, "");
    }
    let section = format!(
        "\n[graph]\nmode = \"server\"\nurl = {url}\nnamespace = {namespace}\ndatabase = {database}\nusername = {username}\npassword_file = {password}\n",
        url = toml_basic(endpoint),
        namespace = toml_basic(NAMESPACE),
        database = toml_basic(pulse_name),
        username = toml_basic(pulse_name),
        password = toml_basic(password_file),
    );
    content.push_str(&section);
    content.trim_start().to_string()
}

fn toml_basic(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other.is_control() => out.push_str(&format!("\\u{:04X}", other as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
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
        std::fs::set_permissions(&chosen, std::fs::Permissions::from_mode(0o755)).unwrap();
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

    #[test]
    fn redact_covers_a_one_to_three_character_secret() {
        assert_eq!(redact("failed ab", "ab", "zzzz"), "failed [redacted]");
        assert_eq!(redact("saw q", "q", ""), "saw [redacted]");
        assert_eq!(redact("no secret", "", ""), "no secret");
        assert_eq!(
            redact("abcd and ab", "ab", "abcd"),
            "[redacted] and [redacted]"
        );
    }

    #[test]
    fn a_password_owned_by_someone_else_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph-password");
        std::fs::write(&path, "pulse-secret").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let fault = inspect_graph_password_as(&path, current_euid().wrapping_add(1));
        let GraphPassword::Fault(reason) = fault else {
            panic!("owned by the caller");
        };
        assert!(reason.contains("not owned by the caller"), "{reason}");
    }

    #[test]
    fn a_nul_or_non_utf8_password_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let nul = dir.path().join("graph-password");
        std::fs::write(&nul, b"ab\0cd").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&nul, std::fs::Permissions::from_mode(0o600)).unwrap();
        let GraphPassword::Fault(reason) = inspect_graph_password(&nul) else {
            panic!("a NUL password was accepted");
        };
        assert!(reason.contains("NUL"), "{reason}");
        assert!(!reason.contains('\0'), "{reason}");

        std::fs::create_dir(dir.path().join("nested")).unwrap();
        let bad = dir.path().join("nested/graph-password");
        std::fs::write(&bad, [0xff, 0xfe]).unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o600)).unwrap();
        let GraphPassword::Fault(reason) = inspect_graph_password(&bad) else {
            panic!("a non-UTF-8 password was accepted");
        };
        assert!(reason.contains("UTF-8"), "{reason}");
    }

    #[test]
    fn recall_echo_toml_round_trips_hostile_values() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join("memory");
        std::fs::create_dir(&memory).unwrap();
        let path = memory.join(".recall-echo.toml");
        std::fs::write(&path, "# keep me\n[other]\nnote = \"stay\"\n").unwrap();
        let endpoint = format!(
            "127.0.0.1:9\"\n[evil]\npassword_file = \"/tmp/pwned\" # \u{1F600}\\{}",
            "a\"".repeat(2000)
        );
        let name = "echo\"]\n# \u{1F600}\\";
        let password_file = "secrets/graph-password]\nkey = 1";
        write_recall_with(dir.path(), &endpoint, name, password_file).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# keep me\n[other]\nnote = \"stay\"\n"),
            "{text}"
        );
        let value: toml::Value = toml::from_str(&text).unwrap();
        let graph = value.get("graph").unwrap().as_table().unwrap();
        assert_eq!(graph.len(), 6, "{graph:?}");
        assert_eq!(graph.get("url").unwrap().as_str().unwrap(), endpoint);
        assert_eq!(graph.get("database").unwrap().as_str().unwrap(), name);
        assert_eq!(graph.get("username").unwrap().as_str().unwrap(), name);
        assert_eq!(
            graph.get("password_file").unwrap().as_str().unwrap(),
            password_file
        );
        assert!(value.get("evil").is_none(), "{value}");
        assert_eq!(
            value
                .get("other")
                .unwrap()
                .get("note")
                .unwrap()
                .as_str()
                .unwrap(),
            "stay"
        );
    }

    #[test]
    fn recall_echo_toml_refuses_a_symlink_and_leaves_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join("memory");
        std::fs::create_dir(&memory).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("echo.toml");
        std::fs::write(&target, "original").unwrap();
        std::os::unix::fs::symlink(&target, memory.join(".recall-echo.toml")).unwrap();
        let err = write_recall(dir.path(), "127.0.0.1:9", "echo").unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    #[test]
    fn recall_echo_toml_refuses_a_symlinked_temp_name() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join("memory");
        std::fs::create_dir(&memory).unwrap();
        let planted = elsewhere_link(&memory);
        let err = write_recall(dir.path(), "127.0.0.1:9", "echo").unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(err.to_string().contains(".recall-echo.toml.tmp"), "{err}");
        assert!(!planted.exists());
        assert!(!memory.join(".recall-echo.toml").exists());
    }

    #[test]
    fn recall_echo_toml_keeps_a_stricter_mode_and_other_content() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join("memory");
        std::fs::create_dir(&memory).unwrap();
        let path = memory.join(".recall-echo.toml");
        std::fs::write(&path, "# comment\n[other]\nx = 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_recall(dir.path(), "127.0.0.1:9", "echo").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# comment\n[other]\nx = 1\n"), "{text}");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let wide = tempfile::tempdir().unwrap();
        let wide_memory = wide.path().join("memory");
        std::fs::create_dir(&wide_memory).unwrap();
        let wide_path = wide_memory.join(".recall-echo.toml");
        std::fs::write(&wide_path, "# wide\n").unwrap();
        std::fs::set_permissions(&wide_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_recall(wide.path(), "127.0.0.1:9", "echo").unwrap();
        assert_eq!(
            std::fs::metadata(&wide_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn elsewhere_link(memory: &Path) -> PathBuf {
        let target = memory.join("not-written");
        std::os::unix::fs::symlink(&target, memory.join(".recall-echo.toml.tmp")).unwrap();
        target
    }

    #[test]
    fn classify_password_fd_reports_the_open_file() {
        let dir = tempfile::tempdir().unwrap();
        let euid = current_euid();
        use std::os::unix::fs::PermissionsExt;

        let regular = dir.path().join("regular");
        std::fs::write(&regular, "pulse-secret").unwrap();
        std::fs::set_permissions(&regular, std::fs::Permissions::from_mode(0o600)).unwrap();
        let regular_file = std::fs::File::open(&regular).unwrap();
        assert!(matches!(
            classify_password_fd(&regular_file, euid),
            GraphPassword::Ready
        ));
        assert!(matches!(
            inspect_graph_password(&regular),
            GraphPassword::Ready
        ));

        let wide = dir.path().join("wide");
        std::fs::write(&wide, "pulse-secret").unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o644)).unwrap();
        let wide_file = std::fs::File::open(&wide).unwrap();
        assert_fd_matches(&wide, &wide_file, euid, "0644");

        let fifo = dir.path().join("fifo");
        let c_fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_fifo` is NUL-terminated. `O_NONBLOCK` keeps the open from waiting.
        let created = unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) };
        assert_eq!(created, 0);
        let fd = unsafe {
            libc::open(
                c_fifo.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        let fifo_file = unsafe { File::from_raw_fd(fd) };
        assert_fd_matches(&fifo, &fifo_file, euid, "not a regular file");

        let big = dir.path().join("big");
        std::fs::write(&big, vec![b'a'; READ_CAP as usize + 1]).unwrap();
        std::fs::set_permissions(&big, std::fs::Permissions::from_mode(0o600)).unwrap();
        let big_file = std::fs::File::open(&big).unwrap();
        assert_fd_matches(&big, &big_file, euid, "too large");

        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let folder_file = std::fs::File::open(&folder).unwrap();
        assert_fd_matches(&folder, &folder_file, euid, "not a regular file");
    }

    fn assert_fd_matches(path: &Path, file: &File, euid: u32, needle: &str) {
        let from_fd = classify_password_fd(file, euid);
        let from_path = inspect_graph_password(path);
        match (from_fd, from_path) {
            (GraphPassword::Fault(left), GraphPassword::Fault(right)) => {
                assert_eq!(left, right);
                assert!(left.contains(needle), "{left}");
            }
            other => panic!("{path:?} disagreed: {other:?}"),
        }
    }

    #[test]
    fn an_oversized_graph_password_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph-password");
        std::fs::write(&path, vec![b'Z'; READ_CAP as usize + 1]).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let GraphPassword::Fault(reason) = inspect_graph_password(&path) else {
            panic!("an oversized password was accepted");
        };
        assert!(reason.contains("too large"), "{reason}");
        assert!(!reason.contains('Z'), "{reason}");
    }

    #[test]
    fn m1_a_fifo_directory_does_not_block_the_predicates() {
        let root = tempfile::tempdir().unwrap();
        let secrets = root.path().join("secrets");
        let name = std::ffi::CString::new(secrets.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let secrets_pending = secrets.clone();
        let pending = std::thread::spawn(move || rotation_is_pending(&secrets_pending));
        let started = std::time::Instant::now();
        while !pending.is_finished() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "rotation_is_pending hung"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!pending.join().unwrap());
        let password = secrets.join("graph-password");
        let inspected = std::thread::spawn(move || inspect_graph_password(&password));
        let started = std::time::Instant::now();
        while !inspected.is_finished() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "inspect_graph_password hung"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let GraphPassword::Fault(reason) = inspected.join().unwrap() else {
            panic!("a fifo secrets directory was usable");
        };
        assert!(
            reason.to_ascii_lowercase().contains("directory"),
            "{reason}"
        );
    }

    #[test]
    fn l1_a_password_file_that_differs_from_the_defined_bytes_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph-password");
        std::fs::write(&path, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let fd = open_dir_nofollow(dir.path()).unwrap();
        let err = verify_installed_password(&fd, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap_err();
        let err = err.to_string();
        assert!(err.contains("does not match"), "{err}");
        assert!(!err.contains('b'), "{err}");
        verify_installed_password(&fd, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    }
}
