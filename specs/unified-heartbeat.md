# Spec: Unified Heartbeat

## Problem

The header heartbeat only reflects entity activity during chat. When the entity is processing through other channels (comms with a peer, scheduled task, self-reflection, research), the header stays idle. This makes it look like the entity is doing nothing.

Current behavior in `main_screen.rs:141-147`:
```rust
let comms_state = self.comms.active_entity_state();
let header_state = if self.active_tab == Tab::Comms && comms_state != EntityState::Idle {
    &comms_state
} else {
    &self.chat.state
};
```

Two bugs here:
1. Comms state only surfaces when the Comms tab is active — switching to Chat tab hides it.
2. Scheduled tasks, intents, and self-reflection never surface at all.
3. Even when comms state is used, the pulse waveform and color still come from `self.chat`.

## Goal

The header heartbeat reflects entity activity from any source. If the entity is processing, the pulse shows it — regardless of which tab is focused.

## Design

### Shared Entity State

Introduce a top-level `EntityPulse` that aggregates activity from all sources:

```rust
pub struct EntityPulse {
    state: EntityState,
    source: PulseSource,
    pulse_data: VecDeque<f64>,
    pulse_color: PulseColorTransition,
}

pub enum PulseSource {
    Chat,
    Comms { peer: String },
    Scheduler { task: String },
    Intent { description: String },
    Idle,
}
```

This lives in `AppContext` so all tabs can update it.

### Priority Resolution

When multiple sources are active simultaneously, use priority:
1. Chat (owner is directly interacting)
2. Comms (peer conversation in progress)
3. Scheduler/Intent (background autonomous work)
4. Idle (nothing happening)

If a higher-priority source goes idle, fall through to the next active source.

### Update Points

1. **Chat tab** — already sets `self.chat.state`. Also writes to `EntityPulse`.
2. **Comms tab** — `LocalActivity` events write to `EntityPulse` with `PulseSource::Comms`.
3. **Scheduler runner** — emit state changes to `EntityPulse` via `AppContext` or EventBus event.
4. **Intent executor** — same as scheduler.

For scheduler and intents (which run outside the TUI in server mode), we need an `EntityState` channel on the EventBus:

```rust
EntityEvent::StateChange {
    state: EntityState,
    source: PulseSource,
}
```

The TUI's event listener picks these up and updates `EntityPulse`.

### Header Rendering

Replace the current conditional logic with:

```rust
let pulse = &ctx.entity_pulse;
header::draw(
    frame,
    chunks[0],
    ctx.entity_name.as_deref(),
    Some("HEALTHY"),
    ctx.model_name.as_deref(),
    &pulse.pulse_data,
    &pulse.state,
    &pulse.pulse_color,
);
```

### State Label

The header state label (currently just the EntityState name) should include the source:

- Idle: `idle`
- Chat thinking: `thinking`
- Comms thinking: `thinking (Nova)`
- Scheduler: `task (reflection)`
- Intent: `research (emergence)`

## Phases

### Phase 1 — Fix Comms Visibility
- Remove the `active_tab == Tab::Comms` guard
- Use comms pulse data/color when comms state is active
- Minimal change, immediate fix

### Phase 2 — EntityPulse Abstraction
- Create `EntityPulse` struct in `AppContext`
- All sources write to it with priority resolution
- Header reads from `EntityPulse` only

### Phase 3 — Background Activity
- Add `EntityEvent::StateChange` to EventBus
- Scheduler and intent executor emit state changes
- TUI event listener updates `EntityPulse`
- Source label in header

## Files Affected

- `src/tui/screens/main_screen.rs` — header state resolution
- `src/tui/app.rs` — EntityPulse in AppContext
- `src/tui/widgets/header.rs` — source label rendering
- `src/tui/tabs/comms.rs` — write to EntityPulse
- `src/tui/tabs/chat.rs` — write to EntityPulse
- `src/events/mod.rs` — StateChange event (Phase 3)
- `src/scheduler/runner.rs` — emit StateChange (Phase 3)
- `src/scheduler/intent.rs` — emit StateChange (Phase 3)
