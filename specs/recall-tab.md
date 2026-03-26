# Spec: Recall Tab

## Problem

The memory graph is a core system with no visibility in the TUI. You can't see whether it's working, how populated it is, or when the entity accesses it. The only way to inspect it is through the GraphQueryTool (which requires the entity to query itself) or raw database inspection.

## Goal

A new "recall" tab in the TUI that shows memory system health, graph activity, and retrieval events in real time. The focus is observability — watching the memory system work, not editing it.

## Design

### Tab Position

Tab index 5 (after Comms). Label: `recall`.

```rust
pub enum Tab {
    Chat,       // 0
    Entity,     // 1
    Evolution,  // 2
    Files,      // 3
    Comms,      // 4
    Recall,     // 5
}
```

### Layout

Three vertical sections:

```
┌─────────────────────────────────────────────────────┐
│  GRAPH HEALTH                                       │
│  entities: 142  relationships: 287  episodes: 53    │
│  ┌─────────────────────────────────────────────┐    │
│  │ Person ████████░░ 12   Thread ██████░░░░ 8  │    │
│  │ Concept ███████░░ 11   Thought █████░░░░ 7  │    │
│  │ Tool ██████░░░░░ 9    Decision ████░░░░░ 6  │    │
│  │ Project █████░░░░ 8    Pattern ███░░░░░░ 5  │    │
│  └─────────────────────────────────────────────┘    │
├─────────────────────────────────────────────────────┤
│  PIPELINE FLOW                                      │
│  learning: 3 active  thoughts: 4 active             │
│  curiosity: 2 open   reflections: 8                 │
│  praxis: 3 policies                                 │
│  stale: 2 (thread:rust-ownership 9d,                │
│           thought:emergence 12d)                    │
│  last sync: 4m ago  (12 created, 3 updated)         │
├─────────────────────────────────────────────────────┤
│  RETRIEVAL LOG                         [live]       │
│  08:42:31  search "rust ownership" → 4 hits (23ms)  │
│  08:42:28  traverse "Echo" depth=2 → 14 nodes       │
│  08:41:55  episode search "comms Nova" → 2 hits     │
│  08:40:12  sync_pipeline → +3 created, 1 updated    │
│  08:39:44  search "prompt injection" → 1 hit (18ms) │
│  ...                                                │
└─────────────────────────────────────────────────────┘
```

### Section 1: Graph Health

Static stats refreshed on tab focus and every 30s while visible.

Data source: `GraphMemory::stats()` returns `GraphStats { entity_count, relationship_count, episode_count, entity_type_counts }`.

Renders:
- Total counts (entities, relationships, episodes)
- Bar chart of entity types sorted by count
- Color: healthy bars in NORD14 (green), empty/zero in NORD11 (red)

### Section 2: Pipeline Flow

Data source: `GraphMemory::pipeline_stats(staleness_days: 7)` returns counts by stage/status plus stale entity list.

Renders:
- Count per pipeline stage (learning, thoughts, curiosity, reflections, praxis)
- Stale entities with days since last update
- Last sync timestamp and delta counts from most recent `PipelineSyncReport`

### Section 3: Retrieval Log

Real-time scrollable log of graph operations. This is the core observability piece — you can watch the entity think through its memory.

Each entry: `timestamp  operation  query/target  →  result summary  (latency)`

#### Implementation: GraphEvent Channel

The `GraphQueryTool` and session sync functions emit events through a bounded channel:

```rust
pub struct GraphEvent {
    pub timestamp: DateTime<Utc>,
    pub operation: GraphOperation,
    pub query: String,
    pub result_count: usize,
    pub latency_ms: u64,
}

pub enum GraphOperation {
    Search,
    EpisodeSearch,
    EntityLookup,
    Traverse,
    PipelineSync { created: usize, updated: usize },
    PipelineStats,
    Ingest { source: String },
}
```

A `tokio::sync::broadcast` channel on `AppContext` (or a ring buffer). The GraphQueryTool emits after each execution. Session sync functions emit after pipeline/vigil sync. The recall tab subscribes and renders the log.

Ring buffer of last 100 events. Scrollable with j/k.

### Memory Health Indicator

A simple status line derived from graph stats:

- **HEALTHY**: graph has entities, pipeline has flow, no stale > 14d
- **WATCH**: pipeline has stale items > 7d or graph is small (< 20 entities)
- **CONCERN**: no episodes in last 7d, or pipeline frozen (no sync in 24h)

This could also feed into the header if we want a global memory health badge.

## Feature Flag

Requires `graph` feature. When graph is disabled, the tab shows a message: "Graph not enabled. Set graph.enabled = true in config."

When graph is enabled but the database is empty (fresh start), show onboarding state: "Graph is empty. It will populate as conversations are archived and pipeline documents are synced."

## Phases

### Phase 1 — Static Health View
- New `RecallTab` struct implementing `TabView`
- Load stats on focus, render graph health and pipeline flow sections
- No retrieval log yet
- Register tab in `mod.rs` and `main_screen.rs`

### Phase 2 — Retrieval Log
- Add `GraphEvent` type and broadcast channel
- Instrument `GraphQueryTool::execute()` to emit events
- Instrument session sync functions
- Render scrollable log in tab

### Phase 3 — Live Refresh and Health
- Auto-refresh stats every 30s while tab is visible
- Compute memory health status
- Optional: feed health status to header as a badge

## Files Affected

- `src/tui/tabs/mod.rs` — add Recall variant to Tab enum
- `src/tui/tabs/recall.rs` — new file, RecallTab implementation
- `src/tui/screens/main_screen.rs` — instantiate and route to RecallTab
- `src/tui/app.rs` — graph event channel in AppContext
- `src/tools/graph_query.rs` — emit GraphEvent after execution (Phase 2)
- `src/session.rs` — emit GraphEvent after sync operations (Phase 2)

## Dependencies

- `recall-graph` crate (already a dependency behind `graph` feature)
- No new external crates needed
