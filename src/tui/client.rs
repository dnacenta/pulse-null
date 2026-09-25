//! HTTP + SSE client for the daemon. The TUI owns no provider, tools, or
//! session store: everything it knows arrives through this type.

use futures_core::Stream;
use serde::Deserialize;
use tokio_stream::StreamExt as _;

/// One server-sent event, as parsed off the wire.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    #[error("daemon returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("bad JSON from daemon: {0}")]
    Json(#[from] serde_json::Error),
}

/// The outcome of a health probe, coarse enough for a menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// `/health` answered OK and named the pulse we expected.
    Up,
    /// `/health` answered OK but for another pulse: the port is theirs.
    Foreign(String),
    /// Nothing listens (connection refused): a daemon can be started.
    Refused,
    /// Something answered, but not a healthy daemon (timeout, bad status).
    Other,
}

/// One connection pool for every client in the process: building a
/// reqwest client is not free, and Home makes one per pulse row.
fn shared_http() -> reqwest::Client {
    static HTTP: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .no_proxy()
            .build()
            .expect("reqwest client")
    });
    HTTP.clone()
}

/// A connection to one pulse daemon.
#[derive(Clone)]
pub struct Client {
    base: String,
    secret: Option<String>,
    http: reqwest::Client,
}

/// `/api/alerts/peek` body — only the count is exposed without draining.
#[derive(Debug, Deserialize)]
struct PeekResponse {
    count: usize,
}

/// One event of a streamed chat turn: the shared wire type.
pub type ChatEvent = crate::wire::ChatStreamEvent;

/// Decode a raw SSE event from the chat stream. Unknown event names and
/// payloads that do not fit are `None`; the caller ignores them.
#[must_use]
pub fn chat_event_from_sse(ev: &SseEvent) -> Option<ChatEvent> {
    ChatEvent::from_sse_parts(ev.event.as_deref()?, &ev.data)
}

/// One message of `/api/session/{channel}`.
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryMessage {
    pub role: String,
    pub text: String,
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct HistoryResponse {
    messages: Vec<HistoryMessage>,
}

impl Client {
    #[must_use]
    pub fn new(host: &str, port: u16, secret: Option<String>) -> Self {
        Self {
            base: format!("http://{host}:{port}"),
            secret,
            http: shared_http(),
        }
    }

    /// The daemon's base URL, for the bar and logs.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let mut r = self.http.get(format!("{}{}", self.base, path));
        if let Some(s) = &self.secret {
            r = r.header("X-Echo-Secret", s);
        }
        r
    }

    /// What listens on the daemon's port, for Home's pulse rows. `/health`
    /// needs no credential, so none is sent: a probe goes to whatever holds
    /// the port. The daemon's answer names its pulse; a mismatch is
    /// `Foreign`, never `Up`.
    pub async fn probe_detail(&self, expect_pulse: &str) -> Probe {
        let resp = self
            .http
            .get(format!("{}/health", self.base))
            .timeout(std::time::Duration::from_secs(1))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                match r.json::<crate::wire::HealthResponse>().await {
                    Ok(h) if h.pulse == expect_pulse => Probe::Up,
                    Ok(h) => Probe::Foreign(h.pulse),
                    Err(_) => Probe::Other,
                }
            }
            Ok(_) => Probe::Other,
            Err(e) if e.is_connect() => Probe::Refused,
            Err(_) => Probe::Other,
        }
    }

    /// True when `/health` answers 200 within a second (any pulse).
    pub async fn probe(&self) -> bool {
        matches!(
            self.http
                .get(format!("{}/health", self.base))
                .timeout(std::time::Duration::from_secs(1))
                .send()
                .await,
            Ok(r) if r.status().is_success()
        )
    }

    /// Small JSON GETs get a hard timeout so a half-dead daemon cannot stall
    /// the render loop. Streams (`events`, `chat_stream`) deliberately do not.
    async fn json(&self, path: &str) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .get(path)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(ClientError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// `GET /health`, the fields the TUI reads.
    pub async fn health(&self) -> Result<crate::wire::HealthResponse, ClientError> {
        Ok(serde_json::from_value(self.json("/health").await?)?)
    }

    /// `GET /api/dashboard`, the fields the TUI reads.
    pub async fn dashboard(&self) -> Result<crate::wire::DashboardResponse, ClientError> {
        Ok(serde_json::from_value(self.json("/api/dashboard").await?)?)
    }

    /// Number of pending alerts, from `/api/alerts/peek`.
    pub async fn alerts_count(&self) -> Result<usize, ClientError> {
        let v = self.json("/api/alerts/peek").await?;
        let peek: PeekResponse = serde_json::from_value(v)?;
        Ok(peek.count)
    }

    /// The conversation on `channel` as the daemon has it.
    pub async fn history(&self, channel: &str) -> Result<Vec<HistoryMessage>, ClientError> {
        let v = self.json(&format!("/api/session/{channel}")).await?;
        let h: HistoryResponse = serde_json::from_value(v)?;
        Ok(h.messages)
    }

    /// Open a streamed turn on `channel`. Dropping the stream cancels the
    /// turn on the daemon.
    pub async fn chat_stream(
        &self,
        channel: &str,
        message: &str,
    ) -> Result<impl Stream<Item = Result<ChatEvent, ClientError>> + Send, ClientError> {
        let mut req = self
            .http
            .post(format!("{}/api/chat/stream", self.base))
            .header("Accept", "text/event-stream")
            .json(&serde_json::json!({ "message": message, "channel": channel }));
        if let Some(s) = &self.secret {
            req = req.header("X-Echo-Secret", s);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Status { status, body });
        }
        let events = sse_stream(resp);
        Ok(async_stream::stream! {
            let mut events = std::pin::pin!(events);
            while let Some(item) = events.next().await {
                match item {
                    Ok(ev) => {
                        if let Some(ce) = chat_event_from_sse(&ev) {
                            yield Ok(ce);
                        }
                    }
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
        })
    }

    /// Open `GET /api/events`, resuming after `after` when given.
    ///
    /// The stream ends when the daemon closes the connection; the caller
    /// reconnects with the last id it saw.
    pub async fn events(
        &self,
        after: Option<u64>,
    ) -> Result<impl Stream<Item = Result<SseEvent, ClientError>> + Send, ClientError> {
        let mut req = self
            .get("/api/events")
            .header("Accept", "text/event-stream");
        if let Some(id) = after {
            req = req.header("Last-Event-ID", id.to_string());
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Status { status, body });
        }
        Ok(sse_stream(resp))
    }
}

/// Turn a streaming HTTP body into parsed SSE events.
fn sse_stream(resp: reqwest::Response) -> impl Stream<Item = Result<SseEvent, ClientError>> + Send {
    async_stream::stream! {
        let mut body = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => {
                    buf.extend_from_slice(&bytes);
                    for ev in parse_sse(&mut buf) {
                        yield Ok(ev);
                    }
                }
                Err(e) => {
                    yield Err(ClientError::Http(e));
                    return;
                }
            }
        }
        // A trailing event without a final blank line is still an event.
        buf.extend_from_slice(b"\n\n");
        for ev in parse_sse(&mut buf) {
            yield Ok(ev);
        }
    }
}

/// Parse every complete event out of `buf`, leaving any partial tail in place.
///
/// Implements the SSE wire format: fields `id`, `event`, `data` (multi-line
/// data joined with `\n`), `:` comments ignored, events separated by a blank
/// line, `\r\n` accepted. Unknown fields are ignored.
pub fn parse_sse(buf: &mut Vec<u8>) -> Vec<SseEvent> {
    let mut out = Vec::new();
    while let Some((end, sep_len)) = find_separator(buf) {
        let block: Vec<u8> = buf.drain(..end + sep_len).collect();
        let block = &block[..end];
        let text = String::from_utf8_lossy(block);
        let mut ev = SseEvent::default();
        let mut data_lines: Vec<&str> = Vec::new();
        let mut any = false;
        for line in text.split(['\n', '\r']) {
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            any = true;
            let (field, value) = match line.find(':') {
                Some(i) => (
                    &line[..i],
                    line[i + 1..].strip_prefix(' ').unwrap_or(&line[i + 1..]),
                ),
                None => (line, ""),
            };
            match field {
                "id" => ev.id = Some(value.to_string()),
                "event" => ev.event = Some(value.to_string()),
                "data" => data_lines.push(value),
                _ => {}
            }
        }
        if any {
            ev.data = data_lines.join("\n");
            out.push(ev);
        }
    }
    out
}

/// Position and length of the first event separator (`\n\n` or `\r\n\r\n`).
fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_single_event() {
        let mut buf = b"id: 7\nevent: row\ndata: {\"a\":1}\n\n".to_vec();
        let evs = parse_sse(&mut buf);
        assert_eq!(
            evs,
            vec![SseEvent {
                id: Some("7".into()),
                event: Some("row".into()),
                data: "{\"a\":1}".into(),
            }]
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn parse_sse_handles_split_chunks_and_crlf() {
        let mut buf = b"data: hel".to_vec();
        assert!(
            parse_sse(&mut buf).is_empty(),
            "partial event stays buffered"
        );
        buf.extend_from_slice(b"lo\r\ndata: world\r\n\r\nevent: x\r\n");
        let evs = parse_sse(&mut buf);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "hello\nworld");
        assert_eq!(
            buf,
            b"event: x\r\n".to_vec(),
            "tail without separator is kept"
        );
    }

    #[test]
    fn parse_sse_ignores_comments_and_unknown_fields() {
        let mut buf = b": keep-alive\n\nfoo: bar\ndata: ok\n\n".to_vec();
        let evs = parse_sse(&mut buf);
        assert_eq!(evs.len(), 1, "a comment-only block is not an event");
        assert_eq!(evs[0].data, "ok");
        assert!(evs[0].id.is_none());
    }

    #[test]
    fn chat_events_decode_from_sse() {
        use crate::wire::{ChatStreamEvent, TurnPhase};
        let ev = |name: &str, data: &str| SseEvent {
            id: None,
            event: Some(name.into()),
            data: data.into(),
        };
        assert_eq!(
            chat_event_from_sse(&ev("status", r#"{"status":"tool","name":"file_read"}"#)),
            Some(ChatStreamEvent::Status {
                status: TurnPhase::Tool,
                name: Some("file_read".into())
            })
        );
        assert_eq!(
            chat_event_from_sse(&ev("delta", r#"{"text":"hi"}"#)),
            Some(ChatStreamEvent::Delta { text: "hi".into() })
        );
        assert_eq!(chat_event_from_sse(&ev("row", "{}")), None);
        assert_eq!(
            chat_event_from_sse(&ev("status", r#"{"status":"dreaming"}"#)),
            None
        );
    }

    #[test]
    fn parse_sse_data_without_space_after_colon() {
        let mut buf = b"data:x\n\n".to_vec();
        assert_eq!(parse_sse(&mut buf)[0].data, "x");
    }
}
