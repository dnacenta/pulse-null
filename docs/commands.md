# Commands

Every command acts on the pulse whose directory you run it from. `--help` on any command
shows its full flags.

```
pulse-null init [--dir PATH]      create a pulse (wizard)
pulse-null up                     terminal UI: Home lists your pulses
pulse-null up --headless          daemon only, for systemd
pulse-null chat                   terminal UI, straight into this pulse's conversation
pulse-null down / status          stop it, show its state
pulse-null repair                 re-create the agent-CLI integration (instruction file, hooks)
pulse-null isolate on|off|status  diagnostic mode: the agent CLI runs read-only
pulse-null schedule list|add|remove|enable|disable|model
pulse-null intent list|add|remove|clear
pulse-null outreach status|respond
pulse-null pipeline health|stale  journal counts and staleness
pulse-null archive list|run       journal overflow archives
pulse-null recall [search|distill]   memory dashboard and tools
pulse-null praxis | vigil ...     journal enforcement, reflection signals
pulse-null plugin list|add|remove
pulse-null probe run [--if-due] [--only SET…] [--keep-scratch]   growth-record probes (sandboxed scratch)
pulse-null probe status           probe spend vs budget, cost per run, results by lineage
pulse-null probe verify-isolation [--turn]   show a probe writes nowhere live (--turn: two real turns)
pulse-null probe facts draw|plant FACT VALUE|list   paired facts for "Remembers" (needs the unit's credentials: run via systemd-run, see methods/probe-runner.md)
pulse-null probe sets install --from DIR --pulse NAME   (root) install the sealed sets into /etc/pulse-null-probe/sets/NAME
pulse-null probe export-graph [--if-due]  export the live graph for the next run; --if-due exits 0 without contacting the server when a scheduled run would be refused
```

Probes are described in [methods/probe-runner.md](methods/probe-runner.md); they refuse to run
without Landlock (kernel ABI 4) and refuse root.

`init` and `up` refuse to run as root; `PULSE_NULL_ALLOW_ROOT=1` overrides that for CI.
