//! Contents API on `heads/<pulse>/<YYYY>-<MM>.log` (UTC).
//!
//! History lists `heads/<pulse>`. A 404 there means no history. Each
//! `YYYY-MM.log` entry is read and kept only when `encoding` is `base64` and
//! the decoded length equals `size`. Anything else is an error and is not
//! published. A month file that would pass 819200 bytes (800 KiB) is not written.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::{DateTime, Datelike, Utc};

use super::{
    format_line, parse_history, parse_line, Anchor, AnchorError, AnchorLine, BoxFuture, Secret,
};

const API_VERSION: &str = "2026-03-10";
const MAX_BODY: usize = 2 * 1024 * 1024;
const MONTH_CAP: usize = 800 * 1024;

pub struct GitHubAnchor {
    owner: String,
    repo: String,
    branch: String,
    dir: String,
    api_base: String,
    token: Secret,
    client: reqwest::Client,
    now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl GitHubAnchor {
    pub(crate) fn new(
        owner: String,
        repo: String,
        branch: String,
        dir: String,
        api_base: String,
        token: Secret,
    ) -> Result<Self, AnchorError> {
        Self::with_clock(
            owner,
            repo,
            branch,
            dir,
            api_base,
            token,
            Arc::new(Utc::now),
        )
    }

    pub(crate) fn with_clock(
        owner: String,
        repo: String,
        branch: String,
        dir: String,
        api_base: String,
        token: Secret,
        now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    ) -> Result<Self, AnchorError> {
        super::config::check_api_base(&api_base)?;
        super::config::check_repo_parts(&owner, &repo)?;
        super::config::check_branch(&branch)?;
        super::config::check_dir(&dir)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| AnchorError::Transport)?;
        Ok(Self {
            owner,
            repo,
            branch,
            dir,
            api_base,
            token,
            client,
            now,
        })
    }

    fn root(&self) -> &str {
        self.dir.trim_matches('/')
    }

    fn pulse_dir(&self, pulse: &str) -> String {
        format!("{}/{pulse}", self.root())
    }

    fn month_path(&self, pulse: &str, when: DateTime<Utc>) -> String {
        format!(
            "{}/{pulse}/{:04}-{:02}.log",
            self.root(),
            when.year(),
            when.month()
        )
    }

    fn url(&self, path: &str) -> String {
        let base = self.api_base.trim_end_matches('/');
        format!(
            "{base}/repos/{}/{}/contents/{}?ref={}",
            encode(&self.owner),
            encode(&self.repo),
            encode_path(path),
            self.encoded_branch()
        )
    }

    fn branch_url(&self) -> String {
        let base = self.api_base.trim_end_matches('/');
        format!(
            "{base}/repos/{}/{}/branches/{}",
            encode(&self.owner),
            encode(&self.repo),
            self.encoded_branch()
        )
    }

    /// One encoding for the branch check and for `?ref=`.
    fn encoded_branch(&self) -> String {
        encode_path(&self.branch)
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> Result<reqwest::RequestBuilder, AnchorError> {
        let mut authorization =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token.expose()))
                .map_err(|_| AnchorError::Transport)?;
        authorization.set_sensitive(true);
        Ok(self
            .client
            .request(method, url)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(
                "User-Agent",
                format!("pulse-null/{}", env!("CARGO_PKG_VERSION")),
            ))
    }

    async fn ensure_branch(&self) -> Result<(), AnchorError> {
        let response = self
            .request(reqwest::Method::GET, &self.branch_url())?
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|_| AnchorError::Transport)?;
        let status = response.status();
        let _body = read_body(response).await?;
        if status.is_success() {
            return Ok(());
        }
        Err(self.status(status))
    }

    async fn list_months(&self, dir: &str) -> Result<Vec<String>, AnchorError> {
        let response = self
            .request(reqwest::Method::GET, &self.url(dir))?
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|_| AnchorError::Transport)?;
        let status = response.status();
        let bytes = read_body(response).await?;
        if status.as_u16() == 404 {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(self.status(status));
        }
        let listed: Vec<Listed> = serde_json::from_slice(&bytes).map_err(|_| AnchorError::Body)?;
        let mut names: Vec<String> = listed
            .into_iter()
            .map(|item| item.name)
            .filter(|name| month_log(name))
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// `Ok(None)` is a missing file. Any other body must be a verified object.
    async fn get_file(&self, path: &str) -> Result<Option<RemoteFile>, AnchorError> {
        let response = self
            .request(reqwest::Method::GET, &self.url(path))?
            .header("Accept", "application/vnd.github.object+json")
            .send()
            .await
            .map_err(|_| AnchorError::Transport)?;
        let status = response.status();
        let bytes = read_body(response).await?;
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(self.status(status));
        }
        let object: Object = serde_json::from_slice(&bytes).map_err(|_| AnchorError::Body)?;
        Ok(Some(verified_file(object)?))
    }

    async fn put_once(
        &self,
        path: &str,
        text: &str,
        sha: Option<&str>,
        message: &str,
    ) -> Result<(), AnchorError> {
        if text.len() > MONTH_CAP {
            return Err(AnchorError::Rotate);
        }
        let mut body = serde_json::json!({
            "message": message,
            "content": STANDARD.encode(text.as_bytes()),
            "branch": self.branch,
        });
        if let Some(sha) = sha {
            if sha.is_empty() {
                return Err(AnchorError::Sha);
            }
            body["sha"] = serde_json::Value::String(sha.to_string());
        }
        let response = self
            .request(reqwest::Method::PUT, &self.url(path))?
            .header("Accept", "application/vnd.github+json")
            .json(&body)
            .send()
            .await
            .map_err(|_| AnchorError::Transport)?;
        let status = response.status();
        let _body = read_body(response).await?;
        if status.is_success() {
            return Ok(());
        }
        Err(self.status(status))
    }

    fn status(&self, status: reqwest::StatusCode) -> AnchorError {
        tracing::warn!(status = status.as_u16(), "anchor request failed");
        AnchorError::Status(status.as_u16())
    }

    async fn publish_path(&self, path: &str, line: &AnchorLine) -> Result<(), AnchorError> {
        let message = format!("{} {}", line.pulse, line.seq);
        let mut retried = false;
        loop {
            let current = self.get_file(path).await?;
            let previous = current.as_ref().map(|file| file.text.as_str());
            let text = append_text(previous, line)?;
            if text.len() > MONTH_CAP {
                return Err(AnchorError::Rotate);
            }
            let sha = current.as_ref().map(|file| file.sha.as_str());
            match self.put_once(path, &text, sha, &message).await {
                Ok(()) => return Ok(()),
                Err(AnchorError::Status(409)) if !retried => {
                    retried = true;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

struct RemoteFile {
    sha: String,
    text: String,
}

fn verified_file(object: Object) -> Result<RemoteFile, AnchorError> {
    if object.encoding.as_deref() != Some("base64") {
        return Err(AnchorError::Encoding);
    }
    let Some(size) = object.size else {
        return Err(AnchorError::Size);
    };
    let raw = object.content.unwrap_or_default();
    let cleaned: String = raw.chars().filter(|ch| !ch.is_whitespace()).collect();
    let bytes = if cleaned.is_empty() {
        Vec::new()
    } else {
        STANDARD.decode(cleaned).map_err(|_| AnchorError::Body)?
    };
    if bytes.len() as u64 != size || bytes.len() > MAX_BODY {
        return Err(if bytes.len() > MAX_BODY {
            AnchorError::TooLarge
        } else {
            AnchorError::Size
        });
    }
    let sha = object.sha.unwrap_or_default();
    if sha.is_empty() {
        return Err(AnchorError::Sha);
    }
    let text = String::from_utf8(bytes).map_err(|_| AnchorError::Body)?;
    Ok(RemoteFile { sha, text })
}

fn month_log(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 11
        && bytes[4] == b'-'
        && bytes[7] == b'.'
        && &bytes[8..] == b"log"
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
}

fn plain_pulse(pulse: &str) -> Result<(), AnchorError> {
    if crate::config::validate::is_plain_pulse_name(pulse) {
        Ok(())
    } else {
        Err(AnchorError::Config("pulse".into()))
    }
}

async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, AnchorError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| AnchorError::Transport)? {
        if body.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(AnchorError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Lines from a GitHub file. A last line with no newline is kept when it
/// parses. A fragment that is not a full line is an error, not a dropped tail.
fn remote_lines(text: &str) -> Result<Vec<AnchorLine>, AnchorError> {
    if text.is_empty() || text.ends_with('\n') {
        return parse_history(text);
    }
    let (head, tail) = match text.rfind('\n') {
        Some(end) => (&text[..=end], &text[end + 1..]),
        None => ("", text),
    };
    let mut lines = parse_history(head)?;
    if !tail.is_empty() {
        lines.push(parse_line(tail)?);
    }
    Ok(lines)
}

fn append_text(previous: Option<&str>, line: &AnchorLine) -> Result<String, AnchorError> {
    let mut text = String::new();
    if let Some(previous) = previous.filter(|text| !text.is_empty()) {
        if !previous.ends_with('\n') {
            remote_lines(previous)?;
        }
        text.push_str(previous);
        if !previous.ends_with('\n') {
            text.push('\n');
        }
    }
    text.push_str(&format_line(line));
    text.push('\n');
    Ok(text)
}

fn encode_path(path: &str) -> String {
    path.split('/').map(encode).collect::<Vec<_>>().join("/")
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(serde::Deserialize)]
struct Object {
    sha: Option<String>,
    content: Option<String>,
    encoding: Option<String>,
    size: Option<u64>,
}

#[derive(serde::Deserialize)]
struct Listed {
    name: String,
}

impl Anchor for GitHubAnchor {
    fn name(&self) -> &'static str {
        "github"
    }

    fn history<'a>(
        &'a self,
        pulse: &'a str,
    ) -> BoxFuture<'a, Result<Vec<AnchorLine>, AnchorError>> {
        Box::pin(async move {
            plain_pulse(pulse)?;
            self.ensure_branch().await?;
            let mut lines = Vec::new();
            for name in self.list_months(&self.pulse_dir(pulse)).await? {
                let path = format!("{}/{name}", self.pulse_dir(pulse));
                match self.get_file(&path).await? {
                    Some(file) => lines.extend(remote_lines(&file.text)?),
                    None => return Err(AnchorError::Status(404)),
                }
            }
            Ok(lines)
        })
    }

    fn publish<'a>(&'a self, line: &'a AnchorLine) -> BoxFuture<'a, Result<(), AnchorError>> {
        Box::pin(async move {
            plain_pulse(&line.pulse)?;
            self.ensure_branch().await?;
            let path = self.month_path(&line.pulse, (self.now)());
            self.publish_path(&path, line).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Anchor, AnchorLine, GitHubAnchor, Secret, API_VERSION};
    use crate::oneself::chain;
    use crate::oneself::event::ReceiptEvent;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::any;
    use axum::Router;

    struct Mock {
        files: Mutex<HashMap<String, (String, String)>>,
        raw: Mutex<HashMap<String, String>>,
        listings: Mutex<HashMap<String, Vec<String>>>,
        paths: Mutex<Vec<String>>,
        uris: Mutex<Vec<String>>,
        puts: Mutex<Vec<serde_json::Value>>,
        gets: AtomicUsize,
        put_status: Mutex<Vec<u16>>,
        get_status: Mutex<Vec<u16>>,
        headers: Mutex<Vec<(String, String, String)>>,
        stale_once: AtomicBool,
        missing: AtomicBool,
        sha_n: AtomicUsize,
    }

    impl Mock {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
                raw: Mutex::new(HashMap::new()),
                listings: Mutex::new(HashMap::new()),
                paths: Mutex::new(Vec::new()),
                uris: Mutex::new(Vec::new()),
                puts: Mutex::new(Vec::new()),
                gets: AtomicUsize::new(0),
                put_status: Mutex::new(Vec::new()),
                get_status: Mutex::new(Vec::new()),
                headers: Mutex::new(Vec::new()),
                stale_once: AtomicBool::new(false),
                missing: AtomicBool::new(false),
                sha_n: AtomicUsize::new(1),
            }
        }
    }

    async fn handle(
        State(mock): State<Arc<Mock>>,
        headers: HeaderMap,
        request: axum::extract::Request,
    ) -> (StatusCode, String) {
        let bearer = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let version = headers
            .get("x-github-api-version")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let agent = headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        mock.headers.lock().unwrap().push((bearer, version, agent));
        let path = request.uri().path().to_string();
        mock.uris.lock().unwrap().push(request.uri().to_string());
        mock.paths.lock().unwrap().push(path.clone());
        let key = path.split("/contents/").nth(1).unwrap_or("").to_string();
        match request.method().as_str() {
            "GET" => {
                mock.gets.fetch_add(1, Ordering::Relaxed);
                if mock.missing.load(Ordering::Relaxed) {
                    return (StatusCode::NOT_FOUND, "missing".into());
                }
                if let Some(status) = mock.get_status.lock().unwrap().pop() {
                    return (StatusCode::from_u16(status).unwrap(), "no".into());
                }
                if path.contains("/branches/") {
                    return (StatusCode::OK, "{\"name\":\"main\"}".into());
                }
                if let Some(body) = mock.raw.lock().unwrap().get(&key).cloned() {
                    return (StatusCode::OK, body);
                }
                let file = mock.files.lock().unwrap().get(&key).cloned();
                if let Some((sha, text)) = file {
                    let body = serde_json::json!({
                        "sha": sha,
                        "content": STANDARD.encode(text.as_bytes()),
                        "encoding": "base64",
                        "size": text.len(),
                    });
                    return (StatusCode::OK, body.to_string());
                }
                if let Some(names) = mock.listings.lock().unwrap().get(&key).cloned() {
                    return (StatusCode::OK, listed(&names));
                }
                let names = {
                    let files = mock.files.lock().unwrap();
                    let prefix = format!("{key}/");
                    let mut names: Vec<String> = files
                        .keys()
                        .filter_map(|item| {
                            item.strip_prefix(&prefix)
                                .filter(|rest| !rest.contains('/'))
                                .map(str::to_string)
                        })
                        .collect();
                    names.sort();
                    names
                };
                if names.is_empty() {
                    (StatusCode::NOT_FOUND, "missing".into())
                } else {
                    (StatusCode::OK, listed(&names))
                }
            }
            "PUT" => {
                if let Some(status) = mock.put_status.lock().unwrap().pop() {
                    return (StatusCode::from_u16(status).unwrap(), "no".into());
                }
                let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                mock.puts.lock().unwrap().push(body.clone());
                if mock.stale_once.swap(false, Ordering::Relaxed) {
                    let sha = format!("sha{}", mock.sha_n.fetch_add(1, Ordering::Relaxed));
                    let mut files = mock.files.lock().unwrap();
                    if let Some(file) = files.get_mut(&key) {
                        file.0 = sha;
                    }
                    return (StatusCode::CONFLICT, "stale".into());
                }
                let sha = body.get("sha").and_then(|value| value.as_str());
                let content = body
                    .get("content")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let text = String::from_utf8(STANDARD.decode(content).unwrap()).unwrap();
                let mut files = mock.files.lock().unwrap();
                if let Some(file) = files.get_mut(&key) {
                    if sha != Some(file.0.as_str()) {
                        return (StatusCode::CONFLICT, "sha".into());
                    }
                    let next = format!("sha{}", mock.sha_n.fetch_add(1, Ordering::Relaxed));
                    *file = (next, text);
                } else if sha.is_some() {
                    return (StatusCode::NOT_FOUND, "missing".into());
                } else {
                    let next = format!("sha{}", mock.sha_n.fetch_add(1, Ordering::Relaxed));
                    files.insert(key, (next, text));
                }
                (StatusCode::CREATED, "{}".into())
            }
            _ => (StatusCode::METHOD_NOT_ALLOWED, String::new()),
        }
    }

    fn listed(names: &[String]) -> String {
        serde_json::json!(names
            .iter()
            .map(|name| serde_json::json!({"name": name}))
            .collect::<Vec<_>>())
        .to_string()
    }

    async fn serve(mock: Arc<Mock>) -> String {
        let app = Router::new()
            .route("/{*rest}", any(handle))
            .with_state(mock);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://127.0.0.1:{port}")
    }

    fn line(seq: u64, head: &str) -> AnchorLine {
        AnchorLine {
            pulse: "echo".into(),
            ledger_id: "0123456789abcdef".into(),
            seq,
            head: head.into(),
            ts: "2026-10-25T00:00:00.000Z".into(),
        }
    }

    fn github_at(base: String, year: i32) -> GitHubAnchor {
        GitHubAnchor::with_clock(
            "owner".into(),
            "repo".into(),
            "main".into(),
            "heads".into(),
            base,
            Secret::new("pn108-token-do-not-leak".into()),
            Arc::new(move || Utc.with_ymd_and_hms(year, 6, 1, 0, 0, 0).unwrap()),
        )
        .unwrap()
    }

    fn echo_index() -> chain::ChainIndex {
        let bytes = super::super::receipt_bytes(&[
            ReceiptEvent::Genesis {
                pulse: "echo".into(),
            },
            ReceiptEvent::Shutdown {
                reason: "stop".into(),
            },
        ]);
        chain::index_bytes(&bytes)
    }

    #[tokio::test]
    async fn create_append_conflict_and_headers() {
        let mock = Arc::new(Mock::new());
        let base = serve(mock.clone()).await;
        let anchor = github_at(base, 2026);
        anchor.publish(&line(1, &"ab".repeat(32))).await.unwrap();
        let stored = mock
            .files
            .lock()
            .unwrap()
            .get("heads/echo/2026-06.log")
            .unwrap()
            .1
            .clone();
        assert!(stored.contains("v1 echo 0123456789abcdef 1 "), "{stored}");
        let (no_sha, message) = {
            let puts = mock.puts.lock().unwrap();
            (
                puts[0].get("sha").is_none(),
                puts[0]["message"].as_str().unwrap_or("").to_string(),
            )
        };
        assert!(no_sha, "create has no sha");
        assert_eq!(message, "echo 1");

        mock.stale_once.store(true, Ordering::Relaxed);
        anchor.publish(&line(2, &"cd".repeat(32))).await.unwrap();
        let puts = mock.puts.lock().unwrap().len();
        assert_eq!(puts, 3, "create, stale update, retry");
        let text = mock
            .files
            .lock()
            .unwrap()
            .get("heads/echo/2026-06.log")
            .unwrap()
            .1
            .clone();
        assert_eq!(text.lines().count(), 2, "{text}");
        let headers = mock.headers.lock().unwrap();
        assert!(headers.iter().all(|(bearer, version, agent)| {
            bearer == "Bearer pn108-token-do-not-leak"
                && version == API_VERSION
                && agent.starts_with("pulse-null/")
        }));
    }

    #[tokio::test]
    async fn statuses_carry_the_code_and_not_the_token() {
        for status in [401_u16, 403, 422, 500, 404] {
            let mock = Arc::new(Mock::new());
            if status == 404 {
                mock.put_status.lock().unwrap().push(404);
                mock.files.lock().unwrap().insert(
                    "heads/echo/2026-06.log".into(),
                    ("sha1".into(), String::new()),
                );
            } else {
                mock.get_status.lock().unwrap().push(status);
            }
            let base = serve(mock).await;
            let anchor = github_at(base, 2026);
            let err = anchor
                .publish(&line(1, &"ab".repeat(32)))
                .await
                .unwrap_err();
            let rendered = format!("{err} {err:?}");
            assert!(rendered.contains(&status.to_string()), "{rendered}");
            assert!(!rendered.contains("pn108-token-do-not-leak"), "{rendered}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_traced_failure_does_not_record_the_token() {
        let mock = Arc::new(Mock::new());
        mock.get_status.lock().unwrap().push(401);
        let base = serve(mock).await;
        let anchor = github_at(base, 2026);
        let buf = std::sync::Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || {
                struct Mem(std::sync::Arc<Mutex<Vec<u8>>>);
                impl std::io::Write for Mem {
                    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().unwrap().extend_from_slice(data);
                        Ok(data.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                Mem(writer.clone())
            })
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let err = anchor.history("echo").await.unwrap_err();
        drop(guard);
        let rendered = format!("{err} {err:?}");
        let traced = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(!rendered.contains("pn108-token-do-not-leak"), "{rendered}");
        assert!(!traced.contains("pn108-token-do-not-leak"), "{traced}");
    }

    #[tokio::test]
    async fn a_month_rollover_reads_both_months() {
        let mock = Arc::new(Mock::new());
        let base = serve(mock.clone()).await;
        let month = Arc::new(std::sync::atomic::AtomicU32::new(6));
        let clock = month.clone();
        let anchor = GitHubAnchor::with_clock(
            "owner".into(),
            "repo".into(),
            "main".into(),
            "heads".into(),
            base,
            Secret::new("pn108-token-do-not-leak".into()),
            Arc::new(move || {
                let month = clock.load(Ordering::Relaxed);
                Utc.with_ymd_and_hms(2026, month, 1, 0, 0, 0).unwrap()
            }),
        )
        .unwrap();
        anchor.publish(&line(1, &"ab".repeat(32))).await.unwrap();
        month.store(7, Ordering::Relaxed);
        anchor.publish(&line(2, &"cd".repeat(32))).await.unwrap();
        let history = anchor.history("echo").await.unwrap();
        assert_eq!(history.len(), 2, "{history:?}");
        assert_eq!(history[0].seq, 1);
        assert_eq!(history[1].seq, 2);
        let files = mock.files.lock().unwrap();
        assert!(files.contains_key("heads/echo/2026-06.log"));
        assert!(files.contains_key("heads/echo/2026-07.log"));
    }

    #[tokio::test]
    async fn a_year_rollover_reads_both_files() {
        let mock = Arc::new(Mock::new());
        let base = serve(mock.clone()).await;
        let year = Arc::new(std::sync::atomic::AtomicI32::new(2026));
        let clock = year.clone();
        let anchor = GitHubAnchor::with_clock(
            "owner".into(),
            "repo".into(),
            "main".into(),
            "heads".into(),
            base,
            Secret::new("pn108-token-do-not-leak".into()),
            Arc::new(move || {
                let year = clock.load(Ordering::Relaxed);
                Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap()
            }),
        )
        .unwrap();
        anchor.publish(&line(1, &"ab".repeat(32))).await.unwrap();
        year.store(2027, Ordering::Relaxed);
        anchor.publish(&line(2, &"cd".repeat(32))).await.unwrap();
        let history = anchor.history("echo").await.unwrap();
        assert_eq!(history.len(), 2, "{history:?}");
        assert_eq!(history[0].seq, 1);
        assert_eq!(history[1].seq, 2);
        let files = mock.files.lock().unwrap();
        assert!(files.contains_key("heads/echo/2026-01.log"));
        assert!(files.contains_key("heads/echo/2027-01.log"));
    }

    #[tokio::test]
    async fn encoding_none_and_a_size_mismatch_publish_nothing() {
        let head = "ab".repeat(32);
        let text = format!("v1 echo 0123456789abcdef 1 {head} 2026-10-25T00:00:00.000Z\n");
        let mock = Arc::new(Mock::new());
        mock.files.lock().unwrap().insert(
            "heads/echo/2026-06.log".into(),
            ("bigsha".into(), text.clone()),
        );
        mock.raw.lock().unwrap().insert(
            "heads/echo/2026-06.log".into(),
            serde_json::json!({
                "sha": "bigsha",
                "content": "",
                "encoding": "none",
                "size": 1_100_000,
            })
            .to_string(),
        );
        let base = serve(mock.clone()).await;
        let anchor = github_at(base, 2026);
        let err = anchor.history("echo").await.unwrap_err();
        assert!(matches!(err, super::AnchorError::Encoding), "{err}");
        let err = anchor.publish(&line(2, &head)).await.unwrap_err();
        assert!(matches!(err, super::AnchorError::Encoding), "{err}");
        assert!(mock.puts.lock().unwrap().is_empty());

        let mock = Arc::new(Mock::new());
        mock.raw.lock().unwrap().insert(
            "heads/echo/2026-06.log".into(),
            serde_json::json!({
                "sha": "abc",
                "content": STANDARD.encode(b"hello"),
                "encoding": "base64",
                "size": 999,
            })
            .to_string(),
        );
        let base = serve(mock.clone()).await;
        let anchor = github_at(base, 2026);
        let err = anchor.publish(&line(2, &head)).await.unwrap_err();
        assert!(matches!(err, super::AnchorError::Size), "{err}");
        assert!(err.to_string().contains("size"), "{err}");
        assert!(mock.puts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_hostile_directory_listing_reads_only_month_files() {
        let head = "ab".repeat(32);
        let text = format!("v1 echo 0123456789abcdef 1 {head} 2026-10-25T00:00:00.000Z\n");
        let mock = Arc::new(Mock::new());
        mock.files
            .lock()
            .unwrap()
            .insert("heads/echo/2026-06.log".into(), ("sha1".into(), text));
        mock.listings.lock().unwrap().insert(
            "heads/echo".into(),
            vec![
                "../x.log".into(),
                "note.txt".into(),
                "2026.log".into(),
                "2026-06.log.bak".into(),
                "2026-06.log".into(),
            ],
        );
        let base = serve(mock.clone()).await;
        let anchor = github_at(base, 2026);
        let history = anchor.history("echo").await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].seq, 1);
        let paths = mock.paths.lock().unwrap().clone();
        let contents: Vec<_> = paths
            .iter()
            .filter(|path| path.contains("/contents/"))
            .cloned()
            .collect();
        assert!(
            contents.iter().any(|path| path.ends_with("/heads/echo")),
            "{contents:?}"
        );
        assert!(
            contents
                .iter()
                .any(|path| path.ends_with("/heads/echo/2026-06.log")),
            "{contents:?}"
        );
        assert!(
            contents.iter().all(|path| {
                !path.contains("x.log") && !path.contains("note.txt") && !path.contains(".bak")
            }),
            "{contents:?}"
        );
        assert!(mock.puts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_branch_is_unverified_and_an_empty_history_is_none() {
        let index = echo_index();
        let mock = Arc::new(Mock::new());
        mock.missing.store(true, Ordering::Relaxed);
        let base = serve(mock.clone()).await;
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(github_at(base, 2026))];
        let out = super::super::check(&index, &backends).await.unwrap();
        assert_eq!(out.state, "unverified");
        assert_eq!(out.notes[0].state, "unverified");
        assert!(
            out.notes[0].detail.as_deref().unwrap_or("").contains("404"),
            "{out:?}"
        );
        assert_ne!(out.notes[0].state, "ok");
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "unverified");
        assert!(mock.puts.lock().unwrap().is_empty());

        let mock = Arc::new(Mock::new());
        let base = serve(mock.clone()).await;
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(github_at(base, 2026))];
        let out = super::super::check(&index, &backends).await.unwrap();
        assert_eq!(out.notes[0].state, "none");
        assert_ne!(out.notes[0].state, "ok");
        assert!(mock.puts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_month_file_over_800kb_is_not_extended() {
        let mock = Arc::new(Mock::new());
        let mut body = vec![b'x'; 800 * 1024];
        *body.last_mut().unwrap() = b'\n';
        let text = String::from_utf8(body).unwrap();
        mock.files
            .lock()
            .unwrap()
            .insert("heads/echo/2026-06.log".into(), ("sha1".into(), text));
        let base = serve(mock.clone()).await;
        let anchor = github_at(base, 2026);
        let err = anchor
            .publish(&line(1, &"ab".repeat(32)))
            .await
            .unwrap_err();
        assert!(matches!(err, super::AnchorError::Rotate), "{err}");
        assert!(err.to_string().contains("819200"), "{err}");
        assert!(err.to_string().contains("800 KiB"), "{err}");
        assert!(mock.puts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_bad_target_sends_nothing() {
        let mock = Arc::new(Mock::new());
        let base = serve(mock.clone()).await;
        let bad = [
            ("..", "repo", "main", "heads"),
            ("owner", "..", "main", "heads"),
            (".", "repo", "main", "heads"),
            ("owner", "repo", "-main", "heads"),
            ("owner", "repo", "a/../b", "heads"),
            ("owner", "repo", "x/./y", "heads"),
            ("owner", "repo", "x//y", "heads"),
            ("owner", "repo", "./x", "heads"),
            ("owner", "repo", "x/.", "heads"),
            ("owner", "repo", "x/", "heads"),
            ("owner", "repo", "main.lock", "heads"),
            ("owner", "repo", "feature/foo.lock", "heads"),
            ("owner", "repo", "main", "/heads"),
            ("owner", "repo", "main", "heads/../x"),
            ("owner", "repo", "main", "."),
        ];
        for (owner, repo, branch, dir) in bad {
            let built = GitHubAnchor::with_clock(
                owner.into(),
                repo.into(),
                branch.into(),
                dir.into(),
                base.clone(),
                Secret::new("pn108-token-do-not-leak".into()),
                Arc::new(Utc::now),
            );
            let Err(err) = built else {
                panic!("expected a config error for {owner}/{repo}");
            };
            assert!(
                err.to_string().contains("anchor config"),
                "{owner} {repo} {branch} {dir}: {err}"
            );
        }
        let anchor = github_at(base, 2026);
        for pulse in ["../x", "a/b", "\u{1b}esc", "\u{202e}bidi", "", ".."] {
            let err = anchor.history(pulse).await.unwrap_err();
            assert!(err.to_string().contains("pulse"), "{pulse:?}: {err}");
            let mut bad_line = line(1, &"ab".repeat(32));
            bad_line.pulse = pulse.to_string();
            let err = anchor.publish(&bad_line).await.unwrap_err();
            assert!(err.to_string().contains("pulse"), "{pulse:?}: {err}");
        }
        assert_eq!(mock.gets.load(Ordering::Relaxed), 0);
        assert!(mock.puts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_valid_last_line_without_a_newline_is_kept() {
        let bare = format!(
            "v1 echo 0123456789abcdef 1 {} 2026-10-25T00:00:00.000Z",
            "ab".repeat(32)
        );
        assert!(!bare.ends_with('\n'));
        let mock = Arc::new(Mock::new());
        mock.files.lock().unwrap().insert(
            "heads/echo/2026-06.log".into(),
            ("sha1".into(), bare.clone()),
        );
        let base = serve(mock.clone()).await;
        let anchor = github_at(base, 2026);
        let history = anchor.history("echo").await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].seq, 1);
        anchor.publish(&line(2, &"cd".repeat(32))).await.unwrap();
        let text = mock
            .files
            .lock()
            .unwrap()
            .get("heads/echo/2026-06.log")
            .unwrap()
            .1
            .clone();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[0], bare);
        assert!(lines[1].contains(" 2 "));
        assert!(text.ends_with('\n'));
    }

    #[tokio::test]
    async fn a_remote_fragment_is_not_dropped() {
        let fragment = "v1 echo not-a-line";
        let mock = Arc::new(Mock::new());
        mock.files.lock().unwrap().insert(
            "heads/echo/2026-06.log".into(),
            ("sha1".into(), fragment.to_string()),
        );
        let base = serve(mock.clone()).await;
        let anchor = github_at(base, 2026);
        let err = anchor.history("echo").await.unwrap_err();
        assert!(matches!(err, super::AnchorError::Line), "{err}");
        let err = anchor
            .publish(&line(2, &"cd".repeat(32)))
            .await
            .unwrap_err();
        assert!(matches!(err, super::AnchorError::Line), "{err}");
        assert!(mock.puts.lock().unwrap().is_empty());
        let kept = mock
            .files
            .lock()
            .unwrap()
            .get("heads/echo/2026-06.log")
            .unwrap()
            .1
            .clone();
        assert_eq!(kept, fragment);

        let good = format!(
            "v1 echo 0123456789abcdef 1 {} 2026-10-25T00:00:00.000Z\n",
            "ab".repeat(32)
        );
        let torn = format!("{good}not-a-line");
        let mock = Arc::new(Mock::new());
        mock.files.lock().unwrap().insert(
            "heads/echo/2026-06.log".into(),
            ("sha1".into(), torn.clone()),
        );
        let base = serve(mock.clone()).await;
        let anchor = github_at(base, 2026);
        assert!(anchor.history("echo").await.is_err());
        assert!(anchor.publish(&line(2, &"cd".repeat(32))).await.is_err());
        assert!(mock.puts.lock().unwrap().is_empty());
        let kept = mock
            .files
            .lock()
            .unwrap()
            .get("heads/echo/2026-06.log")
            .unwrap()
            .1
            .clone();
        assert_eq!(kept, torn);
    }

    #[tokio::test]
    async fn the_branch_check_and_the_ref_use_the_same_encoding() {
        for branch in ["feature/foo", "release-1.2", "feature/foo bar"] {
            let mock = Arc::new(Mock::new());
            let base = serve(mock.clone()).await;
            let anchor = GitHubAnchor::with_clock(
                "owner".into(),
                "repo".into(),
                branch.into(),
                "heads".into(),
                base,
                Secret::new("pn108-token-do-not-leak".into()),
                Arc::new(|| Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()),
            )
            .unwrap();
            let history = anchor.history("echo").await.unwrap();
            assert!(history.is_empty(), "{branch}");
            let uris = mock.uris.lock().unwrap().clone();
            let encoded = super::encode_path(branch);
            let branch_uri = uris
                .iter()
                .find(|uri| uri.contains("/branches/"))
                .unwrap_or_else(|| panic!("no branch request for {branch}: {uris:?}"));
            let file_uri = uris
                .iter()
                .find(|uri| uri.contains("ref="))
                .unwrap_or_else(|| panic!("no ref for {branch}: {uris:?}"));
            let branch_enc = branch_uri
                .split("/branches/")
                .nth(1)
                .unwrap_or_else(|| panic!("{branch_uri}"));
            let ref_enc = file_uri
                .split("ref=")
                .nth(1)
                .unwrap_or_else(|| panic!("{file_uri}"));
            assert_eq!(branch_enc, encoded, "{branch_uri}");
            assert_eq!(ref_enc, encoded, "{file_uri}");
        }
    }

    #[tokio::test]
    async fn two_runs_write_two_lines_and_the_third_does_not_put() {
        let mock = Arc::new(Mock::new());
        let base = serve(mock.clone()).await;
        let mut events = vec![
            ReceiptEvent::Genesis {
                pulse: "echo".into(),
            },
            ReceiptEvent::Shutdown {
                reason: "stop".into(),
            },
        ];
        let first = super::super::receipt_bytes(&events);
        let index = chain::index_bytes(super::super::complete_prefix(&first));
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(github_at(base.clone(), 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "published");

        events.push(ReceiptEvent::Shutdown {
            reason: "again".into(),
        });
        let second = super::super::receipt_bytes(&events);
        let index = chain::index_bytes(super::super::complete_prefix(&second));
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(github_at(base.clone(), 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "published");
        assert_eq!(mock.puts.lock().unwrap().len(), 2);
        let text = mock
            .files
            .lock()
            .unwrap()
            .get("heads/echo/2026-06.log")
            .unwrap()
            .1
            .clone();
        assert_eq!(text.lines().count(), 2, "{text}");

        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(github_at(base.clone(), 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "unchanged");
        assert_eq!(mock.puts.lock().unwrap().len(), 2);

        events.push(ReceiptEvent::Shutdown {
            reason: "later".into(),
        });
        let third = super::super::receipt_bytes(&events);
        let index = chain::index_bytes(&third);
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(github_at(base, 2026))];
        let out = super::super::sync(&index, &backends, true).await.unwrap();
        assert_eq!(out.state, "dry-run");
        assert!(out.line.as_deref().unwrap().starts_with("v1 echo "));
        assert_eq!(mock.puts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn mismatch_and_rewind_publish_nothing() {
        let bytes = super::super::receipt_bytes(&[
            ReceiptEvent::Genesis {
                pulse: "echo".into(),
            },
            ReceiptEvent::Shutdown {
                reason: "stop".into(),
            },
        ]);
        let index = chain::index_bytes(&bytes);
        let ledger = index.genesis_hash.as_deref().unwrap()[..16].to_string();
        let head = "ab".repeat(32);

        let mock = Arc::new(Mock::new());
        mock.files.lock().unwrap().insert(
            "heads/echo/2026-06.log".into(),
            (
                "sha1".into(),
                format!("v1 echo {ledger} 0 {head} 2026-10-25T03:00:00.000Z\n"),
            ),
        );
        let base = serve(mock.clone()).await;
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(github_at(base, 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "mismatch");
        assert!(mock.puts.lock().unwrap().is_empty());

        let mock = Arc::new(Mock::new());
        mock.files.lock().unwrap().insert(
            "heads/echo/2026-06.log".into(),
            (
                "sha1".into(),
                format!("v1 echo {ledger} 9 {head} 2026-10-25T03:00:00.000Z\n"),
            ),
        );
        let base = serve(mock.clone()).await;
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(github_at(base, 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "rewind");
        assert!(mock.puts.lock().unwrap().is_empty());

        let mock = Arc::new(Mock::new());
        mock.files.lock().unwrap().insert(
            "heads/echo/2026-06.log".into(),
            (
                "sha1".into(),
                format!("v1 echo 0000000000000000 9 {head} 2026-10-25T03:00:00.000Z\n"),
            ),
        );
        let base = serve(mock.clone()).await;
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(github_at(base, 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "mismatch");
        let detail = out.notes[0].detail.as_deref().unwrap_or("");
        assert!(detail.contains("ledger_id 0000000000000000"), "{detail}");
        assert!(detail.contains(&format!("local {ledger}")), "{detail}");
        assert!(mock.puts.lock().unwrap().is_empty());
    }
}
