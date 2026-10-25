//! `pulse-null receipts verify` and `show`.
//!
//! Both read a file. Neither locks it or repairs a torn tail. The store does
//! that when it opens.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::oneself::chain::{self, VerifyState};
use crate::oneself::store::{self, Inspected};

/// Exit after the report was already printed.
#[derive(Debug)]
pub struct PrintedExit(pub i32);

impl std::fmt::Display for PrintedExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "exit {}", self.0)
    }
}

impl std::error::Error for PrintedExit {}

pub async fn verify(file: Option<PathBuf>, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let path = match resolve_path(file) {
        Ok(path) => path,
        Err(err) => return unverified(json, &err.to_string()),
    };
    let inspected = match store::inspect(&path) {
        Ok(inspected) => inspected,
        Err(err) => return unverified(json, &err.to_string()),
    };
    emit(json, &inspected)
}

pub async fn show(
    file: Option<PathBuf>,
    since: Option<String>,
    kind: Option<String>,
    limit: Option<u64>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = match resolve_path(file) {
        Ok(path) => path,
        Err(err) => return unverified(json, &err.to_string()),
    };
    let inspected = match store::inspect(&path) {
        Ok(inspected) => inspected,
        Err(err) => return unverified(json, &err.to_string()),
    };
    if inspected.report.first_break.is_some() {
        return emit(json, &inspected);
    }
    let rows = filtered_rows(&inspected, since.as_deref(), kind.as_deref(), limit);
    if json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "state": state_name(inspected.report.state()),
                "rows": rows,
            }))?
        );
    } else {
        for row in &rows {
            println!("{}", row.line);
        }
    }
    Ok(())
}

fn unverified(json_out: bool, message: &str) -> Result<(), Box<dyn std::error::Error>> {
    let body = unverified_body(message);
    if json_out {
        println!("{}", serde_json::to_string(&body)?);
    } else {
        println!("{message}");
    }
    Err(Box::new(PrintedExit(2)))
}

fn unverified_body(message: &str) -> Value {
    json!({
        "state": "unverified",
        "error": message,
    })
}

fn resolve_path(file: Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    resolve_with(file, || Ok(Config::load()?))
}

fn resolve_with(
    file: Option<PathBuf>,
    load: impl FnOnce() -> Result<Config, Box<dyn std::error::Error>>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = file {
        return Ok(path);
    }
    let config = load()?;
    if !config.receipts.path.is_empty() {
        return Ok(PathBuf::from(config.receipts.path));
    }
    Ok(store::default_receipts_path(&config.pulse.name))
}

fn emit(json: bool, inspected: &Inspected) -> Result<(), Box<dyn std::error::Error>> {
    let report = &inspected.report;
    if json {
        println!("{}", serde_json::to_string(&json_report(inspected))?);
    } else if let Some(broken) = &report.first_break {
        println!("broken line {}: {}", broken.line, broken.reason);
    } else {
        let seq = report
            .seq
            .map(|seq| seq.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{} rows={} seq={seq} head={}",
            state_name(report.state()),
            report.rows,
            report.head
        );
        for warn in &report.warns {
            println!("line {}: {}", warn.line, warn.reason);
        }
    }
    if report.first_break.is_some() {
        Err(Box::new(PrintedExit(1)))
    } else {
        Ok(())
    }
}

fn json_report(inspected: &Inspected) -> Value {
    let report = &inspected.report;
    json!({
        "state": state_name(report.state()),
        "rows": report.rows,
        "seq": report.seq,
        "head": report.head,
        "warns": notes(&report.warns),
        "break": report.first_break.as_ref().map(|note| json!({
            "line": note.line,
            "reason": note.reason,
        })),
        "orphans": store::orphan_streams(&inspected.bytes),
    })
}

fn notes(items: &[crate::oneself::chain::VerifyNote]) -> Vec<Value> {
    items
        .iter()
        .map(|note| json!({"line": note.line, "reason": note.reason}))
        .collect()
}

fn state_name(state: VerifyState) -> &'static str {
    match state {
        VerifyState::Ok => "ok",
        VerifyState::Warn => "warn",
        VerifyState::Broken => "broken",
    }
}

#[derive(Serialize)]
struct ShownRow {
    seq: u64,
    ts: String,
    kind: String,
    line: String,
}

fn filtered_rows(
    inspected: &Inspected,
    since: Option<&str>,
    kind: Option<&str>,
    limit: Option<u64>,
) -> Vec<ShownRow> {
    let mut rows = Vec::new();
    for line in inspected.bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        let Ok(row) = serde_json::from_str::<chain::Row>(text) else {
            continue;
        };
        let row_kind = row
            .e
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(kind) = kind {
            if row_kind != kind {
                continue;
            }
        }
        if let Some(since) = since {
            if row.ts.as_str() < since {
                continue;
            }
        }
        rows.push(ShownRow {
            seq: row.seq,
            ts: row.ts,
            kind: row_kind,
            line: text.to_string(),
        });
    }
    if let Some(limit) = limit {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        if rows.len() > limit {
            rows = rows.split_off(rows.len() - limit);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oneself::chain::VerifyState;
    use crate::oneself::event::ReceiptEvent;
    use crate::oneself::protect::LiveProbe;
    use crate::oneself::store::{ReceiptStore, StoreSource};
    use std::io::Write;

    fn sample(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("receipts.jsonl");
        let (mut store, _) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        store
            .append(ReceiptEvent::Shutdown {
                reason: "stop".to_string(),
            })
            .unwrap();
        drop(store);
        path
    }

    #[test]
    fn verify_of_a_temp_file_is_ok_and_a_flipped_byte_names_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let inspected = store::inspect(&path).unwrap();
        assert_eq!(inspected.report.state(), VerifyState::Ok);
        let value = json_report(&inspected);
        assert_eq!(value["state"], "ok");
        assert_eq!(value["rows"], 2);
        assert!(value["seq"].is_number());
        assert_eq!(value["head"].as_str().unwrap().len(), 64);
        assert_eq!(value["warns"], json!([]));
        assert!(value["break"].is_null());
        assert_eq!(value["orphans"], json!([]));

        let mut bytes = std::fs::read(&path).unwrap();
        let nl = bytes.iter().position(|byte| *byte == b'\n').unwrap();
        bytes[nl / 2] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let inspected = store::inspect(&path).unwrap();
        assert_eq!(inspected.report.state(), VerifyState::Broken);
        let broken = json_report(&inspected);
        assert_eq!(broken["state"], "broken");
        assert!(broken["break"]["line"].as_u64().unwrap() >= 1);
        assert!(broken["break"]["reason"].as_str().is_some());
    }

    #[test]
    fn show_filters_kind_since_and_limit() {
        let genesis = chain::canonical(
            &serde_json::to_value(chain::Row {
                v: 1,
                seq: 0,
                prev: chain::GENESIS_PREV.to_string(),
                ts: "2026-10-25T03:00:00.000Z".to_string(),
                e: serde_json::to_value(ReceiptEvent::Genesis {
                    pulse: "echo".to_string(),
                })
                .unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        let hash = chain::sha256_hex(genesis.as_bytes());
        let later = chain::canonical(
            &serde_json::to_value(chain::Row {
                v: 1,
                seq: 1,
                prev: hash,
                ts: "2026-10-25T04:00:00.000Z".to_string(),
                e: serde_json::to_value(ReceiptEvent::Shutdown {
                    reason: "stop".to_string(),
                })
                .unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        let mut body = Vec::new();
        writeln!(&mut body, "{genesis}").unwrap();
        writeln!(&mut body, "{later}").unwrap();
        let inspected = Inspected {
            bytes: body,
            report: chain::verify_lines(&b""[..]),
        };
        let all = filtered_rows(&inspected, None, None, None);
        assert_eq!(all.len(), 2);
        let shutdowns = filtered_rows(&inspected, None, Some("shutdown"), None);
        assert_eq!(shutdowns.len(), 1);
        assert_eq!(shutdowns[0].kind, "shutdown");
        let since = filtered_rows(&inspected, Some("2026-10-25T03:30:00.000Z"), None, None);
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].seq, 1);
        let last = filtered_rows(&inspected, None, None, Some(1));
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].kind, "shutdown");
        let text = serde_json::to_string(&json!({
            "state": "ok",
            "rows": all,
        }))
        .unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["rows"][0]["kind"], "genesis");
        assert_eq!(parsed["rows"][0]["seq"], 0);
        assert!(parsed["rows"][0]["line"]
            .as_str()
            .unwrap()
            .contains("genesis"));
        assert_eq!(parsed["rows"][1]["ts"], "2026-10-25T04:00:00.000Z");
    }

    #[test]
    fn reopen_after_a_cut_verifies_as_warn() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 6);
        std::fs::write(&path, &bytes).unwrap();
        let (store, opened) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        assert!(opened.recovered);
        drop(store);
        let inspected = store::inspect(&path).unwrap();
        assert_eq!(
            inspected.report.state(),
            VerifyState::Warn,
            "{:?}",
            inspected.report
        );
        let value = json_report(&inspected);
        assert_eq!(value["state"], "warn");
        assert!(!value["warns"].as_array().unwrap().is_empty());
    }

    fn block<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn assert_json_unverified(message: &str, fragment: &str) {
        assert!(
            message.contains(fragment),
            "message {message:?} missing {fragment:?}"
        );
        let text = serde_json::to_string(&unverified_body(message)).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["state"], "unverified");
        assert_eq!(parsed["error"], message);
        assert!(parsed.as_object().unwrap().len() == 2);
    }

    #[test]
    fn a_missing_file_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.jsonl");
        let err = store::inspect(&path).unwrap_err();
        assert_json_unverified(&err.to_string(), "No such file");
        let exit = block(verify(Some(path.clone()), true)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
        let shown = block(show(Some(path), None, None, None, true)).unwrap_err();
        assert_eq!(shown.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn a_directory_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let err = store::inspect(dir.path()).unwrap_err();
        assert!(matches!(err, store::StoreError::NotFile { .. }), "{err}");
        assert_json_unverified(&err.to_string(), "not a regular file");
        let exit = block(verify(Some(dir.path().to_path_buf()), false)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn an_unreadable_file_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        std::fs::write(&path, b"{}\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = store::inspect(&path).unwrap_err();
        assert_json_unverified(&err.to_string(), "Permission denied");
        let exit = block(verify(Some(path), true)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn a_missing_config_is_unverified() {
        let missing = crate::errors::ConfigError::NotFound(
            "No pulse-null.toml found. Run `pulse-null init` first.".into(),
        );
        let err = resolve_with(None, || Err(Box::new(missing))).unwrap_err();
        assert_json_unverified(&err.to_string(), "pulse-null.toml");
        let exit = unverified(true, &err.to_string()).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
        let human = unverified(false, &err.to_string()).unwrap_err();
        assert_eq!(human.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn a_fifo_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.fifo");
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path in the temp directory.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
        let err = store::inspect(&path).unwrap_err();
        assert!(matches!(err, store::StoreError::NotFile { .. }), "{err}");
        assert_json_unverified(&err.to_string(), "not a regular file");
        let started = std::time::Instant::now();
        let exit = block(verify(Some(path), true)).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn a_broken_chain_still_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let mut bytes = std::fs::read(&path).unwrap();
        let nl = bytes.iter().position(|byte| *byte == b'\n').unwrap();
        bytes[nl / 2] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let exit = block(verify(Some(path.clone()), true)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 1);
        let shown = block(show(Some(path), None, None, None, false)).unwrap_err();
        assert_eq!(shown.downcast_ref::<PrintedExit>().unwrap().0, 1);
    }
}
