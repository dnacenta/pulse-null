//! Property tests for the lease + fencing substrate.
//!
//! A state machine drives arbitrary interleavings of acquire / renew /
//! release / time-advance / write ops — including the paused-holder-wakes-
//! stale race, which here is just "a holder still writing with a token that
//! has since been superseded". Invariants checked at every step:
//!
//! 1. Fencing tokens are strictly increasing per resource.
//! 2. The holder of the highest issued token is never fenced.
//! 3. A stale holder is always fenced once the superseding holder has written.
//! 4. WAL replay reconstructs the live table exactly (checked at the end).

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use proptest::prelude::*;

use super::fenced::FencedResource;
use super::lease::{FencingToken, LeaseTable};
use super::wal::{LeaseEvent, LeaseWal};

#[derive(Debug, Clone)]
enum Op {
    Acquire { r: u8, h: u8, ttl_secs: u8 },
    Renew { r: u8, h: u8, ttl_secs: u8 },
    Release { r: u8, h: u8 },
    Advance { secs: u8 },
    Write { r: u8, h: u8 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    // 2 resources, 3 holders — small domain forces collisions.
    let r = 0..2u8;
    let h = 0..3u8;
    prop_oneof![
        (r.clone(), h.clone(), 1..60u8).prop_map(|(r, h, ttl_secs)| Op::Acquire { r, h, ttl_secs }),
        (r.clone(), h.clone(), 1..60u8).prop_map(|(r, h, ttl_secs)| Op::Renew { r, h, ttl_secs }),
        (r.clone(), h.clone()).prop_map(|(r, h)| Op::Release { r, h }),
        (1..120u8).prop_map(|secs| Op::Advance { secs }),
        (r, h).prop_map(|(r, h)| Op::Write { r, h }),
    ]
}

fn rid(r: u8) -> String {
    format!("res-{r}")
}

fn hid(h: u8) -> String {
    format!("holder-{h}")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn lease_fencing_invariants(ops in prop::collection::vec(op_strategy(), 1..120)) {
        let dir = tempfile::tempdir().unwrap();
        let wal = LeaseWal::new(dir.path()).unwrap();
        let mut table = LeaseTable::new();
        let mut now: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap();

        // One fencing guard per resource.
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
                    let reclaim = table.get(&r).is_some_and(|l| l.is_expired(now));
                    if let Ok(lease) = table.acquire(&r, &h, Duration::from_secs(ttl_secs as u64), now) {
                        // Invariant 1: strictly above everything issued before.
                        let prior = issued.entry(r.clone()).or_default();
                        prop_assert!(prior.iter().all(|t| lease.fencing_token > *t),
                            "token {:?} not above prior {:?}", lease.fencing_token, prior);
                        prior.push(lease.fencing_token);
                        believed.insert((r, h), lease.fencing_token);
                        let event = if reclaim {
                            LeaseEvent::Reclaimed { ts: now, lease }
                        } else {
                            LeaseEvent::Acquired { ts: now, lease }
                        };
                        wal.append(&event).unwrap();
                    }
                }
                Op::Renew { r, h, ttl_secs } => {
                    let (r, h) = (rid(r), hid(h));
                    if let Ok(lease) = table.renew(&r, &h, Duration::from_secs(ttl_secs as u64), now) {
                        // Renewal never mints a token.
                        prop_assert_eq!(believed.get(&(r, h)).copied(), Some(lease.fencing_token));
                        wal.append(&LeaseEvent::Renewed { ts: now, lease }).unwrap();
                    }
                }
                Op::Release { r, h } => {
                    let (r, h) = (rid(r), hid(h));
                    if table.release(&r, &h).is_ok() {
                        believed.remove(&(r.clone(), h.clone()));
                        wal.append(&LeaseEvent::Released { ts: now, resource_id: r, holder_id: h }).unwrap();
                    }
                }
                Op::Advance { secs } => {
                    now += Duration::from_secs(secs as u64);
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
                            "newest token {:?} fenced on {}", token, r);
                    } else if guard.high_water().is_some_and(|hw| hw > token) {
                        // Invariant 3: once a newer epoch has written, the
                        // stale waker is rejected — the AC7 race.
                        prop_assert!(result.is_err(),
                            "stale token {:?} accepted on {} past high-water", token, r);
                    }
                }
            }
        }

        // Invariant 4: the log alone rebuilds the live table.
        let recovered = wal.replay().unwrap();
        prop_assert_eq!(recovered, table);
    }
}
