//! `pulse-null graph provision`.
//!
//! The SurrealDB root password is one line on stdin. It is not an argument,
//! an environment variable, or a file. The process is made non-dumpable
//! before that line is read.

use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Config;
use crate::surrealdb_manager::{self, ProvisionOutcome};

/// Exit with `code` after the message is printed.
#[derive(Debug)]
pub struct ProvisionExit {
    pub code: i32,
    message: String,
}

impl std::fmt::Display for ProvisionExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProvisionExit {}

/// Define this pulse's graph user. The root password is read from stdin.
pub async fn run(url: Option<String>, rotate: bool) -> Result<(), Box<dyn std::error::Error>> {
    let _non_dumpable = crate::hardening::own_non_dumpable();
    let config = Config::load()?;
    let root = config.root_dir()?;
    run_in(&config, &root, url, rotate, &mut io::stdin().lock()).await
}

/// `run`, with the loaded config, pulse directory, and password reader supplied.
///
/// The endpoint is `--url` when it is set, otherwise the configured graph
/// endpoint (`test_addr` in tests, otherwise `127.0.0.1:8787`). Both go
/// through the loopback check.
pub async fn run_in(
    config: &Config,
    root: &Path,
    url: Option<String>,
    rotate: bool,
    reader: &mut impl BufRead,
) -> Result<(), Box<dyn std::error::Error>> {
    let argv = [OsString::from("graph"), OsString::from("provision")];
    if let Err(refusal) = crate::cli::owner_guard::require_owner(root, &argv) {
        return Err(Box::new(ProvisionExit {
            code: refusal.code,
            message: refusal.message,
        }));
    }
    let endpoint = endpoint_for(config, url.as_deref())?;
    let echo = silence_echo();
    let _signals = echo.as_ref().and_then(EchoSignals::install);
    provision_with(root, &config.pulse.name, &endpoint, reader, rotate)
        .await
        .map(|_| ())
}

/// `--url`, or the configured endpoint when the flag is absent.
pub(crate) fn endpoint_for(config: &Config, url: Option<&str>) -> Result<String, String> {
    match url {
        Some(url) => {
            surrealdb_manager::require_loopback(url)?;
            Ok(url.to_string())
        }
        None => {
            let raw = configured_addr(config);
            surrealdb_manager::require_loopback(&raw)
                .map_err(|err| format!("the configured graph endpoint is refused: {err}"))?;
            Ok(raw)
        }
    }
}

fn configured_addr(config: &Config) -> String {
    #[cfg(test)]
    if let Some(addr) = &config.graph.test_addr {
        return addr.clone();
    }
    #[cfg(not(test))]
    let _ = config;
    surrealdb_manager::default_addr()
}

/// The command body, with the password reader and pulse directory supplied.
pub async fn provision_with(
    pulse_dir: &Path,
    pulse_name: &str,
    endpoint: &str,
    reader: &mut impl BufRead,
    rotate: bool,
) -> Result<ProvisionOutcome, Box<dyn std::error::Error>> {
    surrealdb_manager::require_loopback(endpoint)?;
    let root_password = read_secret(reader)?;
    let outcome =
        surrealdb_manager::provision_pulse(endpoint, &root_password, pulse_name, pulse_dir, rotate)
            .await
            .map_err(|err| err.to_string())?;
    if outcome == ProvisionOutcome::Already {
        println!("already provisioned");
    }
    Ok(outcome)
}

/// One line, with only the trailing newline removed. Empty is exit 2.
fn read_secret(reader: &mut impl BufRead) -> Result<String, ProvisionExit> {
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|err| ProvisionExit {
        code: 1,
        message: format!("could not read stdin: {err}"),
    })?;
    if line.ends_with('\n') {
        line.pop();
    }
    if line.ends_with('\r') {
        line.pop();
    }
    if line.is_empty() {
        return Err(ProvisionExit {
            code: 2,
            message: "the root password on stdin is empty".into(),
        });
    }
    Ok(line)
}

/// Turn off terminal echo while this value is held.
struct EchoOff {
    saved: libc::termios,
}

impl EchoOff {
    fn arm() -> Option<Self> {
        if !io::stdin().is_terminal() {
            return None;
        }
        let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
        // SAFETY: `term` is a termios this function owns. fd 0 is stdin.
        if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
            return None;
        }
        let saved = term;
        term.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &term) } != 0 {
            return None;
        }
        Some(Self { saved })
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        restore_terminal_echo();
    }
}

fn silence_echo() -> Option<EchoOff> {
    let echo = EchoOff::arm()?;
    publish_saved_termios(echo.saved);
    Some(echo)
}

struct SavedEcho {
    ready: AtomicBool,
    term: std::cell::UnsafeCell<libc::termios>,
}

// The setter writes `term` before storing `ready`. The handler is installed
// only after that store, and it reads the saved termios static.
unsafe impl Sync for SavedEcho {}

static SAVED_ECHO: SavedEcho = SavedEcho {
    ready: AtomicBool::new(false),
    term: std::cell::UnsafeCell::new(unsafe { std::mem::zeroed() }),
};

fn publish_saved_termios(term: libc::termios) {
    // SAFETY: the handler does not read `term` until `ready` is stored below.
    unsafe { *SAVED_ECHO.term.get() = term };
    SAVED_ECHO.ready.store(true, Ordering::Release);
}

/// Put the terminal back the way it was before echo was cleared.
fn restore_terminal_echo() {
    if !SAVED_ECHO.ready.swap(false, Ordering::AcqRel) {
        return;
    }
    // SAFETY: `ready` was true, so `term` was published before this swap.
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &*SAVED_ECHO.term.get()) };
}

extern "C" fn restore_echo_and_reraise(signal: libc::c_int) {
    // POSIX lists `tcsetattr` as async-signal-safe. This handler only calls
    // `tcsetattr`, `signal`, and `raise`, and it reads the saved termios
    // static. It does not allocate, take a lock, log, or format.
    // SAFETY: `term` was published before this handler was installed.
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &*SAVED_ECHO.term.get());
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// SIGINT and SIGTERM handlers for the duration of the echo-off read.
struct EchoSignals {
    previous_int: libc::sigaction,
    previous_term: libc::sigaction,
}

impl EchoSignals {
    fn install(_echo: &EchoOff) -> Option<Self> {
        // SAFETY: both actions are zeroed sigaction values this function owns.
        // The handler only calls tcsetattr, signal, and raise.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = restore_echo_and_reraise as *const () as usize;
            let mut previous_int = std::mem::zeroed();
            let mut previous_term = std::mem::zeroed();
            if libc::sigaction(libc::SIGINT, &action, &mut previous_int) != 0 {
                return None;
            }
            if libc::sigaction(libc::SIGTERM, &action, &mut previous_term) != 0 {
                libc::sigaction(libc::SIGINT, &previous_int, std::ptr::null_mut());
                return None;
            }
            Some(Self {
                previous_int,
                previous_term,
            })
        }
    }
}

impl Drop for EchoSignals {
    fn drop(&mut self) {
        // SAFETY: these are the dispositions captured when the handlers were installed.
        unsafe {
            libc::sigaction(libc::SIGINT, &self.previous_int, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &self.previous_term, std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn pulse() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("memory")).unwrap();
        dir
    }

    fn mock_endpoint() -> (String, std::sync::Arc<std::sync::Mutex<String>>) {
        let (addr, sql, _) = mock_server(b"OK");
        (addr, sql)
    }

    fn mock_server(
        ack: &'static [u8],
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<String>>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let accepts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slot = std::sync::Arc::clone(&recorded);
        let count = std::sync::Arc::clone(&accepts);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut stream = stream.expect("accept");
                let _ = std::io::Write::write_all(&mut stream, b"MOCK\n");
                let mut sql = String::new();
                let _ = std::io::Read::read_to_string(&mut stream, &mut sql);
                *slot.lock().unwrap() = sql;
                let _ = std::io::Write::write_all(&mut stream, ack);
            }
        });
        (addr, recorded, accepts)
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn files_contain(root: &Path, needle: &str) -> bool {
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                if std::fs::read_to_string(&path)
                    .ok()
                    .is_some_and(|text| text.contains(needle))
                {
                    return true;
                }
            }
        }
        false
    }

    #[tokio::test]
    async fn provision_reads_root_password_from_stdin_only() {
        if !crate::process_state::isolated_test("provision_reads_root_password_from_stdin_only") {
            return;
        }
        let root = pulse();
        let (addr, sql) = mock_endpoint();
        let root_password = "stdin-root-secret";
        std::env::set_var("SURREAL_PASS", "env-secret-not-used");
        let mut reader = Cursor::new(format!("{root_password}\n"));
        provision_with(root.path(), "echo", &addr, &mut reader, false)
            .await
            .unwrap();

        let password_path = root.path().join("secrets/graph-password");
        let stored = std::fs::read_to_string(&password_path).unwrap();
        assert_ne!(stored, root_password);
        assert_eq!(mode(&password_path), 0o600);
        assert_eq!(
            std::fs::metadata(&password_path).unwrap().uid(),
            current_euid()
        );
        let toml = std::fs::read_to_string(root.path().join("memory/.recall-echo.toml")).unwrap();
        assert!(toml.contains("mode = \"server\""), "{toml}");
        assert!(toml.contains(&format!("url = \"{addr}\"")), "{toml}");
        assert!(toml.contains("database = \"echo\""), "{toml}");
        assert!(
            toml.contains("password_file = \"secrets/graph-password\""),
            "{toml}"
        );
        assert!(!toml.contains(root_password), "{toml}");
        assert!(!files_contain(root.path(), root_password));
        assert!(!files_contain(root.path(), "env-secret-not-used"));
        let recorded = sql.lock().unwrap().clone();
        assert!(
            recorded.contains("DEFINE NAMESPACE IF NOT EXISTS nullarc"),
            "{recorded}"
        );
        assert!(
            recorded.contains("DEFINE USER IF NOT EXISTS echo"),
            "{recorded}"
        );
        assert!(recorded.contains(&stored), "{recorded}");
        assert!(!recorded.contains(root_password), "{recorded}");
        assert!(!recorded.contains("env-secret-not-used"), "{recorded}");
        assert!(
            !std::env::args_os().any(|arg| arg.to_string_lossy().contains(root_password)),
            "the root password is an argument"
        );
    }

    #[tokio::test]
    async fn provision_is_idempotent() {
        let root = pulse();
        let (addr, _) = mock_endpoint();
        let mut reader = Cursor::new("root-secret\n");
        assert_eq!(
            provision_with(root.path(), "echo", &addr, &mut reader, false)
                .await
                .unwrap(),
            ProvisionOutcome::Created
        );
        let password = std::fs::read_to_string(root.path().join("secrets/graph-password")).unwrap();
        let toml = std::fs::read_to_string(root.path().join("memory/.recall-echo.toml")).unwrap();
        let mut reader = Cursor::new("another-root-secret\n");
        assert_eq!(
            provision_with(root.path(), "echo", &addr, &mut reader, false)
                .await
                .unwrap(),
            ProvisionOutcome::Already
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("secrets/graph-password")).unwrap(),
            password
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("memory/.recall-echo.toml")).unwrap(),
            toml
        );
    }

    #[test]
    fn provision_refuses_empty_password() {
        let root = pulse();
        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut reader = Cursor::new("\n");
                provision_with(root.path(), "echo", "127.0.0.1:1", &mut reader, false)
                    .await
                    .unwrap_err()
            });
        let exit = error.downcast_ref::<ProvisionExit>().unwrap();
        assert_eq!(exit.code, 2);
        assert!(exit.to_string().contains("empty"), "{exit}");
        assert!(!root.path().join("secrets").exists());
        assert!(!root.path().join("memory/.recall-echo.toml").exists());
    }

    #[tokio::test]
    async fn provision_refuses_symlinked_secrets_dir() {
        let root = pulse();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("secrets")).unwrap();
        let (addr, _) = mock_endpoint();
        let mut reader = Cursor::new("root-secret\n");
        let error = provision_with(root.path(), "echo", &addr, &mut reader, false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
        assert!(std::fs::read_dir(elsewhere.path())
            .unwrap()
            .next()
            .is_none());
        assert!(!root.path().join("memory/.recall-echo.toml").exists());
    }

    #[tokio::test]
    async fn provision_writes_secret_mode_0600_and_dir_0700() {
        let root = pulse();
        let (addr, _) = mock_endpoint();
        let mut reader = Cursor::new("root-secret\n");
        provision_with(root.path(), "echo", &addr, &mut reader, false)
            .await
            .unwrap();
        assert_eq!(mode(&root.path().join("secrets")), 0o700);
        assert_eq!(mode(&root.path().join("secrets/graph-password")), 0o600);
        let password = std::fs::read_to_string(root.path().join("secrets/graph-password")).unwrap();
        assert!(!password.is_empty());
        assert!(!password.contains("root-secret"));
    }

    #[test]
    fn provision_never_logs_the_root_password() {
        let root = pulse();
        let (addr, _) = mock_endpoint();
        let root_password = "root-secret-must-not-log";
        let log = warnings_and_info(async {
            let mut reader = Cursor::new(format!("{root_password}\n"));
            provision_with(root.path(), "echo", &addr, &mut reader, false)
                .await
                .unwrap();
        });
        assert!(!log.contains(root_password), "{log}");
        assert!(!files_contain(root.path(), root_password));
    }

    fn current_euid() -> u32 {
        // SAFETY: geteuid takes no pointer and cannot fail.
        unsafe { libc::geteuid() }
    }

    fn warnings_and_info(fut: impl std::future::Future<Output = ()>) -> String {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let sink = std::sync::Arc::clone(&buf);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(move || LogSink(std::sync::Arc::clone(&sink)))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(fut);
        });
        let guard = buf.lock().unwrap();
        String::from_utf8(guard.clone()).unwrap()
    }

    struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn loopback_endpoint_accepts_local_hosts() {
        for raw in [
            "127.0.0.1:1",
            "127.0.0.1:8787",
            "127.0.0.1:65535",
            "localhost:80",
            "[::1]:9",
        ] {
            assert!(surrealdb_manager::require_loopback(raw).is_ok(), "{raw}");
        }
    }

    #[test]
    fn localhost_is_dialed_as_ipv4_loopback() {
        assert_eq!(
            surrealdb_manager::dial_addr("localhost:9").unwrap(),
            "127.0.0.1:9"
        );
        assert_eq!(
            surrealdb_manager::dial_addr("127.0.0.1:8787").unwrap(),
            "127.0.0.1:8787"
        );
        assert_eq!(surrealdb_manager::dial_addr("[::1]:9").unwrap(), "[::1]:9");
        assert!(surrealdb_manager::require_loopback("localhost:9").is_ok());
    }

    #[tokio::test]
    async fn loopback_endpoint_rejects_remote_and_decorated_urls() {
        let long = format!("203.0.113.1:443/{}", "x".repeat(80));
        for raw in [
            "203.0.113.1:443",
            "example.com:80",
            "a@b:1",
            "ws://127.0.0.1:8787",
            "127.0.0.1:8787/db",
            "127.0.0.1:9\n[evil]",
            long.as_str(),
        ] {
            let err = surrealdb_manager::require_loopback(raw).unwrap_err();
            assert!(err.contains("127.0.0.1"), "{err}");
            assert!(err.contains("localhost"), "{err}");
            assert!(err.contains("[::1]"), "{err}");
            assert!(!err.contains('\n'), "{err}");
            if raw.chars().count() > 24 {
                assert!(!err.contains(raw), "{err}");
            }
        }
        let (addr, sql, accepts) = mock_server(b"OK");
        for raw in [
            format!("{addr}/db"),
            format!("ws://{addr}"),
            format!("user@{addr}"),
            format!("{addr}\n"),
        ] {
            let root = pulse();
            let mut reader = Cursor::new("stdin-root-secret\n");
            let failed = provision_with(root.path(), "echo", &raw, &mut reader, false)
                .await
                .unwrap_err();
            assert!(failed.to_string().contains("127.0.0.1"), "{failed}");
            assert!(
                !failed.to_string().contains("stdin-root-secret"),
                "{failed}"
            );
            assert!(!failed.to_string().contains('\n'), "{failed}");
            assert!(!root.path().join("secrets").exists());
        }
        assert_eq!(accepts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(sql.lock().unwrap().is_empty());
    }

    #[test]
    fn a_non_loopback_configured_endpoint_is_refused() {
        let mut config = crate::config::test_support::minimal_config();
        config.graph.test_addr = Some("203.0.113.9:443".into());
        let err = endpoint_for(&config, None).unwrap_err();
        assert!(err.contains("configured graph endpoint"), "{err}");
        assert!(err.contains("127.0.0.1"), "{err}");
    }

    #[tokio::test]
    async fn rotate_replaces_the_password_and_the_sql_has_it_once() {
        let root = pulse();
        let (addr, _) = mock_endpoint();
        let mut reader = Cursor::new("root-secret\n");
        provision_with(root.path(), "echo", &addr, &mut reader, false)
            .await
            .unwrap();
        let old = std::fs::read(root.path().join("secrets/graph-password")).unwrap();
        let (addr, sql, _) = mock_server(b"OK");
        let mut reader = Cursor::new("root-secret\n");
        provision_with(root.path(), "echo", &addr, &mut reader, true)
            .await
            .unwrap();
        let new = std::fs::read(root.path().join("secrets/graph-password")).unwrap();
        assert_ne!(new, old);
        let recorded = sql.lock().unwrap().clone();
        let password = String::from_utf8(new.clone()).unwrap();
        assert_eq!(recorded.matches(&password).count(), 1, "{recorded}");
        assert!(
            recorded.contains("DEFINE USER echo ON DATABASE"),
            "{recorded}"
        );
        assert!(
            !recorded.contains("DEFINE USER IF NOT EXISTS"),
            "{recorded}"
        );
        assert!(!root.path().join("secrets/graph-password.new").exists());
        assert!(!files_contain(root.path(), "root-secret"));
    }

    #[tokio::test]
    async fn rotate_define_failure_leaves_the_old_file() {
        let root = pulse();
        let (addr, _) = mock_endpoint();
        let mut reader = Cursor::new("root-secret\n");
        provision_with(root.path(), "echo", &addr, &mut reader, false)
            .await
            .unwrap();
        let old = std::fs::read(root.path().join("secrets/graph-password")).unwrap();
        let toml = std::fs::read(root.path().join("memory/.recall-echo.toml")).unwrap();
        let (addr, sql, _) = mock_server(b"NO");
        let mut reader = Cursor::new("root-secret\n");
        let err = provision_with(root.path(), "echo", &addr, &mut reader, true)
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("root-secret"), "{err}");
        assert_eq!(
            std::fs::read(root.path().join("secrets/graph-password")).unwrap(),
            old
        );
        assert!(!root.path().join("secrets/graph-password.new").exists());
        assert_eq!(
            std::fs::read(root.path().join("memory/.recall-echo.toml")).unwrap(),
            toml
        );
        assert!(!sql.lock().unwrap().contains("root-secret"));
    }

    #[tokio::test]
    async fn rotate_after_a_crash_reuses_the_side_file() {
        let root = pulse();
        let (addr, _) = mock_endpoint();
        let mut reader = Cursor::new("root-secret\n");
        provision_with(root.path(), "echo", &addr, &mut reader, false)
            .await
            .unwrap();
        let old = std::fs::read(root.path().join("secrets/graph-password")).unwrap();
        let (addr, first_sql, _) = mock_server(b"OK");
        let err = surrealdb_manager::provision_pulse_crash_after_define(
            &addr,
            "root-secret",
            "echo",
            root.path(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("before the password file"),
            "{err}"
        );
        assert_eq!(
            std::fs::read(root.path().join("secrets/graph-password")).unwrap(),
            old
        );
        let side = std::fs::read(root.path().join("secrets/graph-password.new")).unwrap();
        let side_text = String::from_utf8(side.clone()).unwrap();
        assert_eq!(first_sql.lock().unwrap().matches(&side_text).count(), 1);
        let (addr, second_sql, _) = mock_server(b"OK");
        let mut reader = Cursor::new("other-root\n");
        provision_with(root.path(), "echo", &addr, &mut reader, true)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("secrets/graph-password")).unwrap(),
            side
        );
        assert!(!root.path().join("secrets/graph-password.new").exists());
        assert_eq!(second_sql.lock().unwrap().matches(&side_text).count(), 1);
    }

    #[tokio::test]
    async fn plain_provision_refuses_a_pending_rotation() {
        let root = pulse();
        let (addr, _) = mock_endpoint();
        let mut reader = Cursor::new("root-secret\n");
        provision_with(root.path(), "echo", &addr, &mut reader, false)
            .await
            .unwrap();
        let old = std::fs::read(root.path().join("secrets/graph-password")).unwrap();
        let (addr, _, _) = mock_server(b"OK");
        surrealdb_manager::provision_pulse_crash_after_define(
            &addr,
            "root-secret",
            "echo",
            root.path(),
        )
        .await
        .unwrap_err();
        let side = std::fs::read(root.path().join("secrets/graph-password.new")).unwrap();
        let (addr, sql, accepts) = mock_server(b"OK");
        let mut reader = Cursor::new("root-secret\n");
        let err = provision_with(root.path(), "echo", &addr, &mut reader, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pending"), "{err}");
        assert!(err.to_string().contains("--rotate"), "{err}");
        assert_eq!(
            std::fs::read(root.path().join("secrets/graph-password")).unwrap(),
            old
        );
        assert_eq!(
            std::fs::read(root.path().join("secrets/graph-password.new")).unwrap(),
            side
        );
        assert_eq!(accepts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(sql.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn provision_refuses_an_empty_graph_password() {
        refuse_bad_password(b"", 0o600, "empty").await;
    }

    #[tokio::test]
    async fn provision_refuses_a_world_readable_graph_password() {
        refuse_bad_password(b"pulse-secret", 0o644, "chmod 600 secrets/graph-password").await;
    }

    #[tokio::test]
    async fn provision_refuses_a_symlink_password_file() {
        let root = pulse();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("secrets")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("secret"),
            root.path().join("secrets/graph-password"),
        )
        .unwrap();
        let (addr, sql, _) = mock_server(b"OK");
        let mut reader = Cursor::new("root-secret\n");
        let err = provision_with(root.path(), "echo", &addr, &mut reader, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
        assert!(!elsewhere.path().join("secret").exists());
        assert!(sql.lock().unwrap().is_empty());
        assert!(!root.path().join("memory/.recall-echo.toml").exists());
    }

    async fn refuse_bad_password(bytes: &[u8], mode_bits: u32, needle: &str) {
        let root = pulse();
        let path = root.path().join("secrets/graph-password");
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode_bits)).unwrap();
        let (addr, sql, _) = mock_server(b"OK");
        let mut reader = Cursor::new("root-secret\n");
        let err = provision_with(root.path(), "echo", &addr, &mut reader, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains(needle), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(sql.lock().unwrap().is_empty());
        assert!(!root.path().join("memory/.recall-echo.toml").exists());
    }

    #[tokio::test]
    async fn run_provisions_against_the_test_address() {
        let root = pulse();
        let (addr, sql, _) = mock_server(b"OK");
        let shown = addr.clone();
        let mut config = crate::config::test_support::minimal_config();
        config.pulse.name = "echo".into();
        config.graph.test_addr = Some(addr);
        let mut reader = Cursor::new("stdin-root-secret\n");
        run_in(&config, root.path(), None, false, &mut reader)
            .await
            .unwrap();
        let password = std::fs::read_to_string(root.path().join("secrets/graph-password")).unwrap();
        assert_eq!(mode(&root.path().join("secrets/graph-password")), 0o600);
        assert_eq!(mode(&root.path().join("secrets")), 0o700);
        let recorded = sql.lock().unwrap().clone();
        assert!(
            recorded.contains("DEFINE USER IF NOT EXISTS echo"),
            "{recorded}"
        );
        assert!(recorded.contains(&password), "{recorded}");
        assert!(!recorded.contains("stdin-root-secret"), "{recorded}");
        assert!(!files_contain(root.path(), "stdin-root-secret"));
        let toml = std::fs::read_to_string(root.path().join("memory/.recall-echo.toml")).unwrap();
        assert!(toml.contains(&format!("url = \"{shown}\"")), "{toml}");
    }

    #[test]
    fn restore_terminal_echo_clears_saved_state() {
        // No pty helper is available in the dev-dependencies, so this checks
        // the restore function rather than a child under a terminal.
        publish_saved_termios(unsafe { std::mem::zeroed() });
        assert!(SAVED_ECHO.ready.load(std::sync::atomic::Ordering::Acquire));
        restore_terminal_echo();
        assert!(!SAVED_ECHO.ready.load(std::sync::atomic::Ordering::Acquire));
        restore_terminal_echo();
        assert!(!SAVED_ECHO.ready.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn provision_modes_ignore_umask() {
        if !crate::process_state::isolated_test("provision_modes_ignore_umask") {
            return;
        }
        // SAFETY: umask is process-global. This body runs in the isolated child.
        unsafe { libc::umask(0) };
        let root = pulse();
        let (addr, _) = mock_endpoint();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut reader = Cursor::new("root-secret\n");
            provision_with(root.path(), "echo", &addr, &mut reader, false)
                .await
                .unwrap();
        });
        assert_eq!(mode(&root.path().join("secrets")), 0o700);
        assert_eq!(mode(&root.path().join("secrets/graph-password")), 0o600);
        assert_eq!(mode(&root.path().join("memory/.recall-echo.toml")), 0o600);
    }
}
