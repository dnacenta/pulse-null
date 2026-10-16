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
```

`init` and `up` refuse to run as root; `PULSE_NULL_ALLOW_ROOT=1` overrides that for CI.
