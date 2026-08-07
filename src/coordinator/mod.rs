//! Coordinator substrate — Stage 0 of the coordinator spec.
//!
//! Leases bound *intent* (who may act on a resource, for how long); fencing
//! tokens enforce *correctness at the resource* (a stale holder that wakes
//! past reclaim is rejected on write). Stages 1–3 (fail-open faculties,
//! isolation mode, subtask farming) build on this module.
//!
//! **Single-writer invariant:** exactly one process owns the lease WAL and
//! therefore the lease table. `wal::LeaseWal` enforces this with an exclusive
//! file lock held for the process lifetime; a second coordinator instance
//! fails to open rather than silently sharing the log.
// Stages 2-3 consumers (FencedResource, parts of the lease API) are not
// wired yet — keep dead_code suppressed until the umbrella lands complete.
#![allow(dead_code)]

pub mod control;
pub mod durable;
pub mod farm;
pub mod fenced;
pub mod lease;
pub mod wal;

#[cfg(test)]
mod proptests;
