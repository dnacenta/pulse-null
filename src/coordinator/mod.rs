//! Coordinator substrate — Stage 0 of the coordinator spec.
//!
//! Leases bound *intent* (who may act on a resource, for how long); fencing
//! tokens enforce *correctness at the resource* (a stale holder that wakes
//! past reclaim is rejected on write). Stages 1–3 (fail-open faculties,
//! isolation mode, subtask farming) build on this module.
// Nothing wires the coordinator in until Stage 1 — suppress dead_code until then.
#![allow(dead_code)]

pub mod fenced;
pub mod lease;
