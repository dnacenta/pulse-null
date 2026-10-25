# Receipts

Not wired. The daemon, the CLI, and config do not read or write a receipt file yet. This page is the row format and the pinned vectors.

## Row

One JSON object per line, UTF-8, at most 64 KiB not counting the newline. Lines end in `\n`.

```json
{"v":1,"seq":0,"prev":"<64 hex digits>","ts":"<RFC3339 UTC, millisecond, Z>","e":{"kind":"..."}}
```

`seq` starts at 0 and increases by 1. The first `prev` is 64 zeros. Each later `prev` is the lowercase hex SHA-256 of the previous row's canonical JSON. There is no `hash` field and no `sig` field. Numbers are integers. A float, a `sig` field, an unknown field, a version other than 1, or a line over 64 KiB is rejected.

`e.kind` is one of `genesis`, `boot`, `open`, `close`, `shutdown`, `recover`, `gap`. Unknown kinds and unknown fields are rejected. Optional fields that are absent are omitted; they are not written as null. `close` always carries `mailbox_head`, `folded`, and `fence`, each null, reserved for later.

A line that is not a JSON value, or a final line with no newline, is torn. It is not a row. The next row must be `recover` with `torn_offset` (byte offset of those bytes), `torn_len` (their length), and `torn_sha256` (their SHA-256). The chain then warns. Without that matching row, the chain is broken at the torn line.

## Canonical form

The hash is over canonical JSON, not over the raw line:

- object keys sorted by UTF-8 byte order at every level
- no whitespace
- strings escaped the way `serde_json` escapes them
- integers only

The same rules as the probe record's canonical JSON. A float is an error here; the probe record is not modified.

## Vectors

`tests/fixtures/receipts/v1-vectors.jsonl` is a genesis, a boot, an open, a close, a torn line, and the `recover` row for that torn line. `tests/fixtures/receipts/v1-vectors.expected` lists each accepted row as `<hex hash> <canonical json>`. The torn line is not listed.

The first row's canonical JSON is:

```text
{"e":{"kind":"genesis","pulse":"echo"},"prev":"0000000000000000000000000000000000000000000000000000000000000000","seq":0,"ts":"2026-10-25T03:00:00.000Z","v":1}
```

Its hash is `8fa0147ad6e4235d2938de010cd334e48fbd6891ec413d73e66bbd57a6ea33c8`. Recompute it with:

```sh
printf '%s' '{"e":{"kind":"genesis","pulse":"echo"},"prev":"0000000000000000000000000000000000000000000000000000000000000000","seq":0,"ts":"2026-10-25T03:00:00.000Z","v":1}' | sha256sum
```
