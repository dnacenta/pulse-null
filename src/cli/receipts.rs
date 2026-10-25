//! `pulse-null receipts verify` and `show`.
//!
//! Both read a file. Neither locks it or repairs a torn tail. The store does
//! that when it opens.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::oneself::chain::{self, VerifyState};
use crate::oneself::protect::{LiveProbe, Protection, ProtectionProbe};
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
    let located = match resolve_with(file, || Ok(Config::load()?)) {
        Ok(located) => located,
        Err(err) => return unverified(json, &err.to_string()),
    };
    let (bytes, file) = match store::read_receipt(&located.path) {
        Ok(opened) => opened,
        Err(err) => return unverified(json, &err.to_string()),
    };
    // Protection looks at this fd. The path is not opened again.
    let protection = LiveProbe.probe(&file, false);
    drop(file);
    let report = chain::verify_lines(bytes.as_slice());
    let inspected = Inspected { bytes, report };
    emit_verify(json, &inspected, &located, protection)
}

pub async fn show(
    file: Option<PathBuf>,
    since: Option<String>,
    kind: Option<String>,
    limit: Option<u64>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(message) = reject_filters(since.as_deref(), kind.as_deref()) {
        return unverified(json, message);
    }
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
            println!("{}", escape_show(&row.line));
        }
    }
    Ok(())
}

fn unverified(json_out: bool, message: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", unverified_line(json_out, message)?);
    Err(Box::new(PrintedExit(2)))
}

fn unverified_line(json_out: bool, message: &str) -> Result<String, Box<dyn std::error::Error>> {
    if json_out {
        Ok(serde_json::to_string(&unverified_body(message))?)
    } else {
        Ok(escape_show(message))
    }
}

fn unverified_body(message: &str) -> Value {
    json!({
        "state": "unverified",
        "error": message,
    })
}

/// Where `verify` got the path it checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathFrom {
    Flag,
    Config,
    Default,
}

#[derive(Debug)]
struct Located {
    path: PathBuf,
    from: PathFrom,
    /// Set when `[receipts] path` is not the default file.
    path_warn: Option<String>,
}

fn from_name(from: PathFrom) -> &'static str {
    match from {
        PathFrom::Flag => "--file",
        PathFrom::Config => "config",
        PathFrom::Default => "default",
    }
}

fn resolve_path(file: Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(resolve_with(file, || Ok(Config::load()?))?.path)
}

fn resolve_with(
    file: Option<PathBuf>,
    load: impl FnOnce() -> Result<Config, Box<dyn std::error::Error>>,
) -> Result<Located, Box<dyn std::error::Error>> {
    if let Some(path) = file {
        return Ok(Located {
            path,
            from: PathFrom::Flag,
            path_warn: None,
        });
    }
    let config = load()?;
    Ok(located_from_config(
        &config.receipts.path,
        &config.pulse.name,
    ))
}

fn located_from_config(configured: &str, pulse: &str) -> Located {
    let default = store::default_receipts_path(pulse);
    if configured.is_empty() {
        return Located {
            path: default,
            from: PathFrom::Default,
            path_warn: None,
        };
    }
    let path = PathBuf::from(configured);
    let path_warn = (path != default)
        .then(|| format!("config path differs from the default {}", default.display()));
    Located {
        path,
        from: PathFrom::Config,
        path_warn,
    }
}

fn preamble(located: &Located, protection: &Protection) -> String {
    let path = escape_show(&located.path.display().to_string());
    let mut text = format!(
        "file={path} source={}\nsystemd_fd={} owner_root={} mode_safe={} append_flag={} parent_safe={}",
        from_name(located.from),
        protection.systemd_fd,
        protection.owner_root,
        protection.mode_safe,
        protection.append_flag,
        protection.parent_safe
    );
    if let Some(warn) = &located.path_warn {
        let warn = escape_show(warn);
        text.push_str(&format!("\nwarn {warn}"));
    }
    text
}

fn emit_verify(
    json_out: bool,
    inspected: &Inspected,
    located: &Located,
    protection: Protection,
) -> Result<(), Box<dyn std::error::Error>> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string(&verify_json(inspected, located, protection))?
        );
    } else {
        println!("{}", preamble(located, &protection));
        print_human(inspected);
    }
    if inspected.report.first_break.is_some() {
        Err(Box::new(PrintedExit(1)))
    } else {
        Ok(())
    }
}

fn emit(json: bool, inspected: &Inspected) -> Result<(), Box<dyn std::error::Error>> {
    let report = &inspected.report;
    if json {
        println!("{}", serde_json::to_string(&json_report(inspected))?);
    } else {
        print_human(inspected);
    }
    if report.first_break.is_some() {
        Err(Box::new(PrintedExit(1)))
    } else {
        Ok(())
    }
}

fn print_human(inspected: &Inspected) {
    let report = &inspected.report;
    if let Some(broken) = &report.first_break {
        println!("broken line {}: {}", broken.line, broken.reason);
        return;
    }
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

fn verify_json(inspected: &Inspected, located: &Located, protection: Protection) -> Value {
    let mut value = json_report(inspected);
    value["file"] = json!(located.path.display().to_string());
    value["source"] = json!(from_name(located.from));
    value["protection"] = json!({
        "systemd_fd": protection.systemd_fd,
        "owner_root": protection.owner_root,
        "mode_safe": protection.mode_safe,
        "append_flag": protection.append_flag,
        "parent_safe": protection.parent_safe,
    });
    value["path_warn"] = json!(located.path_warn);
    value
}

fn notes(items: &[crate::oneself::chain::VerifyNote]) -> Vec<Value> {
    items
        .iter()
        .map(|note| json!({"line": note.line, "reason": note.reason}))
        .collect()
}

fn reject_filters(since: Option<&str>, kind: Option<&str>) -> Option<&'static str> {
    if let Some(since) = since {
        if !since_ok(since) {
            return Some("since must be RFC3339 or a prefix of YYYY-MM-DDTHH:MM:SS.mmmZ");
        }
    }
    if let Some(kind) = kind {
        if !kind_ok(kind) {
            return Some("kind must be one of genesis, boot, open, close, shutdown, recover, gap");
        }
    }
    None
}

fn since_ok(since: &str) -> bool {
    since_bound(since).is_some()
}

/// UTC instant in the stored form `YYYY-MM-DDTHH:MM:SS.mmmZ`.
///
/// A full RFC3339 value, including an offset or a fractional second, is
/// converted. A shorter prefix of that form is completed and checked as a
/// real date, so `2026-13` is rejected and `2026-10-25` starts at midnight.
fn since_bound(since: &str) -> Option<String> {
    if let Some(utc) = parse_rfc3339_utc(since) {
        return Some(format_ms(utc));
    }
    let completed = complete_prefix(since)?;
    if chain::ts_ok(&completed) {
        Some(completed)
    } else {
        None
    }
}

fn parse_rfc3339_utc(since: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let parsed = chrono::DateTime::parse_from_rfc3339(since)
        .ok()
        .or_else(|| {
            let with_seconds = insert_zero_seconds(since)?;
            chrono::DateTime::parse_from_rfc3339(&with_seconds).ok()
        })?;
    Some(parsed.with_timezone(&chrono::Utc))
}

/// `HH:MM` with no seconds, before `Z` or a numeric offset.
fn insert_zero_seconds(since: &str) -> Option<String> {
    let time_at = since.find('T')?;
    let time = &since[time_at + 1..];
    let zone_at = time.find(['Z', '+', '-'])?;
    let hour_minute = &time[..zone_at];
    if hour_minute.len() == 5 && hour_minute.as_bytes().get(2) == Some(&b':') {
        let mut out = String::with_capacity(since.len() + 3);
        out.push_str(&since[..=time_at]);
        out.push_str(hour_minute);
        out.push_str(":00");
        out.push_str(&time[zone_at..]);
        return Some(out);
    }
    None
}

fn format_ms(utc: chrono::DateTime<chrono::Utc>) -> String {
    let millis = utc.timestamp_subsec_millis();
    format!("{}.{:03}Z", utc.format("%Y-%m-%dT%H:%M:%S"), millis)
}

fn complete_prefix(since: &str) -> Option<String> {
    const FORM: &str = "0000-01-01T00:00:00.000Z";
    if since.is_empty() || since.len() > FORM.len() {
        return None;
    }
    let form = FORM.as_bytes();
    for (index, byte) in since.bytes().enumerate() {
        if form[index].is_ascii_digit() {
            if !byte.is_ascii_digit() {
                return None;
            }
        } else if byte != form[index] {
            return None;
        }
    }
    Some(format!("{since}{}", &FORM[since.len()..]))
}

fn kind_ok(kind: &str) -> bool {
    matches!(
        kind,
        "genesis" | "boot" | "open" | "close" | "shutdown" | "recover" | "gap"
    )
}

fn escape_show(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() || is_bidi_override(ch) {
            out.push_str(&format!("\\u{{{:X}}}", u32::from(ch)));
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_bidi_override(ch: char) -> bool {
    matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
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
    let bound = since.and_then(since_bound);
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
        if let Some(bound) = bound.as_deref() {
            if row.ts.as_str() < bound {
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

    #[test]
    fn verify_names_the_file_the_source_and_the_five_checks() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let located = Located {
            path: path.clone(),
            from: PathFrom::Flag,
            path_warn: None,
        };
        let (_bytes, file) = store::read_receipt(&path).unwrap();
        let protection = LiveProbe.probe(&file, false);
        drop(file);
        assert!(!protection.systemd_fd);
        let inspected = store::inspect(&path).unwrap();
        let value = verify_json(&inspected, &located, protection);
        assert_eq!(value["file"], path.display().to_string());
        assert_eq!(value["source"], "--file");
        assert_eq!(value["protection"]["systemd_fd"], false);
        assert!(value["protection"]["owner_root"].is_boolean());
        assert!(value["protection"]["mode_safe"].is_boolean());
        assert!(value["protection"]["append_flag"].is_boolean());
        assert!(value["protection"]["parent_safe"].is_boolean());
        assert!(value["path_warn"].is_null());
        assert_eq!(value["state"], "ok");
        let text = preamble(&located, &protection);
        let rendered = path.display().to_string();
        assert!(text.contains(&format!("file={rendered} source=--file")));
        assert!(text.contains(&format!(
            "systemd_fd={} owner_root={} mode_safe={} append_flag={} parent_safe={}",
            protection.systemd_fd,
            protection.owner_root,
            protection.mode_safe,
            protection.append_flag,
            protection.parent_safe
        )));
        assert!(block(verify(Some(path), true)).is_ok());
    }

    #[test]
    fn a_config_path_off_the_default_is_warned() {
        let located = located_from_config("/tmp/decoy.jsonl", "echo");
        assert_eq!(from_name(located.from), "config");
        let warn = located.path_warn.clone().unwrap();
        assert!(
            warn.contains("/var/lib/pulse-null/echo/receipts.jsonl"),
            "{warn}"
        );
        let protection = Protection {
            systemd_fd: false,
            owner_root: false,
            mode_safe: false,
            append_flag: false,
            parent_safe: false,
        };
        let text = preamble(&located, &protection);
        assert!(text.contains("file=/tmp/decoy.jsonl source=config"));
        assert!(text.contains(&format!("warn {warn}")));
        let value = verify_json(
            &Inspected {
                bytes: Vec::new(),
                report: chain::verify_lines(&b""[..]),
            },
            &located,
            protection,
        );
        assert_eq!(value["source"], "config");
        assert_eq!(value["path_warn"], warn);

        let fallback = located_from_config("", "echo");
        assert_eq!(from_name(fallback.from), "default");
        assert!(fallback.path_warn.is_none());
        assert_eq!(fallback.path, store::default_receipts_path("echo"));

        let same = located_from_config(
            &store::default_receipts_path("echo").display().to_string(),
            "echo",
        );
        assert_eq!(from_name(same.from), "config");
        assert!(same.path_warn.is_none());

        let flagged = resolve_with(Some(PathBuf::from("/tmp/flag.jsonl")), || {
            panic!("--file must not load config")
        })
        .unwrap();
        assert_eq!(from_name(flagged.from), "--file");
        assert!(flagged.path_warn.is_none());
    }

    #[test]
    fn an_empty_file_is_warn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        std::fs::write(&path, b"").unwrap();
        let inspected = store::inspect(&path).unwrap();
        assert_eq!(inspected.report.state(), VerifyState::Warn);
        assert_eq!(inspected.report.rows, 0);
        assert_eq!(inspected.report.warns.len(), 1);
        assert_eq!(inspected.report.warns[0].reason, "empty");
        assert!(inspected.report.first_break.is_none());
        assert!(block(verify(Some(path), true)).is_ok());
    }

    #[test]
    fn a_symlink_and_an_oversize_file_are_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.jsonl");
        std::fs::write(&target, b"keep").unwrap();
        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let started = std::time::Instant::now();
        let err = store::inspect(&link).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(matches!(err, store::StoreError::NotFile { .. }), "{err}");
        assert_json_unverified(&err.to_string(), "not a regular file");
        let exit = block(verify(Some(link), true)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");

        let huge = dir.path().join("huge.jsonl");
        let file = std::fs::File::create(&huge).unwrap();
        file.set_len(store::MAX_RECEIPT_BYTES + 1).unwrap();
        drop(file);
        let err = store::inspect(&huge).unwrap_err();
        assert_json_unverified(&err.to_string(), "256 MiB");
        let exit = block(show(Some(huge), None, None, None, true)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn verify_escapes_the_path_and_the_error_for_a_human() {
        let raw = "/var/lib/pulse-null/echo/\u{1b}\u{202e}receipts.jsonl";
        let located = Located {
            path: PathBuf::from(raw),
            from: PathFrom::Config,
            path_warn: Some(format!("config path differs from the default {raw}")),
        };
        let protection = Protection {
            systemd_fd: false,
            owner_root: false,
            mode_safe: false,
            append_flag: false,
            parent_safe: false,
        };
        let text = preamble(&located, &protection);
        assert!(text.contains("file="));
        assert!(text.contains("source=config"));
        assert!(text.contains("\\u{1B}"));
        assert!(text.contains("\\u{202E}"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{202e}'));
        assert!(text.contains("warn "));

        let err = store::StoreError::NotFile {
            path: raw.to_string(),
        };
        let human = unverified_line(false, &err.to_string()).unwrap();
        assert!(human.contains("\\u{1B}"));
        assert!(human.contains("\\u{202E}"));
        assert!(!human.contains('\u{1b}'));
        assert!(!human.contains('\u{202e}'));
        let json_line = unverified_line(true, &err.to_string()).unwrap();
        let parsed: Value = serde_json::from_str(&json_line).unwrap();
        assert_eq!(parsed["state"], "unverified");
        assert_eq!(parsed["error"], err.to_string());
        assert!(parsed["error"].as_str().unwrap().contains('\u{202e}'));

        let value = verify_json(
            &Inspected {
                bytes: Vec::new(),
                report: chain::verify_lines(&b""[..]),
            },
            &located,
            protection,
        );
        assert_eq!(value["file"], raw);
        assert!(value["path_warn"].as_str().unwrap().contains('\u{202e}'));
    }

    #[test]
    fn show_escapes_controls_and_bidi_overrides() {
        let raw = "alpha\u{1b}\u{7f}\u{9b}\u{202e}\u{2066}omega";
        let escaped = escape_show(raw);
        assert!(escaped.contains("\\u{1B}"));
        assert!(escaped.contains("\\u{7F}"));
        assert!(escaped.contains("\\u{9B}"));
        assert!(escaped.contains("\\u{202E}"));
        assert!(escaped.contains("\\u{2066}"));
        assert!(!escaped.chars().any(char::is_control));
        assert!(!escaped.contains('\u{202e}'));
        assert!(raw.contains('\u{202e}'));
        let shown = ShownRow {
            seq: 1,
            ts: "2026-10-25T03:00:00.000Z".to_string(),
            kind: "shutdown".to_string(),
            line: raw.to_string(),
        };
        let text = serde_json::to_string(&json!({"rows": [shown]})).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["rows"][0]["line"], raw);
    }

    #[test]
    fn a_bad_since_or_kind_exits_2() {
        assert!(since_ok("2026-10-25T03:00:00.000Z"));
        assert!(since_ok("2026-10-25T03:00:00.000+00:00"));
        assert!(since_ok("2026-10-25"));
        assert!(!since_ok("yesterday"));
        assert!(!since_ok("2026-13"));
        assert!(!since_ok("2026-02-31"));
        assert!(!since_ok(""));
        assert!(kind_ok("recover"));
        assert!(!kind_ok("bogus"));
        let since_message = reject_filters(Some("yesterday"), None).unwrap();
        assert_json_unverified(since_message, "since");
        let kind_message = reject_filters(None, Some("bogus")).unwrap();
        assert_json_unverified(kind_message, "kind");
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let since = block(show(
            Some(path.clone()),
            Some("yesterday".to_string()),
            None,
            None,
            true,
        ))
        .unwrap_err();
        assert_eq!(since.downcast_ref::<PrintedExit>().unwrap().0, 2);
        let kind = block(show(
            Some(path.clone()),
            None,
            Some("bogus".to_string()),
            None,
            false,
        ))
        .unwrap_err();
        assert_eq!(kind.downcast_ref::<PrintedExit>().unwrap().0, 2);
        assert!(reject_filters(Some("2026-10-25T04"), Some("gap")).is_none());
        let bad_month = block(show(
            Some(path),
            Some("2026-13".to_string()),
            None,
            None,
            true,
        ))
        .unwrap_err();
        assert_eq!(bad_month.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn since_compares_an_offset_in_utc_milliseconds() {
        assert_eq!(
            since_bound("2026-10-25T05:00:01+02:00").unwrap(),
            "2026-10-25T03:00:01.000Z"
        );
        assert_eq!(
            since_bound("2026-10-25T03:00:01Z").unwrap(),
            "2026-10-25T03:00:01.000Z"
        );
        assert_eq!(
            since_bound("2026-10-25T03:00:01.5Z").unwrap(),
            "2026-10-25T03:00:01.500Z"
        );
        assert_eq!(
            since_bound("2026-10-25T05:00+02:00").unwrap(),
            "2026-10-25T03:00:00.000Z"
        );
        assert_eq!(
            since_bound("2026-10-25").unwrap(),
            "2026-10-25T00:00:00.000Z"
        );
        assert!(since_bound("2026-13").is_none());

        let mut prev = chain::GENESIS_PREV.to_string();
        let mut body = Vec::new();
        for (seq, ts) in [
            (0, "2026-10-25T03:00:00.000Z"),
            (1, "2026-10-25T03:00:01.000Z"),
            (2, "2026-10-25T04:00:00.000Z"),
        ] {
            let event = if seq == 0 {
                ReceiptEvent::Genesis {
                    pulse: "echo".to_string(),
                }
            } else {
                ReceiptEvent::Shutdown {
                    reason: "stop".to_string(),
                }
            };
            let line = chain::canonical(
                &serde_json::to_value(chain::Row {
                    v: 1,
                    seq,
                    prev: prev.clone(),
                    ts: ts.to_string(),
                    e: serde_json::to_value(event).unwrap(),
                })
                .unwrap(),
            )
            .unwrap();
            prev = chain::sha256_hex(line.as_bytes());
            writeln!(&mut body, "{line}").unwrap();
        }
        let inspected = Inspected {
            bytes: body,
            report: chain::verify_lines(&b""[..]),
        };
        let offset = filtered_rows(&inspected, Some("2026-10-25T05:00:01+02:00"), None, None);
        assert_eq!(
            offset.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let bare = filtered_rows(&inspected, Some("2026-10-25T03:00:01Z"), None, None);
        assert_eq!(bare[0].ts, "2026-10-25T03:00:01.000Z");
        assert_eq!(bare.len(), 2);
    }
}
