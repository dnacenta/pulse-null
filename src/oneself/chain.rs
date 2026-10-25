//! A receipt file is one JSON object per line, each hashed into the next.
//!
//! `hash = SHA-256(canonical(row))`. The first row's `prev` is 64 zeros.
//! Every later `prev` is the previous row's hash. A torn line is not a row;
//! the next row must be a `recover` that names those bytes, or the chain is
//! broken.

use std::io::BufRead;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::event::ReceiptEvent;

/// Longest raw line, not counting its trailing newline.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// `prev` of the first row: 64 zero hex digits.
pub const GENESIS_PREV: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// One receipt row. `e` is the event object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub v: u32,
    pub seq: u64,
    pub prev: String,
    pub ts: String,
    pub e: Value,
}

/// Why a row could not be hashed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    /// A JSON number that is not an integer.
    #[error("floating-point numbers are rejected")]
    Float,
    /// The row could not be turned into JSON.
    #[error("row could not be serialised")]
    Row,
}

/// One warning or the first broken line. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyNote {
    pub line: usize,
    pub reason: &'static str,
}

/// The chain check. `head` is the last accepted row's hash, or
/// [`GENESIS_PREV`] when no row was accepted. `seq` is that row's `seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub rows: u64,
    pub seq: Option<u64>,
    pub head: String,
    pub warns: Vec<VerifyNote>,
    pub first_break: Option<VerifyNote>,
}

/// OK has no warnings. WARN recovered a torn line. BROKEN stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyState {
    Ok,
    Warn,
    Broken,
}

impl VerifyReport {
    #[must_use]
    pub fn state(&self) -> VerifyState {
        if self.first_break.is_some() {
            VerifyState::Broken
        } else if self.warns.is_empty() {
            VerifyState::Ok
        } else {
            VerifyState::Warn
        }
    }

    fn empty() -> Self {
        Self {
            rows: 0,
            seq: None,
            head: GENESIS_PREV.to_string(),
            warns: Vec::new(),
            first_break: None,
        }
    }
}

/// Canonical JSON: object keys sorted by their UTF-8 bytes at every level,
/// no whitespace, strings escaped the way `serde_json` escapes them.
/// Integers only; a float is [`ChainError::Float`].
pub fn canonical(value: &Value) -> Result<String, ChainError> {
    let mut out = String::new();
    write_sorted(value, &mut out)?;
    Ok(out)
}

/// SHA-256 of [`canonical`] for `row`.
pub fn row_hash(row: &Row) -> Result<[u8; 32], ChainError> {
    let value = serde_json::to_value(row).map_err(|_| ChainError::Row)?;
    let text = canonical(&value)?;
    Ok(Sha256::digest(text.as_bytes()).into())
}

/// Read a receipt file and check version, `seq`, `prev`, size, `sig`,
/// floats, and that a torn line is followed by a matching `recover` row.
pub fn verify_lines<R: BufRead>(mut reader: R) -> VerifyReport {
    let mut buf = Vec::new();
    if reader.read_to_end(&mut buf).is_err() {
        let mut report = VerifyReport::empty();
        report.first_break = Some(VerifyNote {
            line: 1,
            reason: "read",
        });
        return report;
    }
    verify_bytes(&buf)
}

struct Phys<'a> {
    number: usize,
    offset: u64,
    bytes: &'a [u8],
    has_newline: bool,
}

struct Accepted {
    seq: u64,
    hash: String,
    event: ReceiptEvent,
}

fn verify_bytes(buf: &[u8]) -> VerifyReport {
    let lines = split_lines(buf);
    let mut report = VerifyReport::empty();
    let mut expected_seq = Some(0u64);
    let mut expected_prev = GENESIS_PREV.to_string();
    let mut index = 0;
    while index < lines.len() {
        let line = &lines[index];
        if line.bytes.len() > MAX_LINE_BYTES {
            report.first_break = Some(VerifyNote {
                line: line.number,
                reason: "oversize",
            });
            return report;
        }
        let Some(seq) = expected_seq else {
            report.first_break = Some(VerifyNote {
                line: line.number,
                reason: "seq",
            });
            return report;
        };
        if index + 1 < lines.len() {
            if let Some(accepted) = matching_recover(&lines[index + 1], line, seq, &expected_prev) {
                report.warns.push(VerifyNote {
                    line: line.number,
                    reason: "torn line",
                });
                accept(&mut report, &mut expected_seq, &mut expected_prev, accepted);
                index += 2;
                continue;
            }
        }
        if torn_payload(line) {
            report.first_break = Some(VerifyNote {
                line: line.number,
                reason: "torn line",
            });
            return report;
        }
        match check_row(line, seq, &expected_prev) {
            Ok(accepted) => {
                accept(&mut report, &mut expected_seq, &mut expected_prev, accepted);
                index += 1;
            }
            Err(reason) => {
                report.first_break = Some(VerifyNote {
                    line: line.number,
                    reason,
                });
                return report;
            }
        }
    }
    report
}

fn accept(
    report: &mut VerifyReport,
    expected_seq: &mut Option<u64>,
    expected_prev: &mut String,
    accepted: Accepted,
) {
    report.rows += 1;
    report.seq = Some(accepted.seq);
    report.head.clone_from(&accepted.hash);
    *expected_prev = accepted.hash;
    *expected_seq = accepted.seq.checked_add(1);
}

fn matching_recover(
    next: &Phys<'_>,
    torn: &Phys<'_>,
    expected_seq: u64,
    expected_prev: &str,
) -> Option<Accepted> {
    if !next.has_newline || next.bytes.len() > MAX_LINE_BYTES {
        return None;
    }
    let accepted = check_row(next, expected_seq, expected_prev).ok()?;
    match &accepted.event {
        ReceiptEvent::Recover {
            torn_offset,
            torn_len,
            torn_sha256,
        } => {
            let len = u64::try_from(torn.bytes.len()).ok()?;
            if *torn_offset == torn.offset
                && *torn_len == len
                && torn_sha256 == &sha256_hex(torn.bytes)
            {
                Some(accepted)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A line the writer would treat as torn: it does not parse as JSON, or it
/// is the last line and the file has no trailing newline.
fn torn_payload(line: &Phys<'_>) -> bool {
    if !line.has_newline {
        return true;
    }
    let Ok(text) = std::str::from_utf8(line.bytes) else {
        return true;
    };
    serde_json::from_str::<Value>(text).is_err()
}

fn check_row(
    line: &Phys<'_>,
    expected_seq: u64,
    expected_prev: &str,
) -> Result<Accepted, &'static str> {
    let text = std::str::from_utf8(line.bytes).map_err(|_| "not a row")?;
    let value: Value = serde_json::from_str(text).map_err(|_| "not a row")?;
    if !value.is_object() {
        return Err("not a row");
    }
    if has_sig(&value) {
        return Err("sig");
    }
    if contains_float(&value) {
        return Err("float");
    }
    let row: Row = serde_json::from_value(value).map_err(|err| schema_reason(&err))?;
    if row.v != 1 {
        return Err("bad version");
    }
    if row.seq != expected_seq {
        return Err("seq");
    }
    if row.prev != expected_prev {
        return Err("prev");
    }
    let event: ReceiptEvent =
        serde_json::from_value(row.e.clone()).map_err(|err| schema_reason(&err))?;
    // The stored bytes are what a reader sees. They must be the canonical
    // form, so a duplicate key cannot show one value and hash another.
    stored_line_is_canonical(text)?;
    let hash = match row_hash(&row) {
        Ok(hash) => hash,
        Err(ChainError::Float) => return Err("float"),
        Err(ChainError::Row) => return Err("not a row"),
    };
    Ok(Accepted {
        seq: row.seq,
        hash: hex(&hash),
        event,
    })
}

/// Reject a line whose bytes are not `canonical` of its JSON value.
///
/// `serde_json` keeps the last of a duplicate key. Canonical output has one
/// copy of each key and no extra whitespace, so a duplicate-key line, a
/// reordered line, or a `\u` escape cannot compare equal.
fn stored_line_is_canonical(text: &str) -> Result<(), &'static str> {
    let parsed: Value = serde_json::from_str(text).map_err(|_| "not a row")?;
    let canon = match canonical(&parsed) {
        Ok(canon) => canon,
        Err(ChainError::Float) => return Err("float"),
        Err(ChainError::Row) => return Err("not a row"),
    };
    if canon == text {
        Ok(())
    } else {
        Err("not canonical")
    }
}

fn schema_reason(err: &serde_json::Error) -> &'static str {
    let msg = err.to_string();
    if msg.contains("unknown field") || msg.contains("unknown variant") {
        "unknown field"
    } else {
        "not a row"
    }
}

fn has_sig(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.contains_key("sig") || map.values().any(has_sig),
        Value::Array(items) => items.iter().any(has_sig),
        _ => false,
    }
}

fn contains_float(value: &Value) -> bool {
    match value {
        Value::Number(n) => is_float(n),
        Value::Object(map) => map.values().any(contains_float),
        Value::Array(items) => items.iter().any(contains_float),
        _ => false,
    }
}

/// `serde_json` prints an integer without a point or an exponent, and a
/// float with one of them, including a whole number that was written `1.0`.
fn is_float(n: &serde_json::Number) -> bool {
    let text = n.to_string();
    text.contains('.') || text.contains('e') || text.contains('E')
}

fn write_sorted(value: &Value, out: &mut String) -> Result<(), ChainError> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_sorted(&map[*key], out)?;
            }
            out.push('}');
            Ok(())
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_sorted(item, out)?;
            }
            out.push(']');
            Ok(())
        }
        Value::Number(n) if is_float(n) => Err(ChainError::Float),
        other => {
            out.push_str(&other.to_string());
            Ok(())
        }
    }
}

fn split_lines(buf: &[u8]) -> Vec<Phys<'_>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (index, byte) in buf.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(Phys {
                number: lines.len() + 1,
                offset: u64::try_from(start).unwrap_or(u64::MAX),
                bytes: &buf[start..index],
                has_newline: true,
            });
            start = index + 1;
        }
    }
    if start < buf.len() {
        lines.push(Phys {
            number: lines.len() + 1,
            offset: u64::try_from(start).unwrap_or(u64::MAX),
            bytes: &buf[start..],
            has_newline: false,
        });
    }
    lines
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oneself::event::ReceiptEvent;

    const TS0: &str = "2026-10-25T03:00:00.000Z";
    const TS1: &str = "2026-10-25T03:00:00.001Z";
    const TS2: &str = "2026-10-25T03:00:00.002Z";
    const TS3: &str = "2026-10-25T03:00:00.003Z";

    fn line_of(seq: u64, prev: &str, ts: &str, event: &ReceiptEvent) -> (String, String) {
        let row = Row {
            v: 1,
            seq,
            prev: prev.to_string(),
            ts: ts.to_string(),
            e: serde_json::to_value(event).unwrap(),
        };
        let value = serde_json::to_value(&row).unwrap();
        let canon = canonical(&value).unwrap();
        let hash = hex(&row_hash(&row).unwrap());
        (canon, hash)
    }

    fn chain_of(events: &[(&str, ReceiptEvent)]) -> (String, Vec<String>) {
        let mut prev = GENESIS_PREV.to_string();
        let mut body = String::new();
        let mut hashes = Vec::new();
        for (seq, (ts, event)) in events.iter().enumerate() {
            let (canon, hash) = line_of(seq as u64, &prev, ts, event);
            body.push_str(&canon);
            body.push('\n');
            prev = hash.clone();
            hashes.push(hash);
        }
        (body, hashes)
    }

    fn three() -> String {
        chain_of(&[
            (
                TS0,
                ReceiptEvent::Genesis {
                    pulse: "echo".to_string(),
                },
            ),
            (
                TS1,
                ReceiptEvent::Gap {
                    dropped: 1,
                    first_at: TS0.to_string(),
                    last_at: TS1.to_string(),
                },
            ),
            (
                TS2,
                ReceiptEvent::Shutdown {
                    reason: "stop".to_string(),
                },
            ),
        ])
        .0
    }

    fn note(report: &VerifyReport) -> (usize, &'static str) {
        let break_at = report.first_break.as_ref().expect("broken");
        (break_at.line, break_at.reason)
    }

    #[test]
    fn vectors_v1_reproduce_pinned_hashes() {
        const VECTORS: &str = include_str!("../../tests/fixtures/receipts/v1-vectors.jsonl");
        const EXPECTED: &str = include_str!("../../tests/fixtures/receipts/v1-vectors.expected");
        let report = verify_lines(VECTORS.as_bytes());
        assert_eq!(report.state(), VerifyState::Warn, "{report:?}");
        assert!(report.first_break.is_none());
        let mut pinned = Vec::new();
        for line in EXPECTED.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (hash, canon) = line.split_once(' ').expect("hash and canonical");
            pinned.push((hash, canon));
        }
        let mut got = Vec::new();
        for line in VECTORS.lines() {
            let Ok(row) = serde_json::from_str::<Row>(line) else {
                continue;
            };
            let value = serde_json::to_value(&row).unwrap();
            let canon = canonical(&value).unwrap();
            let hash = hex(&row_hash(&row).unwrap());
            let again = hex(&Sha256::digest(canon.as_bytes()));
            assert_eq!(again, hash);
            got.push((hash, canon));
        }
        assert_eq!(got.len(), pinned.len());
        for ((hash, canon), (want_hash, want_canon)) in got.iter().zip(&pinned) {
            assert_eq!(canon, want_canon);
            assert_eq!(hash, want_hash);
        }
        assert_eq!(report.rows, got.len() as u64);
        assert_eq!(report.head, got.last().unwrap().0);
    }

    #[test]
    fn canonical_is_key_order_independent() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"d":2,"c":3}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"c":3,"d":2},"b":1}"#).unwrap();
        assert_eq!(canonical(&a).unwrap(), canonical(&b).unwrap());
        assert_eq!(canonical(&a).unwrap(), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    #[test]
    fn canonical_matches_vigil_probe_record_on_shared_inputs() {
        let inputs = [
            serde_json::json!({"b": 1, "a": {"d": 2, "c": 3}}),
            serde_json::json!({"a": {"c": 3, "d": 2}, "b": 1}),
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(false),
            serde_json::json!([1, "x", false, null]),
            serde_json::json!({"s": "a\"b\\c\n"}),
            serde_json::json!({"n": 0}),
            serde_json::json!({"n": -1}),
            serde_json::json!({"empty": {}}),
            serde_json::json!({"arr": []}),
            serde_json::json!({"s": "é"}),
        ];
        for value in inputs {
            let ours = canonical(&value).unwrap();
            let theirs = crate::vigil::probe::record::canonical_json(&value);
            assert_eq!(ours, theirs);
        }
    }

    #[test]
    fn an_edited_row_is_named() {
        let clean = three();
        assert_eq!(verify_lines(clean.as_bytes()).state(), VerifyState::Ok);
        let edited = clean.replacen("\"dropped\":1", "\"dropped\":9", 1);
        let report = verify_lines(edited.as_bytes());
        assert_eq!(note(&report), (3, "prev"));
    }

    #[test]
    fn an_inserted_row_is_named() {
        let (body, _) = chain_of(&[
            (
                TS0,
                ReceiptEvent::Genesis {
                    pulse: "echo".to_string(),
                },
            ),
            (
                TS2,
                ReceiptEvent::Shutdown {
                    reason: "stop".to_string(),
                },
            ),
        ]);
        let (extra, _) = line_of(
            7,
            GENESIS_PREV,
            TS1,
            &ReceiptEvent::Shutdown {
                reason: "x".to_string(),
            },
        );
        let mut lines: Vec<&str> = body.lines().collect();
        lines.insert(1, extra.as_str());
        let text = lines.join("\n") + "\n";
        let report = verify_lines(text.as_bytes());
        assert_eq!(note(&report), (2, "seq"));
    }

    #[test]
    fn a_deleted_row_is_named() {
        let mut lines: Vec<String> = three().lines().map(str::to_string).collect();
        lines.remove(1);
        let text = lines.join("\n") + "\n";
        let report = verify_lines(text.as_bytes());
        // The row that followed now sits at line 2 with the old seq.
        assert_eq!(note(&report), (2, "seq"));
    }

    #[test]
    fn a_reordered_row_is_named() {
        let mut lines: Vec<String> = three().lines().map(str::to_string).collect();
        lines.swap(1, 2);
        let text = lines.join("\n") + "\n";
        let report = verify_lines(text.as_bytes());
        // The later row is now line 2, so its seq is no longer the next one.
        assert_eq!(note(&report), (2, "seq"));
    }

    #[test]
    fn a_seq_gap_is_named() {
        let mut lines: Vec<String> = three().lines().map(str::to_string).collect();
        let mut value: Value = serde_json::from_str(&lines[1]).unwrap();
        value["seq"] = serde_json::json!(4);
        lines[1] = canonical(&value).unwrap();
        let text = lines.join("\n") + "\n";
        let report = verify_lines(text.as_bytes());
        assert_eq!(note(&report), (2, "seq"));
    }

    #[test]
    fn a_float_is_rejected() {
        let whole: Value = serde_json::from_str("1").unwrap();
        assert_eq!(canonical(&whole).unwrap(), "1");
        let frac: Value = serde_json::from_str("1.5").unwrap();
        assert_eq!(canonical(&frac), Err(ChainError::Float));
        let one: Value = serde_json::from_str("1.0").unwrap();
        assert_eq!(canonical(&one), Err(ChainError::Float));
        let (line, _) = line_of(
            0,
            GENESIS_PREV,
            TS0,
            &ReceiptEvent::Shutdown {
                reason: "x".to_string(),
            },
        );
        let bad = line.replacen("\"v\":1", "\"v\":1.0", 1);
        let report = verify_lines(format!("{bad}\n").as_bytes());
        assert_eq!(note(&report), (1, "float"));
    }

    #[test]
    fn an_unknown_field_is_rejected() {
        let (line, _) = line_of(
            0,
            GENESIS_PREV,
            TS0,
            &ReceiptEvent::Shutdown {
                reason: "x".to_string(),
            },
        );
        let bad = line.replacen("\"reason\":\"x\"", "\"reason\":\"x\",\"extra\":true", 1);
        let report = verify_lines(format!("{bad}\n").as_bytes());
        assert_eq!(note(&report), (1, "unknown field"));
    }

    #[test]
    fn a_sig_is_rejected() {
        let (line, _) = line_of(
            0,
            GENESIS_PREV,
            TS0,
            &ReceiptEvent::Shutdown {
                reason: "x".to_string(),
            },
        );
        let bad = line.replacen("\"seq\":0", "\"seq\":0,\"sig\":\"aa\"", 1);
        let report = verify_lines(format!("{bad}\n").as_bytes());
        assert_eq!(note(&report), (1, "sig"));
    }

    #[test]
    fn an_oversize_line_is_rejected() {
        let mut buf = vec![b'x'; MAX_LINE_BYTES + 1];
        buf.push(b'\n');
        let report = verify_lines(buf.as_slice());
        assert_eq!(note(&report), (1, "oversize"));
    }

    #[test]
    fn a_bad_version_is_rejected() {
        let (line, _) = line_of(
            0,
            GENESIS_PREV,
            TS0,
            &ReceiptEvent::Shutdown {
                reason: "x".to_string(),
            },
        );
        let bad = line.replacen("\"v\":1", "\"v\":2", 1);
        let report = verify_lines(format!("{bad}\n").as_bytes());
        assert_eq!(note(&report), (1, "bad version"));
    }

    #[test]
    fn a_torn_line_without_recover_is_broken() {
        let mut text = three();
        text.push_str("{\"e\":{\"kind\":\"clo\"\n");
        let report = verify_lines(text.as_bytes());
        assert_eq!(report.state(), VerifyState::Broken);
        assert_eq!(note(&report), (4, "torn line"));
        assert!(report.warns.is_empty());

        // A final row with no newline is torn even when the bytes parse.
        let mut cut = three();
        assert!(cut.ends_with('\n'));
        cut.pop();
        let report = verify_lines(cut.as_bytes());
        assert_eq!(report.state(), VerifyState::Broken);
        assert_eq!(note(&report), (3, "torn line"));
    }

    #[test]
    fn a_torn_line_with_recover_is_a_warning() {
        let (mut text, hashes) = chain_of(&[
            (
                TS0,
                ReceiptEvent::Genesis {
                    pulse: "echo".to_string(),
                },
            ),
            (
                TS1,
                ReceiptEvent::Gap {
                    dropped: 1,
                    first_at: TS0.to_string(),
                    last_at: TS1.to_string(),
                },
            ),
            (
                TS2,
                ReceiptEvent::Shutdown {
                    reason: "stop".to_string(),
                },
            ),
        ]);
        let torn = b"{\"e\":{\"kind\":\"clo\"";
        let offset = u64::try_from(text.len()).unwrap();
        text.push_str(std::str::from_utf8(torn).unwrap());
        text.push('\n');
        let (recover, _) = line_of(
            3,
            hashes.last().unwrap(),
            TS3,
            &ReceiptEvent::Recover {
                torn_offset: offset,
                torn_len: u64::try_from(torn.len()).unwrap(),
                torn_sha256: sha256_hex(torn),
            },
        );
        text.push_str(&recover);
        text.push('\n');
        let report = verify_lines(text.as_bytes());
        assert_eq!(report.state(), VerifyState::Warn, "{report:?}");
        assert!(report.first_break.is_none());
        assert_eq!(report.warns.len(), 1);
        assert_eq!(report.warns[0].line, 4);
        assert_eq!(report.warns[0].reason, "torn line");
        assert_eq!(report.rows, 4);
        assert_eq!(report.seq, Some(3));
    }

    fn shutdown_after_genesis() -> (String, String) {
        let (genesis, prev) = line_of(
            0,
            GENESIS_PREV,
            TS0,
            &ReceiptEvent::Genesis {
                pulse: "echo".to_string(),
            },
        );
        let (row, _) = line_of(
            1,
            &prev,
            TS1,
            &ReceiptEvent::Shutdown {
                reason: "stop".to_string(),
            },
        );
        (genesis, row)
    }

    #[test]
    fn duplicate_keys_are_not_canonical() {
        let (genesis, row) = shutdown_after_genesis();
        let kind = row.replace(
            "\"kind\":\"shutdown\"",
            "\"kind\":\"genesis\",\"kind\":\"shutdown\"",
        );
        let seq = row.replace("\"seq\":1", "\"seq\":99,\"seq\":1");
        for line in [kind, seq] {
            let text = format!("{genesis}\n{line}\n");
            let report = verify_lines(text.as_bytes());
            assert_eq!(note(&report), (2, "not canonical"));
        }
    }

    #[test]
    fn a_non_canonical_line_is_rejected() {
        let (genesis, row) = shutdown_after_genesis();
        let spaced = row.replace(',', ", ");
        let escaped = row.replace("\"reason\":\"stop\"", "\"reason\":\"\\u0073top\"");
        let crlf = format!("{row}\r");
        let trailed = format!("{row} ");
        let parsed: Value = serde_json::from_str(&row).unwrap();
        let prev = parsed["prev"].as_str().unwrap().to_string();
        let reordered = format!(
            "{{\"v\":1,\"ts\":\"{TS1}\",\"seq\":1,\"prev\":\"{prev}\",\"e\":{{\"reason\":\"stop\",\"kind\":\"shutdown\"}}}}"
        );
        for line in [spaced, escaped, crlf, trailed, reordered] {
            let text = format!("{genesis}\n{line}\n");
            let report = verify_lines(text.as_bytes());
            assert_eq!(note(&report), (2, "not canonical"), "{line}");
        }
    }
}
