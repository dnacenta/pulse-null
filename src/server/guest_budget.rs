//! What a plugin's guests may cost (PN-110 audit R2-5).
//!
//! The per-plugin pool and rate bucket bound how fast a plugin runs turns,
//! not how many: a caller who keeps talking, or a Discord user who keeps
//! posting, could still run paid turns all day and crowd everyone else out.
//! [`GuestLedger`] adds three caps from `[plugin_turns]`, for guests only —
//! the owner's turns are never counted:
//!
//! ```text
//!   daily_turns            every guest turn of the plugin, per UTC day
//!   voice_turns_per_call   one voice call (its session key)
//!   guest_burst / _per_minute  one sender on a text channel, a token bucket
//! ```
//!
//! The ledger is in memory and bounded: at most [`MAX_TRACKED`] calls and
//! senders each, the least recently seen forgotten first.

use std::collections::HashMap;
use std::time::Instant;

use chrono::NaiveDate;

use crate::config::PluginTurnsConfig;

/// Calls, and senders, tracked at once per plugin.
pub const MAX_TRACKED: usize = 1024;

/// Which per-guest cap a turn falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestKind {
    /// A voice call: a cap on its turns.
    Call,
    /// A sender on a text channel: a token bucket.
    Sender,
}

/// Why a guest's turn was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestRefusal {
    /// The plugin spent its guest turns for today.
    DailyBudgetSpent,
    /// This call took all its turns.
    CallTurnsSpent,
    /// This sender is posting faster than its bucket refills.
    SenderRateLimited,
}

#[derive(Debug, Clone, Copy)]
struct CallUse {
    turns: u32,
    seen: Instant,
}

#[derive(Debug, Clone, Copy)]
struct SenderBucket {
    tokens: f64,
    seen: Instant,
}

/// One plugin's guest turns: today's count, and each call's and sender's.
#[derive(Debug, Default)]
pub struct GuestLedger {
    day: Option<NaiveDate>,
    turns_today: u32,
    calls: HashMap<String, CallUse>,
    senders: HashMap<String, SenderBucket>,
}

impl GuestLedger {
    /// Admit one turn for guest `who` of `kind`, at `now` on `today`, or
    /// say which cap refuses it. An admitted turn is counted at once: the
    /// caps bound turns started, not turns that succeeded.
    pub fn admit(
        &mut self,
        who: &str,
        kind: GuestKind,
        limits: &PluginTurnsConfig,
        now: Instant,
        today: NaiveDate,
    ) -> Result<(), GuestRefusal> {
        if self.day != Some(today) {
            self.day = Some(today);
            self.turns_today = 0;
        }
        if limits.daily_turns > 0 && self.turns_today >= limits.daily_turns {
            return Err(GuestRefusal::DailyBudgetSpent);
        }
        match kind {
            GuestKind::Call => self.take_call_turn(who, limits.voice_turns_per_call, now)?,
            GuestKind::Sender => self.take_sender_token(who, limits, now)?,
        }
        self.turns_today += 1;
        Ok(())
    }

    fn take_call_turn(&mut self, who: &str, cap: u32, now: Instant) -> Result<(), GuestRefusal> {
        if !self.calls.contains_key(who) {
            forget_oldest(&mut self.calls, |c| c.seen);
        }
        let call = self.calls.entry(who.to_string()).or_insert(CallUse {
            turns: 0,
            seen: now,
        });
        call.seen = now;
        if cap > 0 && call.turns >= cap {
            return Err(GuestRefusal::CallTurnsSpent);
        }
        call.turns += 1;
        Ok(())
    }

    fn take_sender_token(
        &mut self,
        who: &str,
        limits: &PluginTurnsConfig,
        now: Instant,
    ) -> Result<(), GuestRefusal> {
        if limits.guest_burst == 0 {
            return Ok(());
        }
        let burst = f64::from(limits.guest_burst);
        if !self.senders.contains_key(who) {
            forget_oldest(&mut self.senders, |b| b.seen);
        }
        let bucket = self.senders.entry(who.to_string()).or_insert(SenderBucket {
            tokens: burst,
            seen: now,
        });
        let refill = now.duration_since(bucket.seen).as_secs_f64() * limits.guest_per_minute / 60.0;
        bucket.tokens = (bucket.tokens + refill).min(burst);
        bucket.seen = now;
        if bucket.tokens < 1.0 {
            return Err(GuestRefusal::SenderRateLimited);
        }
        bucket.tokens -= 1.0;
        Ok(())
    }
}

/// Make room for one more entry: drop the least recently seen when full.
fn forget_oldest<V>(map: &mut HashMap<String, V>, seen: impl Fn(&V) -> Instant) {
    if map.len() < MAX_TRACKED {
        return;
    }
    if let Some(oldest) = map
        .iter()
        .min_by_key(|(_, v)| seen(v))
        .map(|(k, _)| k.clone())
    {
        map.remove(&oldest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn limits(daily: u32, per_call: u32, burst: u32, per_minute: f64) -> PluginTurnsConfig {
        PluginTurnsConfig {
            daily_turns: daily,
            voice_turns_per_call: per_call,
            guest_burst: burst,
            guest_per_minute: per_minute,
        }
    }

    fn day(n: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, n).unwrap()
    }

    #[test]
    fn a_call_gets_its_turns_and_no_more() {
        let mut ledger = GuestLedger::default();
        let l = limits(0, 3, 5, 6.0);
        let now = Instant::now();
        for _ in 0..3 {
            assert_eq!(
                ledger.admit("call-a", GuestKind::Call, &l, now, day(18)),
                Ok(())
            );
        }
        assert_eq!(
            ledger.admit("call-a", GuestKind::Call, &l, now, day(18)),
            Err(GuestRefusal::CallTurnsSpent)
        );
        // Another call is not touched by the first one's cap.
        assert_eq!(
            ledger.admit("call-b", GuestKind::Call, &l, now, day(18)),
            Ok(())
        );
    }

    #[test]
    fn a_sender_bucket_refills_with_time() {
        let mut ledger = GuestLedger::default();
        let l = limits(0, 0, 2, 6.0); // one token per 10 s
        let t0 = Instant::now();
        assert_eq!(
            ledger.admit("2002", GuestKind::Sender, &l, t0, day(18)),
            Ok(())
        );
        assert_eq!(
            ledger.admit("2002", GuestKind::Sender, &l, t0, day(18)),
            Ok(())
        );
        assert_eq!(
            ledger.admit("2002", GuestKind::Sender, &l, t0, day(18)),
            Err(GuestRefusal::SenderRateLimited)
        );
        // Another author has a bucket of their own.
        assert_eq!(
            ledger.admit("3003", GuestKind::Sender, &l, t0, day(18)),
            Ok(())
        );
        let later = t0 + Duration::from_secs(10);
        assert_eq!(
            ledger.admit("2002", GuestKind::Sender, &l, later, day(18)),
            Ok(())
        );
        assert_eq!(
            ledger.admit("2002", GuestKind::Sender, &l, later, day(18)),
            Err(GuestRefusal::SenderRateLimited)
        );
    }

    #[test]
    fn the_daily_budget_covers_every_guest_and_resets_at_midnight() {
        let mut ledger = GuestLedger::default();
        let l = limits(3, 0, 0, 0.0);
        let now = Instant::now();
        assert_eq!(
            ledger.admit("call-a", GuestKind::Call, &l, now, day(18)),
            Ok(())
        );
        assert_eq!(
            ledger.admit("2002", GuestKind::Sender, &l, now, day(18)),
            Ok(())
        );
        assert_eq!(
            ledger.admit("call-b", GuestKind::Call, &l, now, day(18)),
            Ok(())
        );
        assert_eq!(
            ledger.admit("call-c", GuestKind::Call, &l, now, day(18)),
            Err(GuestRefusal::DailyBudgetSpent)
        );
        assert_eq!(
            ledger.admit("call-c", GuestKind::Call, &l, now, day(19)),
            Ok(())
        );
    }

    #[test]
    fn a_refused_turn_is_not_counted_against_the_day() {
        let mut ledger = GuestLedger::default();
        let l = limits(2, 1, 0, 0.0);
        let now = Instant::now();
        assert_eq!(
            ledger.admit("call-a", GuestKind::Call, &l, now, day(18)),
            Ok(())
        );
        assert!(ledger
            .admit("call-a", GuestKind::Call, &l, now, day(18))
            .is_err());
        assert_eq!(
            ledger.admit("call-b", GuestKind::Call, &l, now, day(18)),
            Ok(())
        );
    }

    #[test]
    fn zero_turns_a_cap_off() {
        let mut ledger = GuestLedger::default();
        let l = limits(0, 0, 0, 0.0);
        let now = Instant::now();
        for _ in 0..100 {
            assert_eq!(ledger.admit("c", GuestKind::Call, &l, now, day(18)), Ok(()));
            assert_eq!(
                ledger.admit("s", GuestKind::Sender, &l, now, day(18)),
                Ok(())
            );
        }
    }

    #[test]
    fn the_ledger_forgets_the_least_recently_seen_when_full() {
        let mut ledger = GuestLedger::default();
        let l = limits(0, 1, 0, 0.0);
        let t0 = Instant::now();
        for i in 0..MAX_TRACKED {
            let at = t0 + Duration::from_millis(i as u64);
            ledger
                .admit(&format!("call-{i}"), GuestKind::Call, &l, at, day(18))
                .unwrap();
        }
        let later = t0 + Duration::from_secs(60);
        ledger
            .admit("call-new", GuestKind::Call, &l, later, day(18))
            .unwrap();
        assert_eq!(ledger.calls.len(), MAX_TRACKED);
        assert!(
            !ledger.calls.contains_key("call-0"),
            "the oldest was forgotten"
        );
    }
}
