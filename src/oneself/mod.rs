//! Receipt chain for one pulse (PN-108).
//!
//! Row format, canonical JSON, and the hash chain. The daemon, the CLI, and
//! config do not call this yet.

#![allow(dead_code)]

pub mod chain;
pub mod event;
