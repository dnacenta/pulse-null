//! Property tests for the lease + fencing substrate.
//!
//! A state machine drives arbitrary interleavings of acquire / renew /
//! release / time-advance / restart / write ops against the *durable* table —
//! including the paused-holder-wakes-stale race, which here is just "a holder
//! still writing with a token that has since been superseded". Invariants:
//!
//! 1. Fencing tokens are strictly increasing per resource — including across
//!    restarts (recovery must never regress the watermark).
//! 2. The holder of the highest issued token is never fenced.
//! 3. A stale holder is always fenced once the superseding holder has written.
//! 4. Every restart recovers exactly the pre-restart table (WAL replay == live
//!    state), checked at each `Op::Restart`, not just once at the end.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use proptest::prelude::*;

use super::durable::DurableLeaseTable;
use super::fenced::FencedResource;
use super::lease::FencingToken;

#[derive(Debug, Clone)]
enum Op {
    Acquire {
        r: u8,
        h: u8,
        ttl_secs: u8,
    },
    Renew {
        r: u8,
        h: u8,
        ttl_secs: u8,
    },
    Release {
        r: u8,
        h: u8,
    },
    Advance {
        secs: u8,
    },
    Restart,
    /// Crash mid-append: torn unterminated bytes land on the log, then the
    /// coordinator restarts. Acked state must survive unchanged.
    TearAndRestart,
    Write {
        r: u8,
        h: u8,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    // 2 resources, 3 holders — small domain forces collisions.
    let r = 0..2u8;
    let h = 0..3u8;
    prop_oneof![
        4 => (r.clone(), h.clone(), 1..60u8)
            .prop_map(|(r, h, ttl_secs)| Op::Acquire { r, h, ttl_secs }),
        2 => (r.clone(), h.clone(), 1..60u8)
            .prop_map(|(r, h, ttl_secs)| Op::Renew { r, h, ttl_secs }),
        2 => (r.clone(), h.clone()).prop_map(|(r, h)| Op::Release { r, h }),
        3 => (1..120u8).prop_map(|secs| Op::Advance { secs }),
        1 => Just(Op::Restart),
        1 => Just(Op::TearAndRestart),
        4 => (r, h).prop_map(|(r, h)| Op::Write { r, h }),
    ]
}

fn rid(r: u8) -> String {
    format!("res-{r}")
}

fn hid(h: u8) -> String {
    format!("holder-{h}")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    #[test]
    fn lease_fencing_invariants(ops in prop::collection::vec(op_strategy(), 1..120)) {
        let dir = tempfile::tempdir().unwrap();
        let mut durable = DurableLeaseTable::open(dir.path()).unwrap();
        let mut now: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap();

        // One fencing guard per resource. Guards live at the resource, so a
        // coordinator restart does not reset them.
        let mut guards: HashMap<String, FencedResource> = HashMap::new();
        // Every token ever issued, per resource (invariant 1).
        let mut issued: HashMap<String, Vec<FencingToken>> = HashMap::new();
        // Each holder's belief about the token it holds — kept even after the
        // lease is reclaimed under it. This IS the paused stale holder.
        let mut believed: HashMap<(String, String), FencingToken> = HashMap::new();

        for op in ops {
            match op {
                Op::Acquire { r, h, ttl_secs } => {
                    let (r, h) = (rid(r), hid(h));
                    if let Ok(lease) =
                        durable.acquire(&r, &h, Duration::from_secs(u64::from(ttl_secs)), now)
                    {
                        // Invariant 1: strictly above everything issued
                        // before — across restarts too.
                        let prior = issued.entry(r.clone()).or_default();
                        prop_assert!(prior.iter().all(|t| lease.fencing_token > *t),
                            "token {} not above prior {:?}", lease.fencing_token, prior);
                        prior.push(lease.fencing_token);
                        believed.insert((r, h), lease.fencing_token);
                    }
                }
                Op::Renew { r, h, ttl_secs } => {
                    let (r, h) = (rid(r), hid(h));
                    if let Ok(lease) =
                        durable.renew(&r, &h, Duration::from_secs(u64::from(ttl_secs)), now)
                    {
                        // Renewal never mints a token.
                        prop_assert_eq!(believed.get(&(r, h)).copied(), Some(lease.fencing_token));
                    }
                }
                Op::Release { r, h } => {
                    let (r, h) = (rid(r), hid(h));
                    if durable.release(&r, &h, now).is_ok() {
                        believed.remove(&(r, h));
                    }
                }
                Op::Advance { secs } => {
                    now += Duration::from_secs(u64::from(secs));
                }
                Op::Restart => {
                    // Invariant 4: recovery reproduces the live table exactly.
                    let before = durable.table().clone();
                    drop(durable);
                    durable = DurableLeaseTable::open(dir.path()).unwrap();
                    prop_assert_eq!(durable.table(), &before);
                }
                Op::TearAndRestart => {
                    let before = durable.table().clone();
                    drop(durable);
                    {
                        use std::io::Write;
                        let mut f = std::fs::OpenOptions::new()
                            .append(true)
                            .open(dir.path().join("leases.jsonl"))
                            .unwrap();
                        f.write_all(br#"{"v":1,"e":{"event":"acq"#).unwrap();
                    }
                    durable = DurableLeaseTable::open(dir.path()).unwrap();
                    // Torn bytes are truncated; every acked grant survives.
                    prop_assert_eq!(durable.table(), &before);
                }
                Op::Write { r, h } => {
                    let (r, h) = (rid(r), hid(h));
                    let Some(&token) = believed.get(&(r.clone(), h)) else { continue };
                    let guard = guards.entry(r.clone()).or_default();
                    let max_issued = issued[&r].iter().max().copied().unwrap();
                    let result = guard.accept(token);

                    if token == max_issued {
                        // Invariant 2: the newest epoch is never fenced.
                        prop_assert!(result.is_ok(),
                            "newest token {} fenced on {}", token, r);
                    } else if guard.high_water().is_some_and(|hw| hw > token) {
                        // Invariant 3: once a newer epoch has written, the
                        // stale waker is rejected — the AC7 race.
                        prop_assert!(result.is_err(),
                            "stale token {} accepted on {} past high-water", token, r);
                    }
                }
            }
        }

    }
}
