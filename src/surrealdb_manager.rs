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
use std::os::unix::fs::{MetadataExt, PermissionsExt};
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

/// What [`provision_pulse`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// `secrets/graph-password` was written.
    Created,
    /// A regular `secrets/graph-password` was already there and was kept.
    Already,
}

/// Create the pulse's graph user, or keep the password file that is already there.
///
/// `root_password` is the SurrealDB root credential. It is sent only to the
/// graph endpoint and is not written, logged, or returned. The generated
/// pulse password is written only to `secrets/graph-password`. `rotate`
/// replaces that file and the database user's password.
pub async fn provision_pulse(
    endpoint: &str,
    root_password: &str,
    pulse_name: &str,
    pulse_dir: &Path,
    rotate: bool,
) -> Result<ProvisionOutcome, Box<dyn std::error::Error + Send + Sync>> {
    if !crate::config::validate::is_plain_pulse_name(pulse_name) {
        return Err(format!("'{pulse_name}' is not a plain pulse name").into());
    }
    let secrets_dir = pulse_dir.join("secrets");
    if !rotate && regular_file(&secrets_dir.join("graph-password")) {
        info!("Pulse '{pulse_name}' already provisioned (password file exists)");
        return Ok(ProvisionOutcome::Already);
    }

    info!("Provisioning SurrealDB database for pulse '{pulse_name}'");
    let pulse_password = generate_password();
    let sql = provision_sql(pulse_name, &pulse_password, rotate);
    apply_statements(endpoint, root_password, &sql)
        .await
        .map_err(|err| redact(&err.to_string(), root_password, &pulse_password))?;

    ensure_secrets_dir(&secrets_dir)?;
    write_graph_password(&secrets_dir, &pulse_password)
        .map_err(|err| redact(&err.to_string(), root_password, &pulse_password))?;
    let memory_dir = pulse_dir.join("memory");
    if !memory_dir.exists() {
        std::fs::create_dir(&memory_dir)?;
    }
    update_recall_echo_config(
        &memory_dir.join(".recall-echo.toml"),
        endpoint,
        pulse_name,
        "secrets/graph-password",
    )?;
    info!("Pulse '{pulse_name}' provisioned successfully");
    Ok(ProvisionOutcome::Created)
}

fn provision_sql(pulse_name: &str, pulse_password: &str, rotate: bool) -> String {
    let user = if rotate {
        format!("DEFINE USER {pulse_name} ON DATABASE PASSWORD '{pulse_password}' ROLES OWNER;")
    } else {
        format!(
            "DEFINE USER IF NOT EXISTS {pulse_name} ON DATABASE PASSWORD '{pulse_password}' ROLES OWNER;"
        )
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
    let mut out = text.to_string();
    for secret in [root_password, pulse_password] {
        if secret.len() >= 4 {
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
    #[cfg(test)]
    if try_mock(endpoint, sql).await? {
        return Ok(());
    }
    let db = surrealdb::Surreal::new::<surrealdb::engine::remote::ws::Ws>(endpoint).await?;
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
    let mut ack = [0u8; 2];
    stream.read_exact(&mut ack).await?;
    if &ack != b"OK" {
        return Err("graph mock did not accept the statement".into());
    }
    Ok(true)
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

fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

fn ensure_secrets_dir(dir: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match std::fs::symlink_metadata(dir) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(dir)?;
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err("refusing a symlink at secrets/".into());
        }
        Ok(meta) if !meta.is_dir() => return Err("secrets is not a directory".into()),
        Ok(meta) if meta.uid() != current_euid() => {
            return Err("secrets is not owned by the caller".into());
        }
        Ok(_) => {}
        Err(err) => return Err(err.into()),
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn current_euid() -> u32 {
    // SAFETY: geteuid takes no pointer and cannot fail.
    unsafe { libc::geteuid() }
}

fn write_graph_password(
    secrets: &Path,
    password: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let dir = open_dir_nofollow(secrets)?;
    let tmp = format!(
        ".graph-password.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp_name = CString::new(tmp).map_err(|_| "temporary name contains a NUL byte")?;
    let dest = CString::new("graph-password").expect("fixed name");
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open directory and `tmp_name` is NUL-terminated.
    // `O_EXCL|O_NOFOLLOW` refuses a symlink at the temporary name.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), tmp_name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: `openat` succeeded, so this process owns `fd`.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let wrote = (|| -> std::io::Result<()> {
        // SAFETY: `file` is a descriptor this process owns. Create mode is
        // masked by umask, so set 0600 on the inode itself.
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        file.write_all(password.as_bytes())?;
        Ok(())
    })();
    if let Err(err) = wrote {
        // SAFETY: the temporary name is a child of this directory fd.
        unsafe { libc::unlinkat(dir.as_raw_fd(), tmp_name.as_ptr(), 0) };
        return Err(err.into());
    }
    // SAFETY: both names are NUL-terminated children of this directory fd.
    // renameat replaces the destination and does not follow a symlink.
    let rc = unsafe {
        libc::renameat(
            dir.as_raw_fd(),
            tmp_name.as_ptr(),
            dir.as_raw_fd(),
            dest.as_ptr(),
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::unlinkat(dir.as_raw_fd(), tmp_name.as_ptr(), 0) };
        return Err(err.into());
    }
    Ok(())
}

fn open_dir_nofollow(dir: &Path) -> Result<OwnedFd, Box<dyn std::error::Error + Send + Sync>> {
    let path =
        CString::new(dir.as_os_str().as_bytes()).map_err(|_| "secrets path contains a NUL byte")?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `path` is NUL-terminated. `O_NOFOLLOW` refuses a symlink.
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: `open` succeeded, so this process owns `fd`.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn update_recall_echo_config(
    config_path: &Path,
    endpoint: &str,
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
url = "{endpoint}"
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
}
