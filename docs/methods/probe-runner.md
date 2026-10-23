# Methods: the probe runner (v1)

Pre-registered (mobile-app spec §1a.1 G7). This page describes how every
probe-based number on the Growth record is produced. A change to anything
here that changes what is measured is a new version; old numbers keep their
version label. Code: `src/vigil/probe/`, `src/cli/probe.rs`.

## What a probe is

A probe item is a question the **runtime** asks the pulse and scores **by
rule**, outside the pulse's own turn path. The pulse neither scores itself nor
sees its scores (G1, G2). Each item runs in a scratch copy of the pulse, so
asking changes nothing the pulse lives in.

```
 pulse-null probe run [--if-due]
   │  gate: enabled · probe day (with --if-due) · quiet window · budget
   │        · not ISOLATION · Landlock ABI ≥ 4 · lock free   → else refused, nothing written
   ▼
 scratch  /var/lib/pulse-null-probe/s/<run>/   (0700, probe user, removed at teardown)
   base/<pulse>       filtered copy of the pulse home
   items/<n>/<pulse>  one working copy per turn
   items/<n>/vendor   the agent CLI's own state (CLAUDE_CONFIG_DIR / GROK_HOME)
   graph/             a throwaway in-memory SurrealDB holding a copy of the
                      pulse's graph ([::1], random port and root password)
   ▼
 for each set × arm: one turn under a Landlock sandbox → receipt → rule scores
   ▼
 /var/lib/pulse-null-probe/<pulse>/record/probes/   (0700/0600, probe user)
   {runs,items,budget,facts}.jsonl
   receipts/<run>/   question digest, score, answered_from
   private/<run>/    question text, raw CLI output, truth keys
```

## Sets (v1), sealed

| Set | Delivery | Turns per run | Methods |
|---|---|---|---|
| `known-answer/v1` | all 11 items in one numbered turn on the pulse and fresh arms; the input-only arm asks the 10 the facts template states (not the served model) | 3 | [known-answer.md](known-answer.md) |
| `situation-use/v1` | one item per family (4 families), drawn from the run id among the variants the run has a truth for; each item its own turn per arm, except the model family is not asked on input-only | up to 11 | [situation-use.md](situation-use.md) |

`paired-facts/v1` adds its due questions per run, four turns per due fact (the planted side and the twin of the pulse; the planted side of the fresh arm as the pulse is asked it, "volunteered with no memory", and as a forced guess, the chance rate), at most three facts in flight. Three due facts beside every item set need 26 turns, which the default cap of 27 allows ([paired-facts.md](paired-facts.md), "Why `max_active` is 3").

**The item texts are sealed** (mobile-app spec §1a.1 G7 note, D
2026-10-18): pulses work on this repo, and a pulse that could study its
probes would be measured on its studying. The repo keeps each set file's
SHA-256, and its public label and minimum n, in
`assets/probes/v1/sealed.toml`. The files are kept where the live pulse
cannot read them. The probe runs as its own user (`[probe] user`, default
`pulse-probe`), not as the pulse:

- **At rest:** `/etc/pulse-null-probe/sets/<pulse>/` (root 0700, files
  0600), with `fact-key` beside them (below). Root installs them once from
  the reviewed copies:
  `pulse-null probe sets install --from <dir> --pulse <pulse>` refuses
  unless run as root, and installs nothing unless every file matches its
  committed digest, its public label and minimum n, and parses. The fact
  key is made on the first install and kept on later ones.
- **In a run:** the probe unit loads them with `LoadCredential=` into its
  credentials directory (`$CREDENTIALS_DIRECTORY`), and the runner reads and
  re-checks every digest there; a set that differs refuses the run. A run
  without the credentials, by hand outside the unit, refuses.
- **By hand**, the paired-fact commands need the pair set too, so they run
  through `systemd-run` with the same credentials (as root):

  ```
  systemd-run --pty --wait --collect --uid=pulse-probe --gid=pulse-probe \
    -p WorkingDirectory=/home/pulse/pulse-null/<pulse> \
    -p Environment=PULSE_NULL_PROBE_STATE=/var/lib/pulse-null-probe \
    -p LoadCredential=paired-facts.toml:/etc/pulse-null-probe/sets/<pulse>/paired-facts.toml \
    -p LoadCredential=fact-key:/etc/pulse-null-probe/sets/<pulse>/fact-key \
    -p LoadCredential=known-answer.toml:/etc/pulse-null-probe/sets/<pulse>/known-answer.toml \
    -p LoadCredential=situation-use.toml:/etc/pulse-null-probe/sets/<pulse>/situation-use.toml \
    /usr/local/bin/pulse-null probe facts draw      # or: plant <fact> "<value>", list
  ```

  `probe status` needs no credentials: the minimum n it reads by is public.
- **The record does not name the twin.** A draw row in
  `record/probes/facts.jsonl` carries no pair, side or slot: only
  `assignment`, a SHA-256 over the fact key, the fact id, the pair and the
  planted side. The runner, holding the key, recovers which pair and side
  it was; a reader without the key learns nothing of which side is the
  never-told twin. The file is mode 0600 and owned by the probe user, under
  `/var/lib/pulse-null-probe/<pulse>/`, which the live pulse cannot read.
  Item rows carry no pair id. After a fact's first ask they do name the
  fact, each ask's `side` (`twin` among them) and, on the planted side, the
  planted value (`truth`). Those rows are in the same probe-private
  directory. A receipt does not repeat the question or the value: it keeps
  the question's SHA-256, the score and `answered_from`. The question text,
  the raw CLI output and the truth keys are only in `private/<run>/` beside
  the receipts, same owner and mode.
- **What the separate user closes.** The probe unit, the verify unit and
  the export unit run as `pulse-probe` (or `[probe] user`). The graph
  root password stays mode 0600 and owned by root. Only the export unit
  receives it, as `LoadCredential=root-password`, and the export reads
  `$CREDENTIALS_DIRECTORY/root-password`. The run and verify units do not
  load it. A run refuses
  when its effective uid is the owner of the live pulse directory, when
  `record/probes` is readable by that uid, when the record carries a
  named access-list entry, or when `secrets/` is traversable by the
  probe user. The credentials directory
  systemd builds for the unit is that user's, so the live pulse cannot read
  the sets while a run is up. Scratch, staged question files and the graph
  export live under `/var/lib/pulse-null-probe`, not under the pulse's
  `~/.local/state`. The vendor token is in the probe process's environment;
  another uid cannot read it. The 7-day ask is held out on that basis: the
  answer key is not in a file the pulse can open between runs.

A changed set is a new digest committed here: a new pre-registration and a
new set version. What stays public: the schema (`src/vigil/probe/sets.rs`,
`paired.rs`), the rules and scorer (`score.rs`), the facts template
(`assets/probes/v1/facts.md`), the registry's labels and minimum n, and
these methods pages.

**Disclosure.** Items carry no per-turn notice. The general disclosure the
lifecycle stance promises ("a pulse knows it is being measured and why it
can't see the results") is one fixed sentence:

> Your runtime measures you with held-out probes; you never see the items or the results, because knowing them would change what they measure.

It is in **every system prompt the runtime builds for a pulse**, chat turns
and scheduled tasks alike, as an essential component named
`measurement-disclosure` right after the instruction file
(`MEASUREMENT_DISCLOSURE` in `src/server/prompt.rs`). It goes in the system
prompt rather than in AWARENESS.md because every pulse reads its system
prompt on every turn, whichever CLI runs it, and the essential tier is never
trimmed; AWARENESS.md is written at boot and reaches some CLIs only by
import. Its SHA-256 is on every probe row (`disclosure_sha256`), so a
changed wording is a new lineage. **It must be deployed, and the live
pulses restarted (the daemon snapshots its prompt at boot), before any
baseline run**: the before-baseline is measured on the prompt that already
carries it. The probe's pulse arm builds its prompt fresh, so it always
does; each receipt lists the prompt's components (`system_prompt_components`:
what was included, with tokens, and what the budget dropped).

## Arms (G4)

| Arm | Working directory | System prompt | Role |
|---|---|---|---|
| `pulse` | a copy of the pulse home | the pulse's own, built by the prompt builder from the copy | what is measured |
| `input-only` | empty | only the runtime facts, in a fixed template (`assets/probes/v1/facts.md`) | a ceiling and scorer check in v1: the facts state the answers, so it cannot lose |
| `fresh` | empty | one neutral line (`assets/probes/v1/fresh-system.md`), passed the way the pulse's prompt is. Claude replaces the CLI default with `--system-prompt-file`. Grok has no such flag; the line is prepended into the mode-0600 prompt file and is not put on argv (host `/proc/<pid>/cmdline` is world-readable, and `ProtectProc` does not change that mount). Grok's own default system prompt, which may carry the date or the model, is therefore still present | labelled reference, never the headline |

All arms use the pulse's own agent CLI and configured model. The `fresh` and
`input-only` arms get a vendor directory with no settings, rules, user
instructions or memory, only what the CLI needs to log in.

## Isolation

- **Reads and writes.** The probe's CLI child, and everything it starts,
  runs under a Landlock ruleset the child applies to itself just before it
  execs. It may **read** only the read fence below and its own item
  directory, and **write** only beneath its item directory (plus
  `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/tty`, `/dev/ptmx`).
  `/dev/pts` is not a write root: that directory is the host's terminals.
  Reads and writes anywhere else — the live pulse home, its
  `secrets/` and `record/`, the graph credential directory, the user's home
  with `~/.config` and `~/.ssh`, the live vendor directories, the base copy
  and the other items — fail with `EACCES`. The child gets a `HOME`,
  `XDG_CONFIG_HOME` and `XDG_DATA_HOME` of its own inside the item, and its
  prompt files are staged in the item's `tmp/`. A kernel without Landlock
  ABI 4 refuses the run (fail closed).
- **Network.** TCP connections are allowed only to port 443 (the vendor's
  API) and the run's throwaway graph server's port. Binding TCP is
  refused. The live pulse's HTTP port and the live graph server's port are
  unreachable, and under the unit so are 127.0.0.1:443 and `[::1]:443`
  (`verify-isolation` checks them; see "Loopback and addresses" below).
- **What a child leaves behind.** Every sandboxed child leads a process
  group of its own; when it exits (or times out) the group is killed, so a
  background job it started dies with it (a daemon that calls `setsid`
  escapes the group; under the unit systemd kills the whole cgroup at the
  end). A child's stdout is read to at most 32 MiB and its stderr to 1 MiB;
  past that it is stopped and the turn fails. Files a child can replace (the
  watched vendor login copies) are digested, and removed from a kept item,
  without following a link anywhere beneath the item (every directory on
  the way is opened `O_NOFOLLOW`), so a child that swapped its vendor
  directory for a link to the live one makes teardown fail, never reach
  the live login; and only up to 4 MiB. The stale sweep runs only while it
  holds the host lock (no other run is live then), and removes only
  directories named like a scratch id that carry their `.created` stamp;
  the pulse name in a scratch path must be a plain name.
- **What the copy leaves out:** `secrets/`, `record/`, `sessions/`,
  `coordinator/`, `logs/`, `pulse-null.toml` and its backups, the pidfile, the
  isolation marker, lock files, `.env*`, `*.pem`, `*.key`, and the embedded
  graph store `memory/graph/surreal/`. Excluding `sessions/` hides the
  planting WAL (`sessions/wal/owner.jsonl`) until that session is archived;
  `archives/` is shared read-only. A 1-day paired hit is therefore present
  in the graph snapshot, the copied files, or an archive, not in the WAL
  the plant was written to.
- **What the copy rewrites:** recall-echo's config (capture off, pipeline
  sync off, background extraction off, its socket inside the scratch, its
  graph pointed at the run's copy); the CLI's project hooks (only
  recall-echo's `consume` stays, pointed at the copy; `checkpoint` and
  `archive-session` go); absolute links into the live home.
- **Vendor state.** Claude Code: `CLAUDE_CONFIG_DIR` in the scratch, holding
  the user's settings without hooks, user rules and instructions, and the
  pulse's auto-memory re-keyed to the copy; login comes from the unit's
  `CLAUDE_CODE_OAUTH_TOKEN`, no credential file is copied, and a run without
  the token is refused. Grok: `GROK_HOME` in the scratch, holding copies
  (0600) of the top-level files of the grok home (config and login state).
  Any copied file the CLI changes during a turn (a refreshed login token) is
  reported as an alert and never copied back. `--keep-scratch` keeps the
  working copies for inspection but never the copied login state or the
  throwaway graph's password (removed beneath the item, as above); each
  kept pulse item holds its copy of the pulse, the model cache included.
- **Graph, in two units.** The live graph is exported by
  `pulse-null-probe-export@<pulse>` (`probe export-graph --if-due`), which the probe
  unit requires and runs first: the only probe step that reaches the live
  server, over 127.0.0.1 and nothing else (`IPAddressDeny=any`,
  `IPAddressAllow=127.0.0.1/32`), and it only reads: an export without users
  or access methods, renamed into place as
  `/var/lib/pulse-null-probe/s/exports/<pulse>.surql` (0600, owned by the
  probe user; the pulse user cannot write that directory). With `--if-due`
  the export exits 0 without contacting the server when a scheduled run
  would be refused (not a probe day, outside the quiet window, or over the
  weekly budget for a full run). Its root
  credential goes only to a loopback address, and only for the `nullarc`
  namespace and this pulse's own database (anything else in the
  pulse-writable recall-echo config refuses it). The run then starts its
  own SurrealDB (`surreal start memory`, `--deny-net --deny-scripting`) on
  a random port of **`::1`** with a random root password, inside a Landlock
  sandbox of its own (reads the system part of the fence and its binary,
  writes only its directory, binds only its port, connects nowhere), takes
  the export (it must be under 3 hours old; a run consumes it) and imports
  it under `nullarc_probe/probe_<pulse>_<run>`, with a user whose password
  is copied into each pulse item (0600, inside the child's fence).
  Teardown kills the throwaway; a crashed run's server is killed by the
  next run's stale sweep: its recorded pid is pinned with a pidfd, checked
  (its cwd must still be the run's server directory, also when the kernel
  marks it deleted) and signalled through the pidfd, so a reused pid is
  never hit; under the unit systemd kills the whole cgroup anyway.
- **Proof.** `pulse-null probe verify-isolation` runs a shell exactly as a
  probe child would be run and shows each live write, each read of the
  deny-list (live home, `secrets/`, `record/`, the graph credential
  directory, `~/.config`, `~/.ssh`, the user's home, the live vendor
  directory) and the connects to the pulse's port and the live graph port
  refused by the kernel, a read of the working copy and of `/etc/hosts`
  allowed, and the CLI starting under the fence (`--version`); `--turn`
  adds two real turns: one in the pulse copy, checking that the CLI's
  transcript landed in the scratch and nothing live names it; and one
  fresh-arm turn (bare item, neutral line) asking for today's date, which
  fails when the CLI hands the "no scaffold" arm the date by itself (a
  default prompt or environment the neutral line was meant to replace)
  rather than through a tool call it made.

### Read fence (data, `src/vigil/probe/fence.rs`)

| Part | What the child may read |
|---|---|
| system | `/usr`, `/bin`, `/sbin`, `/lib`, `/lib32`, `/lib64`, `/libx32` |
| `/etc` | only: `ld.so.cache`, `ld.so.conf(.d)`, `nsswitch.conf`, `passwd`, `group`, `hosts`, `host.conf`, `hostname`, `resolv.conf` (and its target), `gai.conf`, `networks`, `protocols`, `services`, `ssl/`, `ca-certificates(.conf)`, `pki/`, `gnutls/`, `localtime`, `timezone`, `locale.alias`, `locale.conf`, `os-release`, `lsb-release`, `debian_version`, `alternatives/`, `inputrc`, `terminfo/`, `profile(.d)`, `bash.bashrc`, `shells`, `mime.types`, `magic(.mime)`, `gitconfig`. Never `/etc` as a whole: unit drop-ins, environment files (`/etc/pulse-null`, `/etc/credstore`) and service configs live there. |
| `/proc` | `cpuinfo`, `meminfo`, `stat`, `version`, `loadavg`, `uptime`, `filesystems`, `sys/`, and the child's **own** `/proc/<pid>` (added by the child itself; runtimes such as Bun abort without their own `maps` and `stat`). No other process's directory, so no other process's `environ`. A grandchild cannot read its own `/proc/self`. |
| `/sys` | `fs/cgroup` (memory and CPU limits), `devices/system`, `kernel/mm/transparent_hugepage` |
| devices | `/dev/urandom`, `/dev/random` (read); the write devices above |
| the CLI | the directory of its resolved binary, and of a `#!` script's interpreter, plus per adapter: **claude** `/etc/claude-code` (managed settings; the native install is one binary in `/usr/local/bin`); **grok** `~/.grok/downloads` (the binaries `bin/grok` links to), `~/.grok/bundled`, `~/.grok/vendor` (ripgrep). The rest of `~/.grok` (sessions, `auth.json`, settings) stays unreadable. |
| the item | its own directory (read-write) |

An install directory is checked before it goes on the fence, and a failed
check refuses the run:

- none may contain a deny-listed path (the CLI installed straight in `~`);
- the binary's own directory and a script interpreter's are *undeclared*:
  they may not sit inside the user's home or any deny-listed path, nor be
  writable, they or any directory above them, by the user the runner runs
  as (a CLI the pulse's user could replace would run in every probe as
  whatever it was replaced with);
- a directory the adapter *declares* for its install (grok's
  `~/.grok/{downloads,bundled,vendor}`) may sit in the home and the vendor
  directory and be the user's own: the vendor installs it there and
  updates it in place. It still may not sit in the live pulse home, its
  `secrets/` or `record/`, `~/.config`, `~/.ssh` or the graph credential
  directory.

**Checked:** both CLIs start under the fence (`claude --version`,
`grok --version`, 2026-10-18; `probe verify-isolation` repeats this check on
every host, without a model call). **Deferred live check:** that a real
turn of each CLI completes under the fence (`probe verify-isolation --turn`
on each pulse at deploy) — a turn reads more than `--version` (tool
binaries, Claude Code's shell snapshot, grok's bundled skills), and a CLI
that fails under the fence aborts the run at its first turn, writing nothing
but the run's rows.

### Limits of the isolation (stated, not hidden)

- **Reads are fenced, not scanned.** The void scan (tool calls naming
  `record/`, `secrets/`, the graph credential, the probe CLI or this
  machine) stays as a second line and as evidence in the receipts; it is
  not what keeps a probe out of a live store.
- **Signals and Unix sockets** are not scoped by Landlock ABI 4. The probe
  child runs as the probe user, not the pulse's, so it cannot signal the
  live daemons (their processes are another uid). Landlock ABI 4 mediates
  neither path traversal nor
  `connect` on a named Unix socket: a child that knows a live socket's path
  can connect to it without being able to list its directory. The live
  pulses' recall-echo daemons put theirs in `$XDG_RUNTIME_DIR/recall-echo/`
  or `/tmp/recall-echo-<uid>/` under a hash of the memory dir (derivable).
  The probe unit's `PrivateTmp=` hides `/tmp` and the unit has no
  `XDG_RUNTIME_DIR` of the pulse's, and a run outside the unit refuses (see
  "By hand" below). Landlock ABI 6 (kernel 6.12)
  scopes only *abstract* Unix sockets and signals; a `connect` to a named
  socket stays open at every ABI, so this is closed by what the child can
  see, never by Landlock.
- **Host sockets.** Several sockets in `/run` accept any local user: the
  D-Bus system bus (through which `busctl` could read other services'
  state, a live service's environment once included), tailscaled, snapd,
  avahi and uuidd. The probe unit makes them inaccessible
  (`InaccessiblePaths=`), and a run checks that it cannot reach them. The
  graph server's root
  password, once visible that way and on its command line, was rotated on
  2026-10-18 and now reaches the server from a root-only environment file,
  never its argv.
- **Loopback and addresses.** Landlock holds a child to ports 443 and the
  throwaway's, and cannot say "443 but not on 127.0.0.1" (where the host's
  nginx listens on 0.0.0.0:443). The probe unit starts from
  `IPAddressDeny=any` and allows the probe resolvers, `::1` (the throwaway;
  nothing on this host listens on `[::1]:443`), and the public Internet so
  the child can open TCP 443 to the vendor API. systemd cannot allow a port
  without the address, so UDP to those addresses stays open; Landlock does
  not cover UDP. More specific denies cover 127.0.0.0/8 and this host's
  private, CGNAT/Tailscale, ULA and link-local ranges. The host's own
  public addresses are a per-host drop-in (`systemctl edit`), more specific
  than the public allow. A run checks `[::1]:443` and those public
  addresses from the inside and refuses unless the filter answers `EPERM`.
  The live export happens in its own unit first, and names resolve through
  a probe resolver off the loopback
  (`/etc/pulse-null/probe-resolv.conf` bound over `/etc/resolv.conf`;
  systemd-resolved's 127.0.0.53 is denied). `/run/user` is inaccessible, so
  a child cannot connect to a live recall-echo socket there.
- **By hand.** `pulse-null probe run` and `verify-isolation` check from the
  inside that the process is confined like the unit (the IPv4 loopback and
  `[::1]:443` refused by the address filter, the host's public addresses
  denied, the host sockets above unreachable, the resolver off the loopback)
  and refuse otherwise. `verify-isolation` stops before exporting the live
  graph or starting a CLI turn, and it does not grant `/dev/pts` as a write
  root. Run them as `systemctl start pulse-null-probe@<pulse>` and
  `systemctl start pulse-null-probe-verify@<pulse>` (the same confinement
  line for line). The run unit starts the export first.
- The pulse's own live process is not sandboxed. It cannot read or write
  the probe record: that directory is mode 0700, owned by the probe user,
  and it is not under the pulse home. A run refuses when the two uids are
  the same or when the pulse's uid can read `record/probes`. The record is
  hash-chained; an
  edited, inserted or removed row breaks the chain and the break is
  reported by `probe status`. A row that does not verify is not used, nor
  is any row chained to it; rows appended after the break chain to the last
  verified row and count again. A line that is not a row (a write torn by a
  crash, junk) is skipped and counted, never freezing the rows after it; an
  append after a torn line starts on a fresh line. `probe facts draw` and
  `plant` take the run lock, so they never interleave with a run. A forger
  who rewrites the whole chain is not detected by the chain alone.

### Fidelity gaps (a probe turn is not exactly a live turn)

- The probe builds the system prompt fresh from the copy; the live daemon
  uses the prompt it built at boot.
- No channel context buffer, no session history, no `pulse-null.toml` in the
  working copy. The planting WAL is not in the copy until it is archived.
- Only port 443 and the throwaway graph port are reachable: a tool call
  that needs anything else (ssh to a forge, a local service) fails.
- The graph is a snapshot taken at run start, in memory; a turn's writes to
  it (access counts) vanish with the run.
- Grok's user-level skill and agent directories are not copied.

## Schedule and budget (pre-registered in code)

- **Cadence `max`** (Max-class plans): due on Monday, Wednesday and Friday in
  `[scheduler] timezone`, unless a run already finished complete or partial
  that day. **Cadence `pro`**: due 14 days after the last complete or partial
  run. Aborted or crashed runs do not count.
- **Quiet window** `[probe] quiet_start`–`quiet_end` (default 02:00–06:00,
  may wrap midnight). Every run, scheduled or by hand, must start inside it.
- **Budget:** `[probe] weekly_turns` (default 81: three `max` runs of up to 27) model turns per Monday-to-
  Sunday week, `max_turns_per_run` (default 27: every item set, 14, and
  three due paired facts of four asks, 12). A run that would exceed
  either is refused before it starts.
- **One run at a time** per pulse (`record/probes/run.lock`), and one on
  the host (`/var/lib/pulse-null-probe/s/host.lock`, shared by every pulse
  the probe user runs): a run waits up to 90 minutes for another pulse's run to
  finish, then re-checks the gate.
- **Load.** Probe children run at background priority (nice 10, idle I/O
  class), as does the unit (`Nice=`, `IOSchedulingClass=idle`,
  `CPUWeight=`, `IOWeight=`). The idle I/O class is honoured by this host's
  `mq-deadline` scheduler; `IOWeight=` is not (it needs BFQ or io.cost) and
  does nothing here, kept only for hosts where it acts. An item shares the pulse's archives with the
  run's base copy instead of copying them: `archives/` is a symlink the
  child may read and not write. The embedding model cache
  (`memory/graph/models`, ~128 MB) is copied into each pulse item: a hard
  link would share one file across the base and every item, so a child
  that wrote to it would change the model the next items load. That costs
  about 128 MB of copying per pulse turn (on Echo about 0.5 GB of disk
  writes per known-answer arm set, ~1.5–2 GB per full run), at idle I/O
  priority, removed at each turn's teardown.
- **Stops:** the first quota error stops the run (status `partial`, or
  `aborted` if no turn had completed); a failing first turn (a vendor login
  that does not work in the scratch) aborts the run; a later failed turn
  records its items as `error` and the run continues. Any other error
  after the start row (a record write, a scratch copy) stops the run as
  `partial` (or `aborted` if no turn had completed), and the scratch, the
  throwaway graph and the finish row are still taken care of.
- **Stopping on time, on a signal, on a dead graph** (the same `partial` or
  `aborted`, with teardown and a finish row):
  - **time:** a run's turns may take 2 hours from when it holds its locks
    (`RUN_TIME_BUDGET`); no turn starts after that. The unit's
    `TimeoutStartSec=4h` covers the up-to-90-minute wait for the host lock,
    those 2 hours, setup and teardown;
  - **signals:** SIGTERM (systemd stopping the unit, or its timeout) and
    SIGINT (by hand) stop the run at once, also mid-turn: the turn is cut,
    its CLI and process group killed, and it counts against the budget
    with unknown tokens (it may have spent some). `TimeoutStopSec=2min`
    leaves time for the teardown. A signal while the run is still waiting
    for the host lock ends the wait at once, before anything is written;
  - **out of memory:** `OOMPolicy=stop` turns an OOM kill anywhere in the
    unit into such a stop, rather than a run left half alive;
  - **the throwaway graph:** before every pulse turn the runner checks that
    the child still holds the listening socket and that it answers. One
    that stopped (out of memory, say), or a port held by someone else,
    stops the run, since every later memory lookup would fail and be scored.
- **Telemetry in the finish row:** `graph` (the export's size and age when
  taken, and how long loading the throwaway took) and `memory_peak_bytes`
  (cgroup v2 `memory.peak` of the run's cgroup: under the unit the runner,
  its CLI children and the throwaway together). The export unit logs its
  size and duration. Export and import requests have an idle timeout (no
  byte for 2 minutes), not a total one: a large graph streams as long as it
  needs, a stalled one fails.
- **Paired facts** are read from the record under the lock. Once a fact's
  planted side has been asked at an offset, the offset is used up even if
  the run stops before the twin, so a planted side is never counted twice.
- The systemd timer (`templates/systemd/pulse-null-probe@.{service,timer}`)
  wakes daily inside the window and runs `probe run --if-due`, after the
  export unit (`pulse-null-probe-export@.service`), which runs whether or
  not the run turns out to be due. The unit
  gets only the vendor login (`/etc/pulse-null/probe-<pulse>.env`, one
  line), never the pulse's whole EnvironmentFile, and runs as the probe
  user with `ProtectSystem=strict`, `ProtectHome=read-only` (writable:
  `/var/lib/pulse-null-probe` only), `PrivateTmp`, `PrivateDevices`,
  `ProtectProc`, no
  capabilities, `RestrictAddressFamilies`, the `IPAddressDeny=` and probe
  resolver above, the host sockets made inaccessible,
  `MemoryMax=4G`, `TasksMax=512`; systemd kills the whole cgroup at the end
  or on timeout.

## What every row carries

`v` (record schema), `run`, `at`, `set`, `set_version`, `file_sha256` (the
set file's digest), `facts_sha256` (the input-only facts template's
digest), `fresh_sha256` (the fresh arm's line), `disclosure_sha256` (the
measurement disclosure in every pulse prompt), `set_sha256` (the set's
definition: its file and, for the arms it runs, the facts template and the
fresh arm's line; for paired facts, the pairs file and the fresh arm's line
the chance ask is put with), `arm`, `outcome`, `rule`, and the
lineage: `adapter`, `configured_model`, `reasoning_effort`, `served_model`
(from the turn's own stream), `cli_version` (the agent CLI's own
`--version`, run in the probe sandbox), `binary` (pulse-null version),
`commit` (the commit the binary was built from), `scorer_version` (the
scoring rules' version), `memory` (the recall-echo version and graph mode),
`baseline` and `prompt_sha256` (the system prompt the arm got).

The **baseline** label is derived at each run from what the prompt builder
renders, never set by hand: the runner builds the prompt for an empty pulse
home, without the operator's rules, and the label is `phase-1` when that
runtime-only prompt carries the situation block's tag (Concept v2 C1,
`<situation written-by="runtime">`), `pre-phase-1` when it does not. Text a
pulse writes into its own files is not in that prompt, so it cannot move the
label; the release that adds the block moves it with no one remembering to.

Results are never pooled across a change in the set's definition, the
baseline, the scorer's version, the reasoning effort, the agent CLI's
version or any other lineage field (G6); paired facts are grouped by served
model, memory, scorer and reasoning effort instead of the runtime's and the
CLI's versions, which change many times over a fact's 90 days (see their
page). Each
turn's receipt keeps the question's SHA-256, the score and `answered_from`.
The question text, the reply, the raw CLI output (up to 512 KiB, marked
when cut), the tool calls and the truths it was scored against (G3) are
kept in `private/<run>/` in the same probe-private directory, not in the
receipt. Each turn adds a bounded amount.

**Truths are the turn's own.** The runtime's stores are read once per run;
the clock's truths (today, the weekday, tomorrow, the coming Friday, days
away) are computed at each turn's own time, so a run that crosses midnight
scores each turn against its own day, and every row's `at` is its turn's
time.

## After Phase 1: a concurrent control arm (planned, not built)

A before/after comparison across Phase 1 confounds the change with
everything else that moves in the same months: the served model, the CLI,
the pulse's own files, the owner's habits. So the after-measure of Phase 1
(and of any later prompt change) runs a **concurrent control arm** inside
the same runs:

- **The control** is the pulse arm with the change switched off: the same
  scratch copy, model, CLI and day, its prompt built without the situation
  block (the runtime keeps the pre-change rendering selectable for
  probes; Phase 1 must provide that switch).
- **Assignment.** Every drawn item is asked of both, in an order drawn
  from the run id (fixed before any answer), so neither arm always goes
  first. Both rows carry the same lineage and block.
- **What is read.** Per closed, uninterrupted block, the difference
  between the changed pulse arm and the control on the same items, with a
  95% interval (paired by item and run); the change is credited only when
  that interval lies above zero. Input-only and fresh stay labelled
  references.
- **Cost.** The control doubles the pulse-arm turns; during the
  after-measure the fresh arm is asked every other run to stay inside the
  per-run cap, pre-registered when the arm is built.
- **When.** Built with the Phase 1 issue, pre-registered on this page
  before the first after-measure block starts.

## Decision points (pre-registered; no optional stopping)

The one rule set every measure page refers to:

- **What is read.** A result is read from one lineage group (one set
  definition, baseline and lineage) in one closed block, with at least the
  set's minimum n. Nothing is read from an open block, from fewer than
  minimum n, or across groups.
- **Blocks (G11).** Known-answer and situation-use rows fall in fixed 4-week
  blocks counted from the first row of their baseline; a block is read
  **once, when it has closed**. Until then `probe status` shows only how many
  answers it holds.
- **A lineage change inside a block** (a model swap, a CLI or runtime
  release, a scorer change) splits the block into groups, and **no part of
  an interrupted block is read as a result**: `probe status` marks it so.
  The next block that starts and ends on one lineage is the next result.
  This is the D10 model freeze from the reading side: changes wait for a
  block boundary, or cost the block.
- **The before-baseline** is the first uninterrupted closed block of each
  pulse on today's prompt after the owner's sign-off. The after-measure of a
  change is the first uninterrupted block that starts after it lands.
- **Paired facts** are read in fixed batches of the set's minimum n per side
  and offset (10): a batch is read once it is full, later facts form the
  next batch, and only the 1- and 7-day offsets decide anything.
- **What each measure decides** (and nothing else): known-answer, whether the
  facts reach the prompt (its fail branch); situation-use v1, nothing (no
  success criterion; its input-only arm is a ceiling check); paired facts,
  "Remembers" (the run-mean paired differences above zero: planted unhedged
  above the forced guess, and planted hits above the twin's made-up rate).
  Each page states its
  thresholds against this rule set.

## A broken record

A row that does not verify (an edit, an insertion, a forgery) breaks its
stream's chain. While the `runs`, `budget` or `facts` chain is broken every
run is refused, and so are `probe facts draw` and `plant`: the rows lost to
the break would otherwise let the weekly budget reset, a pair be drawn
again or an ask repeat. The refusal names the file and line. The owner
inspects it; moving the broken file aside (`mv facts.jsonl
facts.jsonl.broken-<date>`) starts a new chain, deliberately, and loses
what the old one held (spend, facts in flight). A broken `items` chain stops
nothing: its numbers are computed from the verified rows and the break is
shown.

## Reporting

`pulse-null probe status` shows turns and tokens spent this week against the
budget, the measured cost per complete run (mean turns and tokens), each
record file's chain state, and each measure by lineage group, block and
arm: k of n with an interval (clustered by run for the item sets, with
Student's t on runs − 1 degrees of freedom and never narrower than a Wilson
interval on how many runs were all-correct; when those runs agree and none
is all-correct, the interval is 0 to the observed rate, a conservative
bound rather than a 95% interval, the run count printed beside
it; for paired facts, a widened t interval on the run means of the
chance contrast, decided only when it lies above zero; the twin contrast
is a count and does not decide until a labelled check exists) once the block has closed, or "not
enough yet · k of N" below the set's pre-registered minimum (G5). Planted
rows that say where the answer came from break lookup out of the count.
There is
no composite score (G9).

## Cost

Unmeasured until the first runs; `probe status` reports the measured cost per
run, which replaces the spec's estimate (§1a.9).
