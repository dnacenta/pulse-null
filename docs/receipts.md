# Receipts

The daemon opens a receipt file when receipts are on. They are off unless `[receipts] enabled` is true or `PULSE_NULL_RECEIPTS=required`. `pulse-null receipts verify`, `show`, `overlaps`, and `swaps` read a file and do not write one. `pulse-null receipts anchor` is a separate command. The daemon never runs it.

## Row

One JSON object per line, UTF-8, at most 64 KiB not counting the newline. Lines end in `\n`.

```json
{"v":1,"seq":0,"prev":"<64 hex digits>","ts":"<RFC3339 UTC, millisecond, Z>","e":{"kind":"..."}}
```

`seq` starts at 0 and increases by 1. The first `prev` is 64 zeros. Each later `prev` is the lowercase hex SHA-256 of the previous row's canonical JSON. There is no `hash` field and no `sig` field. Numbers are integers. A float, a `sig` field, an unknown field, a version other than 1, or a line over 64 KiB is rejected.

`e.kind` is one of `genesis`, `boot`, `open`, `close`, `shutdown`, `recover`, `gap`. Unknown kinds and unknown fields are rejected. Optional fields that are absent are omitted; they are not written as null. `close` always carries `mailbox_head`, `folded`, and `fence`, each null, reserved for later.

A stored line must be exactly the canonical form. A torn line is a line that is not valid UTF-8 JSON, or a final line with no `\n`. The row right after it must be a matching `recover` (`torn_offset`, `torn_len`, `torn_sha256` of exactly those bytes), and the chain then warns; the torn line stays in the file when later rows follow. Without that row the chain is broken at the torn line. Any line that is valid JSON but not a valid row is broken.

## Store

`[receipts] path` is the file the daemon opens when receipts are on and protection is not required. When that key is absent or empty, the file is `/var/lib/pulse-null/<pulse>/receipts.jsonl`. `--file` wins for `verify` and `show`. Those commands do not lock the file and do not repair it.

`PULSE_NULL_RECEIPTS=required` forces receipts on and protection required. The pulse-writable file cannot turn that off, and `[receipts] path` is not opened. Any other non-empty value of that variable is ignored. With `require_protected = true` and no systemd fd, the daemon serves and writes nothing.

The daemon honours `[receipts] path` only when `require_protected` is false and the variable is not `required`, and only when the path resolves under the pulse directory or `/var/lib/pulse-null/<pulse>/`. The parent is canonicalised, so a symlink that leaves those directories is refused. A `..` component is refused. A symlink at the file itself is refused. The parent must already exist; a refused path creates nothing. That requirement is the daemon's confine check. The store creates a missing directory only while opening a path, with `mkdirat` on a directory it has already opened, and it does not create a parent the daemon refused.

The process reads `LISTEN_PID`, `LISTEN_FDS`, and `LISTEN_FDNAMES` once, before the runtime starts, and the store takes that snapshot. It uses a systemd fd named `receipts` only when the pid is this process and the name is present. One pulse adopts that fd. Another pulse in the same process logs that the fd is already in use and writes nothing. `FD_CLOEXEC` is set before the fd is checked. The fd must be a regular file open for reading and writing and for append. Any other fd is closed and refused. When that name was not passed in, open returns not-used and the caller chooses the path. A path open is dev mode. It walks every component from `/` with `openat`, `O_PATH`, `O_DIRECTORY`, and `O_NOFOLLOW`, so a directory that can be searched but not listed still opens, and a symlink anywhere on the path is refused and nothing is created through it. A component that is a regular file is the same refusal, and `/health` reports `unavailable`. A missing directory is created with `mkdirat` on the directory already opened, mode `0700`. The file is opened with `openat` and `O_NOFOLLOW`. A file with more than one hard link is refused and left unchanged. A file over 256 MiB is refused. A second open fails while the first holds the lock.

A non-empty file that does not start with this pulse's genesis row is left byte for byte as it was, including a bit-flipped, non-canonical, or BOM-prefixed first line. The daemon logs that, serves, and writes nothing. An empty path file gets a genesis row. Systemd-fd mode never truncates. An empty systemd fd, or a first line that is not this pulse's genesis, is refused: the message names the file and says to rotate it. A complete genesis with no newline gets that newline and opens. A damaged line after a valid genesis is broken at that line, and the file stays as it is.

A row and its newline are one write, then `sync_all`. If the file is valid once a missing `\n` is added, the store appends that `\n` and writes no recover row. If the last line is torn (not valid UTF-8 JSON), the store appends a newline when it is missing and then a `recover` row, as one write when both are needed. A torn line that already has later rows stays where it is. A line that is valid JSON but not a valid row is not repaired.

An append that fails poisons the store. The next append re-reads the tail and repairs it the same way open does, then writes a `gap` row (`dropped`, `first_at`, `last_at`) and the new row. A row that actually landed is not counted in `dropped`. The store does not truncate on that path. The daemon queues rows on a bounded channel and writes that gap when the channel overflowed or an append failed.

Protection is five checks, all required: the fd came from systemd, the owner is uid 0, the mode is not group- or world-writable, the append-only flag is set, and the parent directory is owned by uid 0 and not group- or world-writable. A path open fails the systemd check, so it is not protected. When protection is required and the file is not protected, the daemon serves and writes nothing. `/health` reports `receipts.state` as `off`, `ok`, `degraded`, `unprotected`, or `unavailable`, and `receipts.seq` when the writer is appending.

The boot row's identity hashes `SELF.md`, `pulse-null.toml`, and the files under `[pulse] rules_dir`. A symlink to a regular file is followed. A fifo is skipped so boot does not block. A file that cannot be read stays in the hash as unreadable and does not drop the rest. The reads run off the runtime's worker.

`verify` prints the file it checked, whether that path came from `--file`, `[receipts] path`, or the default, and those five checks. A `[receipts] path` the pulse can write can point `verify` at a decoy file. The printed path, where it came from, and the protection checks are how an operator notices. Production verification should use `--file` on the root-owned path.

`verify` exits 0 for ok or warn, exits 1 naming the first broken line, and exits 2 when the file cannot be checked (missing, not a regular file, unreadable, larger than 256 MiB, or no config). An empty file is warn `empty` with zero rows, exit 0. `--json` prints `state`, `file`, `source`, `protection`, `path_warn`, `rows`, `seq`, `head`, `warns`, `break`, and `orphans`. `--anchors` adds an `anchors` array; see [Anchors](#anchors). An unverified error is `{"state":"unverified","error":"..."}`. `show` prints matching rows, with control characters and bidi overrides escaped as `\u{...}` in the human output. `--since` is RFC3339 or a prefix of `YYYY-MM-DDTHH:MM:SS.mmmZ`. A prefix is compared as that prefix padded with zeros, so it matches every stored instant that starts with it, and it is accepted only when some real instant starts with it. `--kind` is a known kind, and `--limit` keeps the latest N. A bad `--since` or `--kind` exits 2.

## Streams

A chat turn's principal is `owner`, `peer:<name>`, or `guest`. A scheduled task uses `task` and may set `task_id`. An intent uses `intent`. A farm writes one open with principal `farm`, and each subtask writes its own `farm` open whose `parent` is that farm open's stream id. A local comms turn uses `comms`.

Failure closes `error`. Isolation closes `isolated`. Dropping the future closes `aborted`. A success that has reply text closes `acted`. A success with no reply text closes `silent`. A close records `model` only when the response named one. An error, an abort, or a lost open omits it.

Shutdown stops every pulse's server from accepting and waits one drain window for all of them together. Dropping a server task after that window stops its accept loop and leaves a connection task that is still running. A turn whose close was not queued is reconciled as `close{lost}` on the next boot. The shutdown row is written after the flush.

A refusal before a turn is admitted writes no row. That includes HTTP 413 when the message is too long. There is no `plugin:<name>` principal.

## Overlaps and swaps

`pulse-null receipts overlaps [--file P] [--since T] [--limit N] [--json]` lists open/close intervals on different streams that intersect. An open with no later close is not an interval. A turn covers chain rows from its open up to, but not including, its close, so a close and a later open do not overlap, including when they share a millisecond. The order is the row sequence, so a clock step that puts a close timestamp before its open does not hide an overlap. One stream is not an overlap with itself. `quiet_twin` is true when one close is `acted` and the other is not. `--since` drops an overlap that ended before that instant. `--limit` defaults to 1000 and `0` lists every overlap. Past the limit, human output ends with `truncated, N more not shown` and `--json` sets `truncated` to that count. The command uses the verified chain only: a broken file exits 1 and names the line, and a file that cannot be read exits 2. Human lines escape control characters and bidi overrides the same way `show` does.

`pulse-null receipts swaps [--file P] [--json]` lists consecutive `boot` rows, and consecutive `close` rows, whose provider, adapter, or model differ. A close with no `model` is skipped. A boot pair also says whether the `self_md`, `rules`, and `config` digests were carried (`same` or `changed`). Both say the chain state and how many rows sit between the pair. Identical consecutive rows are not listed. A broken file exits 1.

## Anchors

`pulse-null receipts anchor --pulse <name> [--file P] [--config C] [--token-file T] [--dry-run]` checks the local chain, then each backend. It runs only as this command. The daemon does not call it, so the token never sits in the pulse process.

The anchor config is `/etc/pulse-null/anchor.toml`, not `pulse-null.toml`. That file is refused unless it is owned by root and is not group- or world-writable. `--config` is the override for a test file: the owner check is skipped, and a group- or world-writable file is still refused. Each `[[backend]]` is `kind = "local"` or `kind = "github"` with `repo` (`owner/name`), `branch` (default `main`), `dir` (default `heads`), `token_file`, and `api_base` (default `https://api.github.com`). `api_base` must be `https`, except `http://127.0.0.1` and `http://localhost` for a local stand-in.

The token comes from `--token-file`, then the backend's `token_file`, then `PULSE_NULL_ANCHOR_TOKEN_FILE`, then `$CREDENTIALS_DIRECTORY/anchor-token`, then `PULSE_NULL_ANCHOR_TOKEN`. A file that is group- or world-readable is refused. The value is not printed, not put in a URL, and not written into an error.

`anchor` keeps only the bytes up to the last newline, so a torn tail is not a row. It checks that chain. A broken prefix exits 1 and publishes nothing. `--pulse` must match the genesis row, or the command exits 2. It then reads each backend. Every anchored `(seq, head)` for this pulse must match the local chain. A different head, pulse, or ledger id is `mismatch`. A seq past the local chain is `rewind`. Either one exits 1 and publishes nothing.

When the local seq is ahead of the last anchored seq, one line is appended to each backend that is behind:

```text
v1 <pulse> <ledger_id> <seq> <head> <ts>
```

`ledger_id` is the first 16 hex digits of the genesis hash. `head` is the hash of the last complete row. `ts` is that row's timestamp. `--dry-run` prints the line and writes nothing. A later run with no new row writes nothing.

The local backend appends `<receipt dir>/anchors.log` and calls `sync_all`. The GitHub backend uses the contents API on `heads/<pulse>/<YYYY>.log` (the `dir` key replaces `heads`). It reads the previous object, appends the line, and sends the object's sha. A 409 reads the object again and retries once. A 404 on the read creates the file. History for a year also reads the previous year. A 401, 403, 404, 422, or 5xx exits 2 with the status and no token text. The request sends `Authorization: Bearer`, `X-GitHub-Api-Version: 2026-03-10`, and `User-Agent: pulse-null/<version>`.

`pulse-null receipts verify --anchors [--config C] [--token-file T]` does the same comparison and does not publish. States are `ok`, `warn`, `broken`, `mismatch`, `rewind`, and `unverified`. Exit 0 is ok or warn. Exit 1 is broken, mismatch, or rewind. Exit 2 is unverified or a usage error. Without `--anchors` the report is unchanged.

A private repository can block force-push with a ruleset. That control needs a paid plan. The token should allow contents read and write on that one repository and nothing else.

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
