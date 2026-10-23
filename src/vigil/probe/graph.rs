//! A per-run copy of the pulse's graph for probe items (PN-144).
//!
//! recall-echo's search writes access counts, so even reading the live
//! graph shifts its ranking. Each probe run therefore starts a throwaway
//! SurrealDB of its own and loads a copy of the pulse's database into it.
//! The copy is made in two units, so the probe run itself never needs the
//! IPv4 loopback (where the live server and the host's nginx listen):
//!
//! ```text
//!  pulse-null-probe-export@<pulse>   (loopback allowed, nothing else)
//!    probe export-graph ── export (root, read only) ──▶ live server 127.0.0.1:<port>
//!      writes <probe-state>/s/exports/<pulse>.surql (0600, probe user)
//!  pulse-null-probe@<pulse>          (127.0.0.0/8 denied; ::1 allowed)
//!    probe run ── takes the export (fresh, consumed) ──┐
//!      surreal start memory ──▶ throwaway [::1]:<random port>, random root
//!                               password, sandboxed: binds only its port,
//!                               connects nowhere, writes only its directory
//!      import, define probe user ──▶ throwaway
//!    probe child (recall-echo) ── probe user ──▶ throwaway only
//!    teardown: the throwaway is killed; nothing lands on the live server
//! ```
//!
//! The live root credential is read only by the export, which sends it only
//! to a loopback address: a pulse-writable config cannot point it at another
//! host, and the live namespace and database must be the ones the runtime
//! provisions for this pulse.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli_provider::sandbox::Sandbox;

use super::scratch::GraphTarget;

/// The namespace the probe database lives in, inside the throwaway server.
pub const PROBE_NAMESPACE: &str = "nullarc_probe";

/// Prefix of the probe database and user names.
const PROBE_DB_PREFIX: &str = "probe_";

/// How long a graph request may go without a byte arriving. An idle
/// timeout, not a total one: a large export or import keeps streaming for
/// as long as it needs, a stalled one fails (audit R2-10).
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long connecting to a graph server may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the throwaway server may take to answer its health check.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Ports tried before the throwaway server gives up. A port is accepted
/// only while the child holds its listening socket; a foreign listener is
/// refused and the next attempt picks another port.
const START_ATTEMPTS: u32 = 3;

/// The throwaway server's directory inside the scratch; it is its cwd and
/// its only writable place.
pub const SERVER_DIR: &str = "graph";

/// The throwaway server's pid, for the stale sweep of a crashed run.
pub const SERVER_PID_FILE: &str = "surreal.pid";

/// Why the scratch graph could not be set up or removed.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("graph server request failed: {0}")]
    Http(String),
    #[error("graph server refused the statement: {0}")]
    Statement(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    BadName(String),
    #[error("the pulse's recall-echo [graph] config is refused for probes: {0}")]
    Unsafe(String),
    #[error("the throwaway graph server did not start: {0}")]
    Server(String),
}

/// One SurrealDB HTTP endpoint and where its root credential comes from.
#[derive(Clone)]
struct Endpoint {
    /// `host:port`.
    addr: String,
    root_password: RootPassword,
}

/// The root password, read only when a request needs it, so a run that is
/// refused never touches the live credential.
#[derive(Clone)]
enum RootPassword {
    /// The runtime's shared data dir (`<data_dir>/surrealdb/root-password`).
    DataDir(PathBuf),
    /// A password the runner generated (the throwaway server's).
    Given(String),
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("addr", &self.addr)
            .field("root_password", &"<redacted>")
            .finish()
    }
}

/// The live database a run copies, from the pulse's recall-echo config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveGraph {
    /// `host:port` on a loopback address.
    pub addr: String,
    pub namespace: String,
    pub database: String,
}

impl LiveGraph {
    /// The live graph a pulse's recall-echo config names, when it is in
    /// server mode: `Ok(None)` for an embedded graph (or none configured).
    /// Refused unless the address is a loopback one (the runner sends the
    /// live root credential there) and the namespace and database are the
    /// ones the runtime provisions for `pulse`.
    pub fn from_recall_echo_config(text: &str, pulse: &str) -> Result<Option<Self>, GraphError> {
        let doc: toml::Table = toml::from_str(text).map_err(|e| {
            GraphError::Unsafe(format!(
                "[graph]: {}",
                crate::plugins::config_error::describe(&e)
            ))
        })?;
        let Some(graph) = doc.get("graph").and_then(toml::Value::as_table) else {
            return Ok(None);
        };
        if graph.get("mode").and_then(toml::Value::as_str) != Some("server") {
            return Ok(None);
        }
        let field = |key: &str| {
            graph
                .get(key)
                .and_then(toml::Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| GraphError::Unsafe(format!("[graph] {key} is missing")))
        };
        let addr = loopback_addr(field("url")?)?;
        let namespace = field("namespace")?;
        if namespace != crate::surrealdb_manager::NAMESPACE {
            return Err(GraphError::Unsafe(
                "[graph] namespace: expected the runtime namespace".into(),
            ));
        }
        let database = field("database")?;
        if !database.eq_ignore_ascii_case(pulse)
            || !database
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(GraphError::Unsafe(
                "[graph] database: expected this pulse's name".into(),
            ));
        }
        Ok(Some(Self {
            addr,
            namespace: namespace.to_string(),
            database: database.to_string(),
        }))
    }

    /// The server's port, from `host:port`.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.addr.rsplit_once(':')?.1.parse().ok()
    }
}

/// `url` as `host:port` when its host is a loopback address; an optional
/// `ws://` or `http://` is dropped. Anything else is refused.
fn loopback_addr(url: &str) -> Result<String, GraphError> {
    let bare = ["ws://", "http://"]
        .iter()
        .find_map(|scheme| url.strip_prefix(scheme))
        .unwrap_or(url)
        .trim_end_matches('/');
    let refused = || GraphError::Unsafe("[graph] url: expected a loopback host:port".into());
    let (host, port) = bare.rsplit_once(':').ok_or_else(refused)?;
    port.parse::<u16>().map_err(|_| refused())?;
    let loopback = host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if loopback {
        Ok(bare.to_string())
    } else {
        Err(refused())
    }
}

/// The live server, as the runner sees it: it may only export from it.
#[derive(Debug, Clone)]
pub struct LiveSource {
    endpoint: Endpoint,
}

impl LiveSource {
    /// The live server of `live`, with the root password in the runtime's
    /// shared data dir (`<data_dir>/surrealdb/root-password`), read on use.
    #[must_use]
    pub fn from_data_dir(live: &LiveGraph, data_dir: &Path) -> Self {
        Self {
            endpoint: Endpoint {
                addr: live.addr.clone(),
                root_password: RootPassword::DataDir(data_dir.to_path_buf()),
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn with_password(addr: &str, password: &str) -> Self {
        Self {
            endpoint: Endpoint {
                addr: addr.to_string(),
                root_password: RootPassword::Given(password.to_string()),
            },
        }
    }
}

/// Everything a run needs to copy the pulse's graph: where the export unit
/// left the live graph's export, and the `surreal` binary for the
/// throwaway. A run never reaches the live server itself.
#[derive(Debug, Clone)]
pub struct GraphSource {
    /// The export the export unit writes ([`export_path`]).
    pub export: PathBuf,
    pub surreal_bin: PathBuf,
}

impl GraphSource {
    /// Load the pulse's graph for the run of `scratch` onto a throwaway
    /// server, from a fresh export (which the run consumes).
    pub async fn copy_for(
        &self,
        pulse: &str,
        scratch: &super::scratch::Scratch,
    ) -> Result<ScratchGraph, GraphError> {
        let (export, stats) = take_export(&self.export, EXPORT_MAX_AGE, scratch.dir())?;
        let started = std::time::Instant::now();
        let mut copy = ScratchGraph::from_export(
            &export,
            &CloneSpec {
                pulse,
                run_id: scratch.id(),
                dir: scratch.dir(),
                surreal_bin: &self.surreal_bin,
                read_roots: &super::fence::system_reads(),
            },
        )
        .await?;
        copy.stats = LoadStats {
            load_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..stats
        };
        Ok(copy)
    }
}

/// How old an export may be when a run takes it: the export unit runs just
/// before the probe unit, which may wait up to 90 minutes for the host lock.
pub const EXPORT_MAX_AGE: Duration = Duration::from_secs(3 * 3600);

/// Where the export unit leaves `pulse`'s graph export, beneath the scratch
/// parent: `<scratch parent>/exports/<pulse>.surql`.
#[must_use]
pub fn export_path(scratch_parent: &Path, pulse: &str) -> PathBuf {
    scratch_parent
        .join("exports")
        .join(format!("{}.surql", pulse.to_ascii_lowercase()))
}

/// Export `live` from its server to `dest` (0600, its directory 0700),
/// through a temporary file renamed into place, so a run never takes half
/// an export. Run by the export unit, the only probe step that reaches the
/// live server.
pub async fn export_live(
    source: &LiveSource,
    live: &LiveGraph,
    dest: &Path,
) -> Result<ExportStats, GraphError> {
    let started = std::time::Instant::now();
    let dir = dest
        .parent()
        .ok_or_else(|| GraphError::Unsafe("an export path needs a directory".into()))?;
    create_private_dir(dir)?;
    let partial = dest.with_extension("surql.partial");
    let _ = std::fs::remove_file(&partial);
    let exported = source
        .endpoint
        .export(&live.namespace, &live.database, &partial)
        .await;
    if let Err(e) = exported {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    let bytes = std::fs::metadata(&partial)?.len();
    std::fs::rename(&partial, dest)?;
    Ok(ExportStats {
        bytes,
        elapsed: started.elapsed(),
    })
}

/// What an export took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportStats {
    pub bytes: u64,
    pub elapsed: Duration,
}

/// What loading a run's graph took, for its finish row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct LoadStats {
    /// The export's size.
    pub export_bytes: u64,
    /// How old the export was when the run took it.
    pub export_age_s: u64,
    /// How long the throwaway took to start and the import to finish.
    pub load_ms: u64,
}

/// Move the export at `path` into `into` for one run, if it is a regular
/// file younger than `max_age`. A run consumes its export, so a stale one
/// is never imported twice.
fn take_export(
    path: &Path,
    max_age: Duration,
    into: &Path,
) -> Result<(PathBuf, LoadStats), GraphError> {
    let missing = || {
        GraphError::Unsafe(format!(
            "no fresh graph export at {} (the export unit, pulse-null-probe-export@<pulse>, \
             writes it just before a run; by hand: pulse-null probe export-graph)",
            path.display()
        ))
    };
    let metadata = std::fs::symlink_metadata(path).map_err(|_| missing())?;
    let age = metadata
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .ok_or_else(missing)?;
    if !metadata.is_file() || age > max_age {
        return Err(missing());
    }
    let taken = into.join("graph-export.surql");
    std::fs::rename(path, &taken)?;
    Ok((
        taken,
        LoadStats {
            export_bytes: metadata.len(),
            export_age_s: age.as_secs(),
            load_ms: 0,
        },
    ))
}

/// A probe run's database and user names inside the throwaway server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeNames {
    pub database: String,
    pub username: String,
}

/// `probe_<pulse>_<run>` and `probe_<run>`. The pulse name is the same plain
/// name config accepts (letters, digits, `-` or `_`). A run id stays
/// `[a-z0-9_]`. The database name is quoted where it is interpolated,
/// because a hyphen is not an unquoted identifier.
pub fn probe_names(pulse: &str, run_id: &str) -> Result<ProbeNames, GraphError> {
    let pulse = pulse.to_ascii_lowercase();
    if !crate::config::validate::is_plain_pulse_name(&pulse) {
        return Err(GraphError::BadName(format!(
            "'{pulse}' is not a plain pulse name (letters, digits, - or _)"
        )));
    }
    if run_id.is_empty() || !is_identifier(run_id) {
        return Err(GraphError::BadName(format!(
            "'{run_id}' cannot be used in a probe database name (letters, digits and _ only)"
        )));
    }
    Ok(ProbeNames {
        database: format!("{PROBE_DB_PREFIX}{pulse}_{run_id}"),
        username: format!("{PROBE_DB_PREFIX}{run_id}"),
    })
}

/// Backtick-quote a name whose characters are already a plain pulse name.
/// The charset has no backtick, so the quotes cannot be closed early.
fn quote_ident(name: &str) -> Result<String, GraphError> {
    if !crate::config::validate::is_plain_pulse_name(name) {
        return Err(GraphError::Unsafe(
            "probe database name is not a plain identifier".into(),
        ));
    }
    Ok(format!("`{name}`"))
}

fn is_identifier(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The statements that create the probe database and its user.
///
/// The password is interpolated. It is refused unless every character is
/// in the alphanumeric alphabet [`crate::surrealdb_manager::generate_password`]
/// emits, so a quote or statement break cannot land in the text.
fn define_sql(names: &ProbeNames, password: &str) -> Result<String, GraphError> {
    if password.is_empty() || !password.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(GraphError::Unsafe(
            "probe database password is not alphanumeric".into(),
        ));
    }
    let db = quote_ident(&names.database)?;
    let user = quote_ident(&names.username)?;
    Ok(format!(
        "DEFINE NAMESPACE IF NOT EXISTS {PROBE_NAMESPACE};\n\
         USE NS {PROBE_NAMESPACE};\n\
         DEFINE DATABASE {db};\n\
         USE DB {db};\n\
         DEFINE USER {user} ON DATABASE PASSWORD '{password}' ROLES OWNER;",
    ))
}

/// What the live export leaves out: users (their password hashes) and
/// access methods (their signing keys).
const EXPORT_OPTIONS: &str = r#"{"users":false,"accesses":false}"#;

impl Endpoint {
    fn password(&self) -> Result<String, GraphError> {
        match &self.root_password {
            RootPassword::DataDir(dir) => crate::surrealdb_manager::read_root_password(dir)
                .map_err(|e| GraphError::Io(std::io::Error::other(e.to_string()))),
            RootPassword::Given(password) => Ok(password.clone()),
        }
    }

    fn client(&self) -> Result<reqwest::Client, GraphError> {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(IDLE_TIMEOUT)
            .build()
            .map_err(|e| GraphError::Http(e.to_string()))
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}/{path}", self.addr)
    }

    fn request(
        &self,
        client: &reqwest::Client,
        path: &str,
        scope: Option<(&str, &str)>,
    ) -> Result<reqwest::RequestBuilder, GraphError> {
        let mut request = client
            .post(self.url(path))
            .basic_auth("root", Some(self.password()?));
        if let Some((namespace, database)) = scope {
            request = request
                .header("surreal-ns", namespace)
                .header("surreal-db", database);
        }
        Ok(request)
    }

    /// Run statements as root; every one must succeed. Returns the results.
    async fn sql(&self, statements: &str) -> Result<Vec<serde_json::Value>, GraphError> {
        let client = self.client()?;
        let response = self
            .request(&client, "sql", None)?
            .header("Accept", "application/json")
            .body(statements.to_string())
            .send()
            .await
            .map_err(|e| GraphError::Http(e.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| GraphError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(GraphError::Statement(bounded(&body)));
        }
        let results: Vec<serde_json::Value> =
            serde_json::from_str(&body).map_err(|_| GraphError::Statement(bounded(&body)))?;
        if let Some(failed) = results.iter().find(|r| r["status"] != "OK") {
            return Err(GraphError::Statement(bounded(
                &failed["result"].to_string(),
            )));
        }
        Ok(results.into_iter().map(|r| r["result"].clone()).collect())
    }

    /// Export `namespace`/`database` (without users and access methods) to
    /// `path`, which is created 0600 before anything is written to it.
    async fn export(&self, namespace: &str, database: &str, path: &Path) -> Result<(), GraphError> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        let response = self.export_response(namespace, database).await?;
        write_stream(response, file).await
    }

    /// The server's answer to an export request, refused unless it is a
    /// success.
    async fn export_response(
        &self,
        namespace: &str,
        database: &str,
    ) -> Result<reqwest::Response, GraphError> {
        let client = self.client()?;
        let response = self
            .request(&client, "export", Some((namespace, database)))?
            .header("Accept", "application/octet-stream")
            .header("Content-Type", "application/json")
            .body(EXPORT_OPTIONS)
            .send()
            .await
            .map_err(|e| GraphError::Http(e.to_string()))?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(GraphError::Statement(bounded(&body)));
        }
        Ok(response)
    }

    /// Import the export at `path` into `namespace`/`database`.
    async fn import(&self, namespace: &str, database: &str, path: &Path) -> Result<(), GraphError> {
        let body = tokio::fs::File::open(path).await?;
        let client = self.client()?;
        let response = self
            .request(&client, "import", Some((namespace, database)))?
            .header("Accept", "application/json")
            .body(reqwest::Body::from(body))
            .send()
            .await
            .map_err(|e| GraphError::Http(e.to_string()))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(GraphError::Statement(bounded(&body)));
        }
        // A successful import answers with the statements that failed, if any.
        if let Ok(results) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
            if let Some(failed) = results.iter().find(|r| r["status"] != "OK") {
                return Err(GraphError::Statement(bounded(
                    &failed["result"].to_string(),
                )));
            }
        }
        Ok(())
    }

    /// Does the server answer its health check?
    async fn healthy(&self) -> bool {
        let Ok(client) = self.client() else {
            return false;
        };
        client
            .get(self.url("health"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }
}

/// Server text is bounded before it reaches a log or an error.
fn bounded(text: &str) -> String {
    crate::utils::safe_truncate(text.trim(), 300).to_string()
}

/// A SurrealDB of one run's own: in memory, on a random port of the IPv6
/// loopback, with
/// a random root password, inside a Landlock sandbox. Killed when stopped or
/// dropped.
#[derive(Debug)]
pub struct ThrowawayServer {
    child: tokio::sync::Mutex<tokio::process::Child>,
    endpoint: Endpoint,
    port: u16,
}

/// Write `response`'s body into `file` as it streams, then sync it.
async fn write_stream(
    mut response: reqwest::Response,
    mut file: std::fs::File,
) -> Result<(), GraphError> {
    use std::io::Write;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| GraphError::Http(e.to_string()))?
    {
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    Ok(())
}

/// How the throwaway server is started.
#[derive(Debug, Clone, Copy)]
pub struct ServerSpec<'a> {
    /// The `surreal` binary.
    pub bin: &'a Path,
    /// Its directory (created 0700): cwd, temp dir and only writable place.
    pub dir: &'a Path,
    /// What it may read (the system part of the probe read fence and the
    /// binary's own directory).
    pub read_roots: &'a [PathBuf],
}

/// `surreal start` in memory, no network or scripting, bound to `[::1]:port`
/// as root with `password`, logging into `surreal.log` in its directory.
fn server_command(
    spec: &ServerSpec<'_>,
    port: u16,
    password: &str,
) -> Result<tokio::process::Command, GraphError> {
    let log = std::fs::File::create(spec.dir.join("surreal.log"))?;
    let mut cmd = tokio::process::Command::new(spec.bin);
    cmd.args(["start", "--deny-net", "--deny-scripting", "memory"])
        .current_dir(spec.dir)
        .env_clear()
        .env("SURREAL_USER", "root")
        .env("SURREAL_PASS", password)
        .env("SURREAL_BIND", format!("[::1]:{port}"))
        .env("SURREAL_NO_BANNER", "true")
        .env("SURREAL_LOG", "warn")
        .env("TMPDIR", spec.dir)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true);
    Ok(cmd)
}

/// The server's sandbox: it writes only its own directory (and the device
/// files), reads the given roots and its binary's directory, connects
/// nowhere, and binds only `port`.
fn server_sandbox(
    spec: &ServerSpec<'_>,
    port: u16,
) -> Result<crate::cli_provider::sandbox::PreparedSandbox, GraphError> {
    let mut write_roots = vec![spec.dir.to_path_buf()];
    write_roots.extend(
        super::fence::DEVICE_WRITES
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists()),
    );
    let mut read_roots = spec.read_roots.to_vec();
    if let Some(dir) = spec
        .bin
        .canonicalize()
        .ok()
        .and_then(|b| b.parent().map(Path::to_path_buf))
    {
        read_roots.push(dir);
    }
    Sandbox {
        read_roots,
        write_roots,
        tcp_connect: Vec::new(),
        tcp_bind: vec![port],
        background: true,
    }
    .prepare()
    .map_err(|e| GraphError::Server(e.to_string()))
}

/// The throwaway dies with the runner, even one killed outright (a by-hand
/// run's SIGKILL, a crash): the kernel sends SIGKILL when the spawning
/// thread exits (a Tokio worker, alive for the runtime's life). A parent
/// already gone before the setting took fails the spawn (audit R2-13).
fn die_with_runner(cmd: &mut tokio::process::Command) {
    // SAFETY: getpid has no preconditions.
    let parent = unsafe { libc::getpid() };
    // SAFETY: the hook runs in the forked child before exec and makes only
    // two raw syscalls on integers.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::other("the runner is gone"));
            }
            Ok(())
        });
    }
}

impl ThrowawayServer {
    /// Start the server and wait until it answers its health check.
    pub async fn start(spec: &ServerSpec<'_>) -> Result<Self, GraphError> {
        create_private_dir(spec.dir)?;
        let mut last_error = String::new();
        for _ in 0..START_ATTEMPTS {
            match Self::try_start(spec).await {
                Ok(server) => return Ok(server),
                Err(e) => last_error = e.to_string(),
            }
        }
        Err(GraphError::Server(last_error))
    }

    async fn try_start(spec: &ServerSpec<'_>) -> Result<Self, GraphError> {
        let port = free_loopback_port()?;
        let password = crate::surrealdb_manager::generate_password();
        let mut cmd = server_command(spec, port, &password)?;
        let sandbox = server_sandbox(spec, port)?;
        die_with_runner(&mut cmd);
        let child = sandbox.spawn(cmd).await?;
        if let Some(pid) = child.id() {
            std::fs::write(spec.dir.join(SERVER_PID_FILE), pid.to_string())?;
        }
        let server = Self {
            child: tokio::sync::Mutex::new(child),
            endpoint: Endpoint {
                addr: format!("[::1]:{port}"),
                root_password: RootPassword::Given(password),
            },
            port,
        };
        server.answering(spec.dir).await
    }

    /// `Some(status)` once the child has exited, `None` while it runs.
    async fn child_exit(&self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.lock().await.try_wait()
    }

    /// Whether the child process holds the listening socket for this port.
    /// A health check is not identity: another local account can bind the
    /// port in the gap after it was chosen and answer `/health`.
    async fn owns_port(&self) -> bool {
        self.child
            .lock()
            .await
            .id()
            .is_some_and(|pid| port_owned_by(pid, self.port))
    }

    /// The started server once the child holds the port and answers. An
    /// error (and the server stopped) when the child exits first, or when
    /// that does not happen within [`STARTUP_TIMEOUT`]. Health is asked
    /// only after `try_wait` shows the child still running and only when
    /// the listening socket is one of the child's fds.
    async fn answering(mut self, dir: &Path) -> Result<Self, GraphError> {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if let Some(status) = self.child_exit().await? {
                let log = std::fs::read_to_string(dir.join("surreal.log")).unwrap_or_default();
                return Err(GraphError::Server(format!(
                    "exited {status}: {}",
                    bounded(&log)
                )));
            }
            if self.owns_port().await
                && self.endpoint.healthy().await
                && self.owns_port().await
                && self.child_exit().await?.is_none()
            {
                return Ok(self);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.stop().await;
        Err(GraphError::Server(format!(
            "no answer on [::1]:{} within {}s",
            self.port,
            STARTUP_TIMEOUT.as_secs()
        )))
    }

    #[must_use]
    pub fn addr(&self) -> &str {
        &self.endpoint.addr
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Kill the server and reap it.
    pub async fn stop(&mut self) {
        let _ = self.child.lock().await.kill().await;
    }
}

/// `[::1]` as `/proc/net/tcp6` prints an IPv6 address (four little-endian words).
const LOOPBACK_V6: &str = "00000000000000000000000001000000";

/// `TCP_LISTEN` in `/proc/net/tcp6`.
const TCP_LISTEN: &str = "0A";

/// Whether `pid` holds a listening socket on `[::1]:port`.
///
/// `/proc/<pid>/net/tcp6` is the network namespace, so a foreign listener
/// on the same port is listed there too. It counts only when one of that
/// process's own file descriptors is the socket's inode. Unreadable proc
/// entries are not ownership.
fn port_owned_by(pid: u32, port: u16) -> bool {
    let Ok(table) = std::fs::read_to_string(format!("/proc/{pid}/net/tcp6")) else {
        return false;
    };
    parse_listen_inodes(&table, port)
        .iter()
        .any(|inode| process_has_socket(pid, *inode))
}

/// Inodes of `[::1]:port` listeners in a `/proc/net/tcp6` table.
fn parse_listen_inodes(table: &str, port: u16) -> Vec<u64> {
    let want = format!("{LOOPBACK_V6}:{port:04X}");
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _slot = fields.next()?;
            let local = fields.next()?;
            let _remote = fields.next()?;
            let state = fields.next()?;
            if !local.eq_ignore_ascii_case(&want) || !state.eq_ignore_ascii_case(TCP_LISTEN) {
                return None;
            }
            // tx:rx, tr:when, retrnsmt, uid, timeout, then the inode.
            fields.nth(5)?.parse().ok()
        })
        .collect()
}

/// Whether `pid` has a file descriptor for socket `inode`.
fn process_has_socket(pid: u32, inode: u64) -> bool {
    let want = format!("socket:[{inode}]");
    let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    dir.flatten().any(|entry| {
        std::fs::read_link(entry.path())
            .ok()
            .is_some_and(|path| path.to_string_lossy() == want)
    })
}

/// A port on the IPv6 loopback that was free a moment ago. The socket is
/// dropped so the child can bind it; the child is accepted only once that
/// listening socket is one of its own fds ([`port_owned_by`]). The throwaway
/// listens on `::1`, which the probe unit allows, never on 127.0.0.1, which
/// it denies (the live graph server and the host's nginx listen there).
fn free_loopback_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind("[::1]:0")?.local_addr()?.port())
}

/// Kill the throwaway server a crashed run left in `scratch_dir`, if it is
/// still running: its recorded pid, and only if that process's working
/// directory is still that run's server directory (also when the kernel
/// reports it as deleted). The process is pinned with a pidfd before it is
/// checked and signalled through the pidfd, so a pid reused after the check
/// is never signalled (audit R2-12).
pub fn kill_stale_server(scratch_dir: &Path) {
    let server_dir = scratch_dir.join(SERVER_DIR);
    let Some(pid) = std::fs::read_to_string(server_dir.join(SERVER_PID_FILE))
        .ok()
        .and_then(|t| t.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 1)
    else {
        return;
    };
    // SAFETY: pidfd_open takes a pid and flags and returns a descriptor or -1.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let Ok(raw) = i32::try_from(raw) else {
        return;
    };
    if raw < 0 {
        return;
    }
    // SAFETY: `raw` is a pidfd this function just opened.
    let pidfd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
    if !cwd.is_some_and(|cwd| is_server_dir(&cwd, &server_dir)) {
        return;
    }
    // SAFETY: a signal through the pidfd pinned above: it reaches that
    // process or none.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            std::os::fd::AsRawFd::as_raw_fd(&pidfd),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if sent == 0 {
        tracing::info!("killed stale throwaway graph server (pid {pid})");
    }
}

/// Is `cwd` (as `/proc/<pid>/cwd` reads) the server directory `dir`, even
/// one the kernel marks ` (deleted)`?
fn is_server_dir(cwd: &Path, dir: &Path) -> bool {
    let expected = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let deleted = PathBuf::from(format!("{} (deleted)", expected.display()));
    cwd == expected || cwd == deleted
}

/// One run's copy of the pulse's graph, on its throwaway server.
#[derive(Debug)]
pub struct ScratchGraph {
    server: ThrowawayServer,
    names: ProbeNames,
    password_file: PathBuf,
    /// What loading it took.
    pub stats: LoadStats,
}

/// What a graph copy is made for.
#[derive(Debug, Clone, Copy)]
pub struct CloneSpec<'a> {
    pub pulse: &'a str,
    pub run_id: &'a str,
    /// The run's scratch directory (0700): the probe user's password is
    /// written there; the server lives in `<dir>/graph`.
    pub dir: &'a Path,
    pub surreal_bin: &'a Path,
    pub read_roots: &'a [PathBuf],
}

impl ScratchGraph {
    /// Start a throwaway server and load the export at `export` into it,
    /// with a probe user whose password is written to `<dir>/graph-password`
    /// (0600). The export is deleted once loaded; a load that fails half-way
    /// stops the server.
    pub async fn from_export(export: &Path, spec: &CloneSpec<'_>) -> Result<Self, GraphError> {
        let names = probe_names(spec.pulse, spec.run_id)?;
        let mut server = ThrowawayServer::start(&ServerSpec {
            bin: spec.surreal_bin,
            dir: &spec.dir.join(SERVER_DIR),
            read_roots: spec.read_roots,
        })
        .await?;
        let password = crate::surrealdb_manager::generate_password();
        let password_file = spec.dir.join(super::scratch::GRAPH_PASSWORD_FILE);
        let loaded = async {
            if server.child_exit().await?.is_some() || !server.owns_port().await {
                return Err(GraphError::Server(
                    "the throwaway port is not held by the server".into(),
                ));
            }
            write_private(&password_file, &password)?;
            server
                .endpoint
                .sql(&define_sql(&names, &password)?)
                .await
                .map_err(|e| redact(e, &password))?;
            server
                .endpoint
                .import(PROBE_NAMESPACE, &names.database, export)
                .await
        }
        .await;
        let _ = std::fs::remove_file(export);
        if let Err(e) = loaded {
            server.stop().await;
            return Err(e);
        }
        Ok(Self {
            server,
            names,
            password_file,
            stats: LoadStats::default(),
        })
    }

    /// Does the throwaway still answer, and is the listener still the child?
    /// Checked before every turn that uses it: a server that died (out of
    /// memory, say) would turn every later memory lookup into an error the
    /// pulse is scored on (audit R2-10). A foreign listener on the port is
    /// not the server.
    pub async fn alive(&self) -> bool {
        if self.server.child_exit().await.ok().flatten().is_some() || !self.server.owns_port().await
        {
            return false;
        }
        self.server.endpoint.healthy().await
            && self.server.owns_port().await
            && self.server.child_exit().await.ok().flatten().is_none()
    }

    /// What a probe item's recall-echo config should point at.
    pub fn target(&self) -> GraphTarget {
        GraphTarget::Server {
            addr: self.server.addr().to_string(),
            namespace: PROBE_NAMESPACE.to_string(),
            database: self.names.database.clone(),
            username: self.names.username.clone(),
            password_file: self.password_file.clone(),
        }
    }

    /// The throwaway server's port: the one graph port a probe child may
    /// connect to.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.server.port()
    }

    #[cfg(test)]
    pub fn database(&self) -> &str {
        &self.names.database
    }

    /// Stop the throwaway server; the copy goes with it.
    pub async fn remove(mut self) {
        self.server.stop().await;
    }
}

/// A server error that might quote a statement must not carry the password.
fn redact(error: GraphError, password: &str) -> GraphError {
    match error {
        GraphError::Statement(text) => GraphError::Statement(text.replace(password, "<redacted>")),
        GraphError::Http(text) => GraphError::Http(text.replace(password, "<redacted>")),
        other => other,
    }
}

fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content.as_bytes())
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listen_line_names_its_inode_only_for_that_port() {
        let header = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let listen = "   1: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0\n";
        assert_eq!(
            parse_listen_inodes(&format!("{header}{listen}"), 0x1F90),
            vec![12345]
        );
        assert!(parse_listen_inodes(&format!("{header}{listen}"), 1).is_empty());
        let established = listen.replace(" 0A ", " 01 ");
        assert!(parse_listen_inodes(&format!("{header}{established}"), 0x1F90).is_empty());
        let other_addr = listen.replace(LOOPBACK_V6, "00000000000000000000000000000000");
        assert!(parse_listen_inodes(&format!("{header}{other_addr}"), 0x1F90).is_empty());
    }

    /// A listener in this process is not a child that does not hold the fd.
    #[test]
    fn a_foreign_listener_on_the_throwaway_port_is_refused() {
        let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(
            port_owned_by(std::process::id(), port),
            "this process holds the listening socket"
        );
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        assert!(
            !port_owned_by(child.id(), port),
            "the listener is not the child's"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn probe_names_are_plain_identifiers() {
        let names = probe_names("Echo", "3fa2b1c4d5e6").unwrap();
        assert_eq!(names.database, "probe_echo_3fa2b1c4d5e6");
        assert_eq!(names.username, "probe_3fa2b1c4d5e6");
        assert!(probe_names("echo; REMOVE NAMESPACE nullarc", "x").is_err());
        assert!(probe_names("echo", "a-b").is_err());
        assert!(probe_names("", "abc").is_err());
        let hyphen = probe_names("My-Pulse", "abc123").unwrap();
        assert_eq!(hyphen.database, "probe_my-pulse_abc123");
        let define = define_sql(&hyphen, "pw").unwrap();
        assert!(
            define.contains("DEFINE DATABASE `probe_my-pulse_abc123`;"),
            "{define}"
        );
        assert!(
            define.contains("USE DB `probe_my-pulse_abc123`;"),
            "{define}"
        );
        let mut crafted = hyphen;
        crafted.database = "probe`; REMOVE".into();
        let err = define_sql(&crafted, "pw").unwrap_err().to_string();
        assert!(!err.contains("REMOVE"), "{err}");
        assert!(err.contains("plain identifier"), "{err}");
    }

    #[test]
    fn statements_create_in_the_probe_namespace_only() {
        let names = probe_names("synth", "abc123").unwrap();
        let define = define_sql(&names, "pw").unwrap();
        assert!(define.contains("USE NS nullarc_probe;"));
        assert!(define.contains("DEFINE DATABASE `probe_synth_abc123`;"));
        assert!(
            define.contains("DEFINE USER `probe_abc123` ON DATABASE PASSWORD 'pw' ROLES OWNER;")
        );
        assert!(EXPORT_OPTIONS.contains(r#""users":false"#));
        assert!(EXPORT_OPTIONS.contains(r#""accesses":false"#));
    }

    /// A password outside the generator's alphabet is refused before it is
    /// placed in the statement.
    #[test]
    fn a_password_outside_the_alphabet_is_not_interpolated() {
        let names = probe_names("synth", "abc123").unwrap();
        for password in ["pw'", "pw; REMOVE", "pw password", "päss"] {
            let err = define_sql(&names, password).unwrap_err();
            let msg = err.to_string();
            assert!(!msg.contains(password), "{msg}");
            assert!(msg.contains("alphanumeric"), "{msg}");
        }
        assert!(define_sql(&names, "").is_err());
        assert!(define_sql(&names, "Ab3").is_ok());
    }

    fn config(url: &str, namespace: &str, database: &str) -> String {
        format!(
            "[graph]\nmode = \"server\"\nurl = \"{url}\"\nnamespace = \"{namespace}\"\n\
             database = \"{database}\"\nusername = \"echo\"\npassword_file = \"secrets/graph-password\"\n"
        )
    }

    #[test]
    fn the_live_graph_comes_from_recall_echo_config() {
        assert_eq!(
            LiveGraph::from_recall_echo_config(
                &config("127.0.0.1:8787", "nullarc", "Echo"),
                "Echo"
            )
            .unwrap(),
            Some(LiveGraph {
                addr: "127.0.0.1:8787".into(),
                namespace: "nullarc".into(),
                database: "Echo".into(),
            })
        );
        let ws = LiveGraph::from_recall_echo_config(
            &config("ws://localhost:8787", "nullarc", "echo"),
            "Echo",
        )
        .unwrap()
        .unwrap();
        assert_eq!(ws.addr, "localhost:8787");
        assert_eq!(ws.port(), Some(8787));
        assert_eq!(
            LiveGraph::from_recall_echo_config("[graph]\nmode = \"embedded\"\n", "Echo").unwrap(),
            None
        );
        assert_eq!(
            LiveGraph::from_recall_echo_config("", "Echo").unwrap(),
            None
        );
    }

    /// Audit #12: the live root credential goes to loopback only, and only
    /// for this pulse's own database in the runtime's namespace.
    #[test]
    fn a_config_that_points_the_credential_elsewhere_is_refused() {
        for (url, namespace, database) in [
            ("203.0.113.7:8787", "nullarc", "Echo"),
            ("evil.example:8787", "nullarc", "Echo"),
            ("wss://127.0.0.1:8787", "nullarc", "Echo"),
            ("127.0.0.1", "nullarc", "Echo"),
            ("127.0.0.1:8787", "other", "Echo"),
            ("127.0.0.1:8787", "nullarc", "Synth"),
            ("127.0.0.1:8787", "nullarc", "Echo; REMOVE"),
        ] {
            let result =
                LiveGraph::from_recall_echo_config(&config(url, namespace, database), "Echo");
            assert!(
                matches!(result, Err(GraphError::Unsafe(_))),
                "{url} {namespace} {database}: {result:?}"
            );
        }
        assert!(LiveGraph::from_recall_echo_config(
            &config("[::1]:8787", "nullarc", "Echo"),
            "Echo"
        )
        .unwrap()
        .is_some());
    }

    /// A pulse-writable recall-echo config must not be copied into the error.
    #[test]
    fn a_config_error_names_the_key_and_not_the_value() {
        let secret = "super-secret-value";
        let broken = format!("[graph]\nmode = \"{secret}");
        let toml_err = LiveGraph::from_recall_echo_config(&broken, "echo").unwrap_err();
        assert!(!toml_err.to_string().contains(secret), "{toml_err}");
        let url_err = LiveGraph::from_recall_echo_config(
            &config(&format!("https://{secret}:443"), "nullarc", "echo"),
            "echo",
        )
        .unwrap_err();
        let msg = url_err.to_string();
        assert!(!msg.contains(secret), "{msg}");
        assert!(msg.contains("url"), "{msg}");
        assert!(msg.contains("loopback"), "{msg}");
    }

    #[test]
    fn the_root_password_never_prints() {
        let source = LiveSource::with_password("127.0.0.1:1", "s3cr3t-fixture");
        assert!(!format!("{source:?}").contains("s3cr3t-fixture"));
        let err = redact(GraphError::Statement("PASSWORD 'abc' failed".into()), "abc");
        assert!(!err.to_string().contains("'abc'"));
    }

    /// The `surreal` binary, when this host has one.
    fn surreal_bin() -> Option<PathBuf> {
        which::which("surreal")
            .ok()
            .or_else(|| Some(PathBuf::from("/usr/local/bin/surreal")).filter(|p| p.exists()))
    }

    /// A throwaway server playing the live one: namespace `nullarc`,
    /// database `fixture` with two entities, a user and an access method.
    async fn seeded_live(bin: &Path, dir: &Path) -> (ThrowawayServer, LiveSource, LiveGraph) {
        let server = ThrowawayServer::start(&ServerSpec {
            bin,
            dir: &dir.join("graph"),
            read_roots: crate::vigil::probe::scratch::tests::test_read_roots(),
        })
        .await
        .unwrap();
        server
            .endpoint
            .sql(
                "DEFINE NAMESPACE nullarc; USE NS nullarc; DEFINE DATABASE fixture; USE DB fixture; \
                 DEFINE USER fixture ON DATABASE PASSWORD 'fixture' ROLES OWNER; \
                 DEFINE ACCESS fixture_jwt ON DATABASE TYPE JWT ALGORITHM HS512 KEY 'fixture-key'; \
                 CREATE entity:a SET name = 'alpha'; CREATE entity:b SET name = 'beta'; \
                 RELATE entity:a->relates_to->entity:b SET confidence = 0.7;",
            )
            .await
            .unwrap();
        let RootPassword::Given(password) = server.endpoint.root_password.clone() else {
            unreachable!()
        };
        let source = LiveSource::with_password(server.addr(), &password);
        let live = LiveGraph {
            addr: server.addr().to_string(),
            namespace: "nullarc".into(),
            database: "fixture".into(),
        };
        (server, source, live)
    }

    /// What the two units do in turn: the export (live → file), then a
    /// run taking the fresh export and loading it onto its throwaway.
    async fn copied(source: &LiveSource, live: &LiveGraph, spec: &CloneSpec<'_>) -> ScratchGraph {
        let exports = tempfile::tempdir().unwrap();
        let path = export_path(exports.path(), spec.pulse);
        let exported = export_live(source, live, &path).await.unwrap();
        assert_eq!(exported.bytes, std::fs::metadata(&path).unwrap().len());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (taken, stats) = take_export(&path, EXPORT_MAX_AGE, spec.dir).unwrap();
        assert!(stats.export_bytes > 0);
        assert!(!path.exists(), "a run consumes its export");
        ScratchGraph::from_export(&taken, spec).await.unwrap()
    }

    #[test]
    fn a_stale_or_missing_export_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let into = tempfile::tempdir().unwrap();
        let path = export_path(dir.path(), "Echo");
        assert!(path.ends_with("exports/echo.surql"));
        assert!(take_export(&path, EXPORT_MAX_AGE, into.path()).is_err());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "-- export").unwrap();
        assert!(
            take_export(&path, Duration::ZERO, into.path()).is_err(),
            "too old"
        );
        std::os::unix::fs::symlink("/dev/null", dir.path().join("exports/link.surql")).unwrap();
        assert!(take_export(
            &dir.path().join("exports/link.surql"),
            EXPORT_MAX_AGE,
            into.path()
        )
        .is_err());
        assert!(take_export(&path, EXPORT_MAX_AGE, into.path()).is_ok());
    }

    /// R2-3, end to end: a probe child, inside its item's sandbox, reads
    /// its graph password and queries the throwaway copy, as recall-echo in
    /// the child does. The run's own copy of the password stays fenced off.
    #[tokio::test]
    async fn a_sandboxed_child_reads_its_throwaway_graph() {
        use crate::cli_provider::adapters::claude::Claude;
        use crate::vigil::probe::scratch::tests::{plan, Fixture};
        use crate::vigil::probe::scratch::{ItemKind, ItemPlan};
        let Some(bin) = surreal_bin() else {
            eprintln!("SKIPPED: no surreal binary");
            return;
        };
        let python = Path::new("/usr/bin/python3");
        if !python.exists() {
            eprintln!("SKIPPED: no python3");
            return;
        }
        if !crate::cli_provider::sandbox::landlock_or_skip(
            "a_sandboxed_child_reads_its_throwaway_graph",
        ) {
            return;
        }
        let live_dir = tempfile::tempdir().unwrap();
        let (mut live_server, source, live) = seeded_live(&bin, live_dir.path()).await;
        let fixture = Fixture::new();
        let scratch = fixture.scratch(false);
        let copy = copied(
            &source,
            &live,
            &CloneSpec {
                pulse: "fixture",
                run_id: scratch.id(),
                dir: scratch.dir(),
                surreal_bin: &bin,
                read_roots: crate::vigil::probe::scratch::tests::test_read_roots(),
            },
        )
        .await;
        let target = copy.target();
        let ports = [copy.port()];
        let item = scratch
            .item(
                0,
                ItemKind::Pulse,
                &ItemPlan {
                    tcp_connect: &ports,
                    ..plan(&Claude, &target)
                },
            )
            .unwrap();
        // What recall-echo does with its [graph] config: read the password
        // file it names and query the server as that user.
        let script = r#"
import base64, json, sys, tomllib, urllib.request
cfg = tomllib.load(open("memory/.recall-echo.toml", "rb"))["graph"]
pw = open(cfg["password_file"]).read().strip()
req = urllib.request.Request(
    "http://" + cfg["url"] + "/sql",
    data=b"SELECT count() FROM entity GROUP ALL;",
    headers={
        "Accept": "application/json",
        "surreal-ns": cfg["namespace"],
        "surreal-db": cfg["database"],
        "surreal-auth-ns": cfg["namespace"],
        "surreal-auth-db": cfg["database"],
        "Authorization": "Basic " + base64.b64encode((cfg["username"] + ":" + pw).encode()).decode(),
    },
)
print("COUNT", json.load(urllib.request.urlopen(req, timeout=10))[0]["result"][0]["count"])
try:
    open(sys.argv[1]).read()
    print("RUN PASSWORD READ")
except OSError:
    print("RUN PASSWORD FENCED")
"#;
        let mut cmd = tokio::process::Command::new(python);
        cmd.arg("-I")
            .arg("-S")
            .arg("-c")
            .arg(script)
            .arg(
                scratch
                    .dir()
                    .join(super::super::scratch::GRAPH_PASSWORD_FILE),
            )
            .current_dir(item.root())
            .env_clear()
            .envs(item.overrides().env);
        let output = item
            .overrides()
            .sandbox
            .unwrap()
            .prepare()
            .unwrap()
            .output(cmd)
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("COUNT 2"), "{stdout} {stderr}");
        assert!(stdout.contains("RUN PASSWORD FENCED"), "{stdout} {stderr}");
        item.finish();
        copy.remove().await;
        live_server.stop().await;
    }

    /// Against throwaway servers only: one plays the live server, the copy
    /// gets its own. Skips on a host without `surreal` or Landlock.
    #[tokio::test]
    async fn a_runs_graph_is_loaded_from_the_export_it_consumes() {
        let Some(bin) = surreal_bin() else {
            eprintln!("SKIPPED: no surreal binary");
            return;
        };
        if !crate::cli_provider::sandbox::landlock_or_skip(
            "a_runs_graph_is_loaded_from_the_export_it_consumes",
        ) {
            return;
        }
        let live_dir = tempfile::tempdir().unwrap();
        let (mut live_server, source, live) = seeded_live(&bin, live_dir.path()).await;
        let exports = tempfile::tempdir().unwrap();
        let export = export_path(exports.path(), "fixture");
        let exported = export_live(&source, &live, &export).await.unwrap();
        let fixture = crate::vigil::probe::scratch::tests::Fixture::new();
        let scratch = fixture.scratch(false);
        let graph = GraphSource {
            export: export.clone(),
            surreal_bin: bin.clone(),
        };

        let copy = graph.copy_for("fixture", &scratch).await.unwrap();
        assert!(!export.exists(), "the run consumed its export");
        assert_eq!(copy.stats.export_bytes, exported.bytes);
        let rows = copy
            .server
            .endpoint
            .sql(&format!(
                "USE NS {PROBE_NAMESPACE}; USE DB {}; SELECT count() FROM entity GROUP ALL;",
                copy.database()
            ))
            .await
            .unwrap();
        assert_eq!(rows.last().unwrap()[0]["count"].as_u64(), Some(2));
        assert_ne!(copy.port(), live.port().unwrap());

        assert!(
            graph.copy_for("fixture", &scratch).await.is_err(),
            "an export is loaded once; a second run needs a new one"
        );
        copy.remove().await;
        live_server.stop().await;
    }

    #[tokio::test]
    async fn clone_copies_into_a_throwaway_server_and_leaves_the_source_untouched() {
        let Some(bin) = surreal_bin() else {
            eprintln!("SKIPPED: no surreal binary");
            return;
        };
        if !crate::cli_provider::sandbox::landlock_or_skip(
            "clone_copies_into_a_throwaway_server_and_leaves_the_source_untouched",
        ) {
            return;
        }
        let roots = crate::vigil::probe::scratch::tests::test_read_roots();
        let live_dir = tempfile::tempdir().unwrap();
        let (mut live_server, source, live) = seeded_live(&bin, live_dir.path()).await;
        let count = |endpoint: Endpoint, ns: &'static str, db: String| async move {
            let r = endpoint
                .sql(&format!(
                    "USE NS {ns}; USE DB {db}; SELECT count() FROM entity GROUP ALL;"
                ))
                .await
                .unwrap();
            r.last().unwrap()[0]["count"].as_u64().unwrap()
        };

        let scratch = tempfile::tempdir().unwrap();
        let copy = copied(
            &source,
            &live,
            &CloneSpec {
                pulse: "fixture",
                run_id: "abc123def456",
                dir: scratch.path(),
                surreal_bin: &bin,
                read_roots: roots,
            },
        )
        .await;
        assert_ne!(
            copy.port(),
            live.port().unwrap(),
            "the copy has its own server"
        );
        assert_eq!(
            count(
                copy.server.endpoint.clone(),
                PROBE_NAMESPACE,
                copy.database().to_string()
            )
            .await,
            2
        );
        let info = copy
            .server
            .endpoint
            .sql(&format!(
                "USE NS {PROBE_NAMESPACE}; USE DB {}; INFO FOR DB;",
                copy.database()
            ))
            .await
            .unwrap();
        let db_info = info.last().unwrap();
        let users = db_info["users"].as_object().unwrap();
        assert!(users.contains_key("probe_abc123def456"));
        assert!(!users.contains_key("fixture"), "live users are not copied");
        assert!(
            db_info["accesses"].as_object().is_none_or(|a| a.is_empty()),
            "access methods (their keys) are not copied: {db_info}"
        );
        assert!(
            !scratch.path().join("graph-export.surql").exists(),
            "the export is gone once loaded"
        );
        // Writes to the copy never reach the source.
        copy.server
            .endpoint
            .sql(&format!(
                "USE NS {PROBE_NAMESPACE}; USE DB {}; DELETE entity;",
                copy.database()
            ))
            .await
            .unwrap();
        assert_eq!(
            count(live_server.endpoint.clone(), "nullarc", "fixture".into()).await,
            2
        );
        let copy_endpoint = copy.server.endpoint.clone();
        assert!(copy.alive().await);
        copy.remove().await;
        assert!(!copy_endpoint.healthy().await, "the throwaway is gone");
        live_server.stop().await;
    }

    #[test]
    fn a_deleted_server_dir_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join(SERVER_DIR);
        std::fs::create_dir_all(&server).unwrap();
        let canonical = server.canonicalize().unwrap();
        assert!(is_server_dir(&canonical, &server));
        assert!(is_server_dir(
            &PathBuf::from(format!("{} (deleted)", canonical.display())),
            &server
        ));
        assert!(!is_server_dir(Path::new("/"), &server));
    }

    #[tokio::test]
    async fn a_stale_server_is_killed_only_when_its_cwd_matches() {
        let scratch = tempfile::tempdir().unwrap();
        let server_dir = scratch.path().join(SERVER_DIR);
        std::fs::create_dir_all(&server_dir).unwrap();
        let mut sleeper = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .current_dir(&server_dir)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = sleeper.id().unwrap();
        std::fs::write(server_dir.join(SERVER_PID_FILE), pid.to_string()).unwrap();
        // A pid whose process runs elsewhere is left alone.
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(other.path().join(SERVER_DIR)).unwrap();
        std::fs::write(
            other.path().join(SERVER_DIR).join(SERVER_PID_FILE),
            pid.to_string(),
        )
        .unwrap();
        kill_stale_server(other.path());
        assert!(
            sleeper.try_wait().unwrap().is_none(),
            "not this run's server"
        );
        kill_stale_server(scratch.path());
        let status = tokio::time::timeout(Duration::from_secs(5), sleeper.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }
}
