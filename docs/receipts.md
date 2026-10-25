# Receipts

The daemon does not open a receipt file yet. `pulse-null receipts verify` and `pulse-null receipts show` read one. The store below is what a later slice opens.

## Row

One JSON object per line, UTF-8, at most 64 KiB not counting the newline. Lines end in `\n`.

```json
{"v":1,"seq":0,"prev":"<64 hex digits>","ts":"<RFC3339 UTC, millisecond, Z>","e":{"kind":"..."}}
```

`seq` starts at 0 and increases by 1. The first `prev` is 64 zeros. Each later `prev` is the lowercase hex SHA-256 of the previous row's canonical JSON. There is no `hash` field and no `sig` field. Numbers are integers. A float, a `sig` field, an unknown field, a version other than 1, or a line over 64 KiB is rejected.

`e.kind` is one of `genesis`, `boot`, `open`, `close`, `shutdown`, `recover`, `gap`. Unknown kinds and unknown fields are rejected. Optional fields that are absent are omitted; they are not written as null. `close` always carries `mailbox_head`, `folded`, and `fence`, each null, reserved for later.

A stored line must be exactly the canonical form. A torn line is a line that is not valid UTF-8 JSON, or a final line with no `\n`. The row right after it must be a matching `recover` (`torn_offset`, `torn_len`, `torn_sha256` of exactly those bytes), and the chain then warns; the torn line stays in the file when later rows follow. Without that row the chain is broken at the torn line. Any line that is valid JSON but not a valid row is broken.

## Store

`[receipts] path` is the file. When that key is absent or empty, the file is `/var/lib/pulse-null/<pulse>/receipts.jsonl`. `--file` wins. `verify` and `show` do not lock the file and do not repair it.

The store opens a systemd fd named `receipts` when `LISTEN_PID` is this process and the name is present. The fd must be a regular file open for append; otherwise it is refused. The store then sets `FD_CLOEXEC` and clears `LISTEN_PID`, `LISTEN_FDS`, and `LISTEN_FDNAMES`. When that fd was not passed in, the store opens the path instead (dev mode) and creates it if needed. A second open fails while the first holds the lock.

A crash while writing genesis leaves no complete row, and a recover row cannot fix that, because seq 0 has to be genesis. In path mode the store replaces such a file with a new genesis and logs it, and only when the file holds no complete row. In systemd-fd mode the store never truncates: it refuses to open, names the file, and says to rotate it. A later slice reports that refusal as degraded.

If the last line is a complete valid row missing only its `\n`, the store appends `\n` and writes no recover row. If the last line is torn (not valid UTF-8 JSON), the store appends `\n` when it is missing and then a `recover` row. A torn line that already has later rows stays where it is. A line that is valid JSON but not a valid row is not repaired.

An append that fails poisons the store. The next append that succeeds writes a `gap` row first (`dropped`, `first_at`, `last_at`) and then the new row. The daemon wires that up in a later slice.

Protection is five checks, all required: the fd came from systemd, the owner is uid 0, the mode is not group- or world-writable, the append-only flag is set, and the parent directory is owned by uid 0 and not group- or world-writable. A path open fails the systemd check, so it is not protected. That is reported, not refused, until a later slice.

`verify` prints the file it checked, whether that path came from `--file`, `[receipts] path`, or the default, and those five checks. A `[receipts] path` the pulse can write can point `verify` at a decoy file. The printed path, where it came from, and the protection checks are how an operator notices. Production verification should use `--file` on the root-owned path.

`verify` exits 0 for ok or warn, and exits 1 naming the first broken line. `--json` prints `state`, `rows`, `seq`, `head`, `warns`, `break`, and `orphans`. `show` prints matching rows. `--since` keeps a `ts` at least that string, `--kind` matches `e.kind`, and `--limit` keeps the latest N.

## Canonical form

A stored line is exactly this form, and the hash is SHA-256 of those bytes:

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
