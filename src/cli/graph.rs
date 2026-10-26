//! `pulse-null graph provision`.
//!
//! The SurrealDB root password is one line on stdin. It is not an argument,
//! an environment variable, or a file. The process is made non-dumpable
//! before that line is read.

use std::io::{self, BufRead, IsTerminal};
use std::path::Path;

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
    let endpoint = url.unwrap_or_else(surrealdb_manager::default_addr);
    let _echo = silence_echo();
    provision_with(
        &root,
        &config.pulse.name,
        &endpoint,
        &mut io::stdin().lock(),
        rotate,
    )
    .await
    .map(|_| ())
}

/// The command body, with the password reader and pulse directory supplied.
pub async fn provision_with(
    pulse_dir: &Path,
    pulse_name: &str,
    endpoint: &str,
    reader: &mut impl BufRead,
    rotate: bool,
) -> Result<ProvisionOutcome, Box<dyn std::error::Error>> {
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
        // SAFETY: `saved` is the termios read from stdin before echo was cleared.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.saved) };
    }
}

fn silence_echo() -> Option<EchoOff> {
    EchoOff::arm()
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let slot = std::sync::Arc::clone(&recorded);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.expect("accept");
                let _ = std::io::Write::write_all(&mut stream, b"MOCK\n");
                let mut sql = String::new();
                let _ = std::io::Read::read_to_string(&mut stream, &mut sql);
                *slot.lock().unwrap() = sql;
                let _ = std::io::Write::write_all(&mut stream, b"OK");
            }
        });
        (addr, recorded)
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
}
