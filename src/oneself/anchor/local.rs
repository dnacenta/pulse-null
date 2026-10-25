//! `<ledger dir>/anchors.log`. Append one line and `sync_all`.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::{format_line, parse_history, Anchor, AnchorError, AnchorLine, BoxFuture};

pub struct LocalFileAnchor {
    path: PathBuf,
}

impl LocalFileAnchor {
    #[must_use]
    pub fn new(ledger_dir: &Path) -> Self {
        Self {
            path: ledger_dir.join("anchors.log"),
        }
    }
}

impl Anchor for LocalFileAnchor {
    fn name(&self) -> &'static str {
        "local"
    }

    fn history<'a>(
        &'a self,
        _pulse: &'a str,
    ) -> BoxFuture<'a, Result<Vec<AnchorLine>, AnchorError>> {
        let path = self.path.clone();
        Box::pin(async move { read_history(&path) })
    }

    fn publish<'a>(&'a self, line: &'a AnchorLine) -> BoxFuture<'a, Result<(), AnchorError>> {
        let path = self.path.clone();
        let text = format_line(line);
        Box::pin(async move { append_line(&path, &text) })
    }
}

fn read_history(path: &Path) -> Result<Vec<AnchorLine>, AnchorError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_history(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(_) => Err(AnchorError::Local),
    }
}

fn append_line(path: &Path, text: &str) -> Result<(), AnchorError> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o644)
        .open(path)
        .map_err(|_| AnchorError::Local)?;
    use std::io::Write;
    file.write_all(text.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|_| AnchorError::Local)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(seq: u64) -> AnchorLine {
        AnchorLine {
            pulse: "echo".into(),
            ledger_id: "0123456789abcdef".into(),
            seq,
            head: "ab".repeat(32),
            ts: "2026-10-25T00:00:00.000Z".into(),
        }
    }

    #[tokio::test]
    async fn a_local_anchor_appends_syncs_and_reads_history() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = LocalFileAnchor::new(dir.path());
        assert!(anchor.history("echo").await.unwrap().is_empty());
        anchor.publish(&sample(1)).await.unwrap();
        anchor.publish(&sample(2)).await.unwrap();
        let history = anchor.history("echo").await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].seq, 2);
        let text = std::fs::read_to_string(dir.path().join("anchors.log")).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(text.lines().count(), 2);
        let mut torn = text.clone();
        torn.push_str("v1 partial");
        std::fs::write(dir.path().join("anchors.log"), torn).unwrap();
        let history = anchor.history("echo").await.unwrap();
        assert_eq!(history.len(), 2, "a partial tail is not a line");
    }
}
