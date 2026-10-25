//! Contents API on `heads/<pulse>/<year>.log`.
//!
//! GET the object, PUT the previous bytes plus one line. A 409 is one fresh
//! GET and one retry. A 404 on GET creates the file.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::{DateTime, Datelike, Utc};

use super::{format_line, parse_history, Anchor, AnchorError, AnchorLine, BoxFuture, Secret};

const API_VERSION: &str = "2026-03-10";
const MAX_BYTES: usize = 1024 * 1024;

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

    fn path(&self, pulse: &str, year: i32) -> String {
        format!("{}/{pulse}/{year}.log", self.dir.trim_matches('/'))
    }

    fn url(&self, path: &str) -> String {
        let base = self.api_base.trim_end_matches('/');
        format!(
            "{base}/repos/{}/{}/contents/{}?ref={}",
            encode(&self.owner),
            encode(&self.repo),
            encode_path(path),
            encode(&self.branch)
        )
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.token.expose()))
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(
                "User-Agent",
                format!("pulse-null/{}", env!("CARGO_PKG_VERSION")),
            )
    }

    async fn get(&self, path: &str) -> Result<Option<(String, String)>, AnchorError> {
        let response = self
            .request(reqwest::Method::GET, &self.url(path))
            .header("Accept", "application/vnd.github.object+json")
            .send()
            .await
            .map_err(|_| AnchorError::Transport)?;
        let status = response.status();
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(self.status(status));
        }
        let body: Object = response.json().await.map_err(|_| AnchorError::Transport)?;
        let raw = body.content.unwrap_or_default();
        let cleaned: String = raw.chars().filter(|ch| !ch.is_whitespace()).collect();
        let bytes = if cleaned.is_empty() {
            Vec::new()
        } else {
            STANDARD.decode(cleaned).map_err(|_| AnchorError::Line)?
        };
        if bytes.len() > MAX_BYTES {
            return Err(AnchorError::TooLarge);
        }
        let text = String::from_utf8(bytes).map_err(|_| AnchorError::Line)?;
        Ok(Some((body.sha.unwrap_or_default(), text)))
    }

    async fn put_once(
        &self,
        path: &str,
        text: &str,
        sha: Option<&str>,
        message: &str,
    ) -> Result<(), AnchorError> {
        if text.len() > MAX_BYTES {
            return Err(AnchorError::TooLarge);
        }
        let mut body = serde_json::json!({
            "message": message,
            "content": STANDARD.encode(text.as_bytes()),
            "branch": self.branch,
        });
        if let Some(sha) = sha {
            body["sha"] = serde_json::Value::String(sha.to_string());
        }
        let response = self
            .request(reqwest::Method::PUT, &self.url(path))
            .header("Accept", "application/vnd.github+json")
            .json(&body)
            .send()
            .await
            .map_err(|_| AnchorError::Transport)?;
        let status = response.status();
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
            let current = self.get(path).await?;
            let text = append_text(current.as_ref().map(|(_, text)| text.as_str()), line);
            let sha = current.as_ref().map(|(sha, _)| sha.as_str());
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

fn append_text(previous: Option<&str>, line: &AnchorLine) -> String {
    let mut text = String::new();
    if let Some(previous) = previous {
        let kept = if previous.is_empty() || previous.ends_with('\n') {
            previous.to_string()
        } else {
            match previous.rfind('\n') {
                Some(end) => previous[..=end].to_string(),
                None => String::new(),
            }
        };
        text.push_str(&kept);
    }
    text.push_str(&format_line(line));
    text.push('\n');
    text
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
            let year = (self.now)().year();
            let mut lines = Vec::new();
            for year in [year - 1, year] {
                let path = self.path(pulse, year);
                if let Some((_, text)) = self.get(&path).await? {
                    lines.extend(parse_history(&text)?);
                }
            }
            Ok(lines)
        })
    }

    fn publish<'a>(&'a self, line: &'a AnchorLine) -> BoxFuture<'a, Result<(), AnchorError>> {
        let path = self.path(&line.pulse, (self.now)().year());
        Box::pin(async move { self.publish_path(&path, line).await })
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
        puts: Mutex<Vec<serde_json::Value>>,
        gets: AtomicUsize,
        put_status: Mutex<Vec<u16>>,
        get_status: Mutex<Vec<u16>>,
        headers: Mutex<Vec<(String, String, String)>>,
        stale_once: AtomicBool,
        sha_n: AtomicUsize,
    }

    impl Mock {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
                puts: Mutex::new(Vec::new()),
                gets: AtomicUsize::new(0),
                put_status: Mutex::new(Vec::new()),
                get_status: Mutex::new(Vec::new()),
                headers: Mutex::new(Vec::new()),
                stale_once: AtomicBool::new(false),
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
        let key = path.split("/contents/").nth(1).unwrap_or("").to_string();
        match request.method().as_str() {
            "GET" => {
                mock.gets.fetch_add(1, Ordering::Relaxed);
                if let Some(status) = mock.get_status.lock().unwrap().pop() {
                    return (StatusCode::from_u16(status).unwrap(), "no".into());
                }
                let files = mock.files.lock().unwrap();
                if let Some((sha, text)) = files.get(&key) {
                    let body = serde_json::json!({
                        "sha": sha,
                        "content": STANDARD.encode(text.as_bytes()),
                        "encoding": "base64",
                    });
                    (StatusCode::OK, body.to_string())
                } else {
                    (StatusCode::NOT_FOUND, "missing".into())
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

    fn anchor(base: String, year: i32) -> GitHubAnchor {
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

    #[tokio::test]
    async fn create_append_conflict_and_headers() {
        let mock = Arc::new(Mock::new());
        let base = serve(mock.clone()).await;
        let anchor = anchor(base, 2026);
        anchor.publish(&line(1, &"ab".repeat(32))).await.unwrap();
        let stored = mock
            .files
            .lock()
            .unwrap()
            .get("heads/echo/2026.log")
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
            .get("heads/echo/2026.log")
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
                mock.files
                    .lock()
                    .unwrap()
                    .insert("heads/echo/2026.log".into(), ("sha1".into(), String::new()));
            } else {
                mock.get_status.lock().unwrap().push(status);
            }
            let base = serve(mock).await;
            let anchor = anchor(base, 2026);
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
        let anchor = anchor(base, 2026);
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
        assert!(mock
            .files
            .lock()
            .unwrap()
            .contains_key("heads/echo/2026.log"));
        assert!(mock
            .files
            .lock()
            .unwrap()
            .contains_key("heads/echo/2027.log"));
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
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(anchor(base.clone(), 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "published");

        events.push(ReceiptEvent::Shutdown {
            reason: "again".into(),
        });
        let second = super::super::receipt_bytes(&events);
        let index = chain::index_bytes(super::super::complete_prefix(&second));
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(anchor(base.clone(), 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "published");
        assert_eq!(mock.puts.lock().unwrap().len(), 2);
        let text = mock
            .files
            .lock()
            .unwrap()
            .get("heads/echo/2026.log")
            .unwrap()
            .1
            .clone();
        assert_eq!(text.lines().count(), 2, "{text}");

        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(anchor(base.clone(), 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "unchanged");
        assert_eq!(mock.puts.lock().unwrap().len(), 2);

        events.push(ReceiptEvent::Shutdown {
            reason: "later".into(),
        });
        let third = super::super::receipt_bytes(&events);
        let index = chain::index_bytes(&third);
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(anchor(base, 2026))];
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
            "heads/echo/2026.log".into(),
            (
                "sha1".into(),
                format!("v1 echo {ledger} 0 {head} 2026-10-25T03:00:00.000Z\n"),
            ),
        );
        let base = serve(mock.clone()).await;
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(anchor(base, 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "mismatch");
        assert!(mock.puts.lock().unwrap().is_empty());

        let mock = Arc::new(Mock::new());
        mock.files.lock().unwrap().insert(
            "heads/echo/2026.log".into(),
            (
                "sha1".into(),
                format!("v1 echo {ledger} 9 {head} 2026-10-25T03:00:00.000Z\n"),
            ),
        );
        let base = serve(mock.clone()).await;
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(anchor(base, 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "rewind");
        assert!(mock.puts.lock().unwrap().is_empty());

        let mock = Arc::new(Mock::new());
        mock.files.lock().unwrap().insert(
            "heads/echo/2026.log".into(),
            (
                "sha1".into(),
                format!("v1 echo 0000000000000000 9 {head} 2026-10-25T03:00:00.000Z\n"),
            ),
        );
        let base = serve(mock.clone()).await;
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(anchor(base, 2026))];
        let out = super::super::sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "mismatch");
        assert!(mock.puts.lock().unwrap().is_empty());
    }
}
