# Terminal UI

`pulse-null up` (without `--headless`) and `pulse-null chat` open the terminal UI. It is a
client of the running daemon: if one is up it attaches over HTTP, otherwise it starts one
in-process and stops it cleanly on exit. It never owns a provider or writes session files.

**Home** is the first screen: one *Talk to <pulse>* row per pulse you own (found in
`~/pulse-null/<name>`, the directory you are in, and the legacy `~/entity`), each showing
`up`, `stopped` or `unreachable`, probed every two seconds; then *Create a new pulse*, which
runs the setup wizard; then *Exit*. `Enter` on a stopped pulse starts its daemon in this
process and opens Talk; on a running one it attaches. `:home` goes back without stopping
anything; Exit stops the daemons Home started. `pulse-null chat` inside a pulse directory
skips Home.

The window model follows Hyprland: panes with a one-cell gap, one accent border on the
focused pane, `Ctrl+h/j/k/l` to move focus, `f` for fullscreen. A one-line bar shows the
pulse, model, page, cognitive status (*no signal yet* until there is data), alert count and
clock.

**Talk** is the conversation. Your message appears the instant you press Enter; the reply
streams token by token; typing works during a reply and Enter queues one message; `Ctrl+c`
cancels a reply (the daemon rolls the turn back); scrolling up during a reply holds your
place and shows `↓ new`, `G` glides back to the tail.

**Peer to peer** is a dialogue between two of your pulses, run by the first one's daemon.
With two pulses up, Home lists a *Peer to peer · A ↔ B* row per pair; Enter asks for an
optional topic and a turn cap (20 by default, 50 at most) and opens the **Peer** page,
where turns stream in with each side in its own colour. `Space` pauses and resumes, `Ctrl+c`
stops (after a confirm), `q` goes back to Home while the dialogue keeps running — the row
reads `dialogue running · turn n/max` and Enter re-attaches. From Talk, `:comms <pulse>
[topic]` does the same (`Tab` completes; a name under `[peers]` works too). Every dialogue
is archived like a conversation when it ends.

`:` opens the command line (`Tab` completes): `:home`, `:comms <pulse> [topic]`,
`:theme <name|system>`, `:motion <full|reduced|off>`, `:quit`, `:help`. `?` shows the keys
for the focused pane.

```toml
[tui]
theme = "gruvbox"     # gruvbox (default), tokyo-night, catppuccin, everforest, rose-pine, nord, or "system"
motion = "full"       # full | reduced | off — drops to reduced by itself on a slow link
nerd_font = "auto"    # auto | on | off
```

With `theme = "system"` on an [Omarchy](https://omarchy.org) desktop the palette is read
from `~/.config/omarchy/current/theme/colors.toml` and crossfades when you switch themes.
Logs go to `logs/tui.log` in the pulse directory while the UI is up.
