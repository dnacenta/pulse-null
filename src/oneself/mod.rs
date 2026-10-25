//! Receipt chain for one pulse (PN-108).
//!
//! The row format, the hash chain, and the append-only store. The daemon
//! does not open the store yet. `pulse-null receipts verify` and `show` read
//! a file.

pub mod chain;
pub mod event;
pub mod protect;
pub mod store;
