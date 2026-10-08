use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use tokio::sync::RwLock;

use super::{PulseEvent, SalienceKind};
use crate::config::Config;
use crate::outreach::{self, Decision, OutreachCandidate};
use crate::scheduler::evaluator::{resolve_docs_dir, SchedulerState};
use crate::scheduler::intent::{Intent, IntentOutput, IntentPriority, IntentQueue, IntentSource};

/// Cooldown period for event-sourced intents (prevents death spirals).
const EVENT_COOLDOWN_MINUTES: i64 = 60;

/// Maximum fires of the same event within the breaker window before the
/// channel is held open.
const MAX_CONSECUTIVE_FIRES: u32 = 3;

/// Quiet period after which an event's fire count decays back to zero and an
/// open breaker closes again.
///
/// Must exceed `EVENT_COOLDOWN_MINUTES`, or the count could never reach
/// `MAX_CONSECUTIVE_FIRES` and the breaker would be unreachable.
const BREAKER_RESET_MINUTES: i64 = 6 * 60;

/// Compile-time guard for the relationship above: if the reset window ever
/// drops to or below the cooldown, the fire count can never reach
/// `MAX_CONSECUTIVE_FIRES` and the fire limit silently becomes dead code.
const _: () = assert!(BREAKER_RESET_MINUTES > EVENT_COOLDOWN_MINUTES);

/// What the circuit breaker decided about an incoming event.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BreakerDecision {
    /// Let the event through.
    Pass,
    /// Inside the per-event cooldown window.
    Cooldown { minutes_remaining: i64 },
    /// Breaker is open — the channel fired too often without going quiet.
    Open {
        fires: u32,
        resets_at: DateTime<Utc>,
    },
}

/// Per-event-type breaker state.
#[derive(Debug, Clone)]
struct BreakerEntry {
    /// When this event last successfully queued an intent.
    last_queued: DateTime<Utc>,
    /// Fires since the channel last went quiet for `BREAKER_RESET_MINUTES`.
    fires: u32,
    /// Whether the trip into the open state has already been reported, so the
    /// transition is logged once instead of on every suppressed arrival.
    open_reported: bool,
}

/// Circuit breaker for event-sourced intents.
///
/// Two independent gates per event type:
/// - a **cooldown**, which spaces repeated fires, and
/// - a **fire limit**, which holds the channel open when it keeps firing.
///
/// The fire count decays to zero after `BREAKER_RESET_MINUTES` without a
/// successful queue, so an open breaker always closes again.
///
/// That decay is the fix for a real outage. The count previously only ever
/// incremented — there was no decrement, removal, or reset anywhere — which
/// made the limit a once-per-process quota rather than a rate limit: three
/// fires killed the channel for the lifetime of the listener task, and only a
/// process restart cleared it. In the two weeks before this change three
/// channels died that way (`pipeline_frozen`, `pipeline_alert_LEARNING`,
/// `pipeline_alert_PRAXIS`), so a genuine document overflow could not be
/// reported at all.
///
/// Note the limit counts *fires*, not *unresolved* fires: the listener sees
/// events, never whether the intent it queued was acted on, so going quiet is
/// the only resolution signal available here.
///
/// Since PN-133 the pulse's own events never reach the breaker (see
/// [`is_self_event`]) and Salience bypasses it, so no event type in use today
/// queues through it; it stays as the backstop for any outside event that
/// does.
#[derive(Debug, Default)]
struct EventBreaker {
    entries: HashMap<String, BreakerEntry>,
}

impl EventBreaker {
    /// Decide whether `event_type` may proceed at `now`, decaying a stale fire
    /// count first.
    fn check(&mut self, event_type: &str, now: DateTime<Utc>) -> BreakerDecision {
        let Some(entry) = self.entries.get_mut(event_type) else {
            return BreakerDecision::Pass;
        };

        let elapsed = now - entry.last_queued;

        // Decay first: a channel quiet for the reset window is healthy again
        // regardless of what it did before.
        if entry.fires > 0 && elapsed >= Duration::minutes(BREAKER_RESET_MINUTES) {
            if entry.open_reported {
                tracing::info!(
                    "Event '{}' circuit breaker CLOSED — channel quiet for {} min, resuming",
                    event_type,
                    elapsed.num_minutes()
                );
            }
            entry.fires = 0;
            entry.open_reported = false;
        }

        if elapsed < Duration::minutes(EVENT_COOLDOWN_MINUTES) {
            return BreakerDecision::Cooldown {
                minutes_remaining: EVENT_COOLDOWN_MINUTES - elapsed.num_minutes(),
            };
        }

        if entry.fires >= MAX_CONSECUTIVE_FIRES {
            return BreakerDecision::Open {
                fires: entry.fires,
                resets_at: entry.last_queued + Duration::minutes(BREAKER_RESET_MINUTES),
            };
        }

        BreakerDecision::Pass
    }

    /// Returns true the first time an open breaker is reported, so a trip is
    /// logged as a transition rather than once per suppressed arrival.
    fn should_report_open(&mut self, event_type: &str) -> bool {
        match self.entries.get_mut(event_type) {
            Some(entry) if !entry.open_reported => {
                entry.open_reported = true;
                true
            }
            _ => false,
        }
    }

    /// Record that an event successfully queued an intent.
    fn record_fire(&mut self, event_type: String, now: DateTime<Utc>) {
        let entry = self.entries.entry(event_type).or_insert(BreakerEntry {
            last_queued: now,
            fires: 0,
            open_reported: false,
        });
        entry.last_queued = now;
        entry.fires += 1;
    }
}

/// Listen for events and translate them into queued intents.
pub async fn event_listener(
    mut rx: tokio::sync::broadcast::Receiver<PulseEvent>,
    intent_queue: Arc<RwLock<IntentQueue>>,
    config: Arc<Config>,
    root_dir: PathBuf,
) {
    tracing::info!("Event listener started");

    let max_queue_size = config.autonomy.max_queue_size;

    // Circuit breaker state: cooldown + decaying fire count, per event type.
    let mut breaker = EventBreaker::default();

    // Load evaluator state for structural precondition checks
    let mut eval_state = SchedulerState::load(&root_dir);
    let docs_dir = resolve_docs_dir(&root_dir);

    loop {
        match rx.recv().await {
            Ok(event) => {
                // The pulse's own activity and the monitors' readings of it
                // never wake the pulse (PN-133, D4). They stay on the bus for
                // the ledger; here nothing about them is recorded or saved.
                if is_self_event(&event) {
                    continue;
                }

                let event_type = event.event_type();
                let now = Utc::now();

                // Salience carries its own admission control: the quality
                // gate, quiet hours and per-kind daily caps (PN-94 §2.2–2.5).
                // A blanket 60-minute cooldown on top of those would silently
                // throttle `Blocking`, which the spec requires to be uncapped,
                // and the circuit breaker would retire the channel after three
                // messages. The caps are the control; this is not.
                let self_governed = matches!(event, PulseEvent::Salience { .. });

                // Salience bypasses both breaker gates for the reason above;
                // every other event type is spaced and fire-limited.
                let decision = if self_governed {
                    BreakerDecision::Pass
                } else {
                    breaker.check(&event_type, now)
                };

                match decision {
                    BreakerDecision::Pass => {}
                    BreakerDecision::Cooldown { minutes_remaining } => {
                        tracing::debug!(
                            "Event '{}' on cooldown ({} min remaining), skipping",
                            event_type,
                            minutes_remaining
                        );
                        continue;
                    }
                    BreakerDecision::Open { fires, resets_at } => {
                        // Log the trip as a transition, once, at error level —
                        // a suppression nothing reports is the same failure
                        // class as the alert it replaces. Repeats go to debug
                        // so the one line that matters is not buried.
                        if breaker.should_report_open(&event_type) {
                            tracing::error!(
                                "Event '{}' fired {} times without going quiet — circuit breaker \
                                 OPEN, channel suppressed until {} ({} min); intents from this \
                                 event are being dropped until then",
                                event_type,
                                fires,
                                resets_at.to_rfc3339(),
                                (resets_at - now).num_minutes().max(0)
                            );
                        } else {
                            tracing::debug!(
                                "Event '{}' suppressed — breaker open until {}",
                                event_type,
                                resets_at.to_rfc3339()
                            );
                        }
                        continue;
                    }
                }

                // Shed while isolated: translating events writes intent and
                // evaluator state.
                if crate::server::isolation::is_active(&root_dir) {
                    continue;
                }

                // Outreach admission runs after the isolation shed (it writes
                // outreach.json) and before translation: a candidate that
                // does not clear the gate never becomes an intent at all.
                if self_governed && !admit_salience(&event, &config, &root_dir).await {
                    continue;
                }

                if let Some(intent) = translate_event(&event) {
                    // Delta against disk (reconcile) + refresh the shared copy,
                    // so this push survives a concurrent daemon write and vice versa.
                    let mut pushed = false;
                    let description = intent.description.clone();
                    let merged =
                        crate::scheduler::intent::IntentQueue::save_delta(&root_dir, |q| {
                            pushed = q.push(intent, max_queue_size);
                        });
                    match merged {
                        Ok(merged) => *intent_queue.write().await = merged,
                        Err(e) => tracing::error!("Failed to persist intent queue: {}", e),
                    }
                    if pushed {
                        tracing::info!("Event → intent queued: '{}'", description);

                        eval_state.record_fire(&event_type, &docs_dir);
                        if let Err(e) = eval_state.save(&root_dir) {
                            tracing::error!("Failed to persist evaluator state: {}", e);
                        }

                        // Update cooldown tracking
                        breaker.record_fire(event_type, Utc::now());
                    } else if self_governed {
                        // The outreach already counted against the daily cap
                        // at admission, so a dropped intent spends budget on
                        // a message D never sees. Loud, not debug: this is
                        // the channel silently under-delivering.
                        tracing::warn!(
                            "Admitted outreach not queued (queue full or duplicate) — \
                             the send is already counted against today's cap: '{}'",
                            description
                        );
                    } else {
                        tracing::debug!(
                            "Event intent not queued (full or duplicate): '{}'",
                            description
                        );
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("Event listener lagged, missed {} events", n);
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::info!("Event bus closed, listener stopping");
                break;
            }
        }
    }
}

/// Run the outreach admission on a `Salience` event (PN-94, spec §2.3–§2.5).
///
/// Returns true only when the candidate cleared the quality gate, the quiet
/// window and its daily cap. Every rejection is logged with its reason and
/// recorded in `outreach.json`: a gate whose rejections are invisible cannot
/// be audited by anyone, and the rejection rate is one of the spec's success
/// criteria (§8).
///
/// Any pending cap-tightening notice is delivered here, before the decision
/// is acted on, and is marked announced only once delivery succeeds — a
/// notice lost to a failing webhook must be retried, not assumed seen.
async fn admit_salience(event: &PulseEvent, config: &Arc<Config>, root_dir: &Path) -> bool {
    let Some(candidate) = OutreachCandidate::from_event(event) else {
        return false;
    };

    let admission = outreach::admit_async(
        root_dir.to_path_buf(),
        Arc::clone(config),
        candidate.clone(),
        Utc::now(),
    )
    .await;

    if let Some(ref tightening) = admission.pending_notice {
        announce_tightening(tightening, config, root_dir).await;
    }

    match admission.decision {
        Decision::Admitted { id } => {
            tracing::info!(
                outreach_id = %id,
                kind = %candidate.kind,
                "Outreach admitted: {}",
                candidate.headline
            );
            true
        }
        Decision::Rejected(reason) => {
            tracing::info!(
                kind = %candidate.kind,
                "Outreach rejected ({reason}): {}",
                candidate.headline
            );
            false
        }
    }
}

/// Deliver a cap-tightening notice to D and record that it went out.
///
/// Delivery is reported (unlike the fire-and-forget share router) because a
/// tightening D never learns about is precisely the silent self-throttling
/// spec §2.4 forbids. A failed delivery leaves the announcement unrecorded,
/// so the next candidate of that kind tries again.
async fn announce_tightening(
    tightening: &crate::outreach::feedback::Tightening,
    config: &Config,
    root_dir: &Path,
) {
    let notice = crate::outreach::feedback::tightening_notice(tightening);
    let webhook = config.scheduler.output.share_webhook.as_deref();

    match crate::scheduler::output::deliver_liveness_alert(&notice, webhook).await {
        Ok(()) => {
            if let Err(e) =
                crate::outreach::feedback::mark_announced(root_dir, tightening, Utc::now()).await
            {
                tracing::error!(
                    error = %e,
                    "Cap tightening announced but not recorded — D may be told twice"
                );
            }
        }
        Err(e) => tracing::error!(
            error = %e,
            kind = %tightening.kind,
            "Cap tightening notice undelivered — will retry on the next candidate"
        ),
    }
}

/// Whether `event` reports the pulse's own activity, or a monitor's reading
/// of it, rather than something from outside. Such events never become
/// intents: a pulse woken by its own output is a loop (PN-133, D4).
fn is_self_event(event: &PulseEvent) -> bool {
    matches!(
        event,
        PulseEvent::PostInteraction { .. }
            | PulseEvent::PipelineAlert { .. }
            | PulseEvent::PipelineFrozen { .. }
            | PulseEvent::CognitiveHealthChanged { .. }
            | PulseEvent::PipelineConversionLow { .. }
    )
}

/// Translate an event into an intent. Returns None for events that never
/// queue one.
fn translate_event(event: &PulseEvent) -> Option<Intent> {
    match event {
        // Self-events are dropped before translation (see `is_self_event`).
        PulseEvent::PostInteraction { .. }
        | PulseEvent::PipelineAlert { .. }
        | PulseEvent::PipelineFrozen { .. }
        | PulseEvent::CognitiveHealthChanged { .. }
        | PulseEvent::PipelineConversionLow { .. } => None,

        // PluginStateChanged triggers AWARENESS.md rebuild — handled externally,
        // no intent needed from the event listener.
        PulseEvent::PluginStateChanged { .. } => None,

        // ProviderError notifications are handled directly via output::route_error()
        // in the scheduler — not through the intent system, because the provider
        // is likely down when this fires.
        PulseEvent::ProviderError { .. } => None,

        // Salience has already cleared the outreach admission by the time it
        // gets here (see `admit_salience`), so this only shapes the message.
        // Routing is `Share`: `Call` stays manual until the Discord channel
        // has a track record (spec §2.5, `allow_call_routing = false`).
        PulseEvent::Salience {
            kind,
            thread_id,
            headline,
            evidence,
            confidence,
        } => Some(salience_intent(SalienceIntentParts {
            kind: *kind,
            thread_id: thread_id.as_deref(),
            headline,
            evidence,
            confidence: *confidence,
        })),
    }
}

/// The parts of a `Salience` event that shape its outreach message.
struct SalienceIntentParts<'a> {
    kind: SalienceKind,
    thread_id: Option<&'a str>,
    headline: &'a str,
    evidence: &'a str,
    confidence: f64,
}

/// Build the intent that writes the admitted outreach message.
///
/// The prompt hands the pulse the material it already committed to — the
/// headline, the evidence, the stated cost — and asks it to write the message
/// around them. It is explicitly not asked to reconsider whether to send:
/// that judgement was made mechanically and re-opening it here would put the
/// decision back in the hands of the faculty under audit.
fn salience_intent(parts: SalienceIntentParts<'_>) -> Intent {
    let SalienceIntentParts {
        kind,
        thread_id,
        headline,
        evidence,
        confidence,
    } = parts;

    // Blocking means D's work is stalled pending a decision from him; it
    // jumps the queue for the same reason it is uncapped.
    let priority = if kind == SalienceKind::Blocking {
        IntentPriority::Urgent
    } else {
        IntentPriority::Normal
    };

    let thread_note = thread_id.map_or_else(String::new, |id| {
        format!("\nThis continues thread '{id}' — say so if the continuation is the point.\n")
    });

    Intent {
        id: format!("event-salience-{}", &uuid::Uuid::new_v4().to_string()[..8]),
        description: format!("Outreach ({kind}): {headline}"),
        prompt: format!(
            "You raised this as worth telling D unprompted, and it cleared the outreach \
            quality gate. Write the message.\n\n\
            Kind: {kind}\n\
            Headline: {headline}\n\
            Confidence: {confidence:.2}\n\
            Evidence you cited:\n{evidence}\n{thread_note}\n\
            Write it as a short Discord message and emit it with a [SHARE:] marker. \
            Requirements, all of them load-bearing:\n\
            1. Lead with the claim itself, not with the fact that you are reaching out. \
            No preamble, no 'I've been thinking about'.\n\
            2. Carry the external referent through verbatim — the file and line, URL \
            or measured number. It is the part of this D can check, and \
            the part you did not author.\n\
            3. End with the cost line exactly as stated above: what you are asking for \
            is nothing, a read, or a decision.\n\
            4. Keep it under 150 words. If it does not fit, the claim is not sharp yet.\n\
            5. Do not restate the evidence twice, and do not pad with hedging.\n\n\
            Emit exactly one [SHARE:] marker. Do not queue intents or schedule tasks \
            from this — it is one message, not a work item.",
        ),
        source: IntentSource::Event(format!("salience_{kind}")),
        priority,
        created_at: Utc::now(),
        chain: None,
        output_routing: IntentOutput::Share,
        depth: 0,
    }
}

#[cfg(test)]
mod breaker_tests {
    use super::*;

    const EV: &str = "outside_event_a";

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-15T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Drive the breaker to the fire limit, honouring the cooldown between
    /// fires. Returns the time of the last fire.
    fn trip(breaker: &mut EventBreaker, start: DateTime<Utc>) -> DateTime<Utc> {
        let mut now = start;
        for _ in 0..MAX_CONSECUTIVE_FIRES {
            assert_eq!(breaker.check(EV, now), BreakerDecision::Pass);
            breaker.record_fire(EV.to_string(), now);
            now += Duration::minutes(EVENT_COOLDOWN_MINUTES);
        }
        now - Duration::minutes(EVENT_COOLDOWN_MINUTES)
    }

    #[test]
    fn unknown_event_passes() {
        let mut b = EventBreaker::default();
        assert_eq!(b.check(EV, t0()), BreakerDecision::Pass);
    }

    #[test]
    fn cooldown_blocks_rapid_refire() {
        let mut b = EventBreaker::default();
        b.record_fire(EV.to_string(), t0());

        match b.check(EV, t0() + Duration::minutes(10)) {
            BreakerDecision::Cooldown { minutes_remaining } => {
                assert_eq!(minutes_remaining, EVENT_COOLDOWN_MINUTES - 10);
            }
            other => panic!("expected cooldown, got {:?}", other),
        }

        // Once the window elapses the event is allowed through again.
        assert_eq!(
            b.check(EV, t0() + Duration::minutes(EVENT_COOLDOWN_MINUTES)),
            BreakerDecision::Pass
        );
    }

    #[test]
    fn opens_after_max_fires() {
        let mut b = EventBreaker::default();
        let last = trip(&mut b, t0());

        match b.check(EV, last + Duration::minutes(EVENT_COOLDOWN_MINUTES)) {
            BreakerDecision::Open { fires, resets_at } => {
                assert_eq!(fires, MAX_CONSECUTIVE_FIRES);
                assert_eq!(resets_at, last + Duration::minutes(BREAKER_RESET_MINUTES));
            }
            other => panic!("expected open breaker, got {:?}", other),
        }
    }

    /// The regression test for the outage: before the decay existed, the fire
    /// count only ever incremented, so an open breaker stayed open for the
    /// lifetime of the listener task and the channel was silently dead.
    #[test]
    fn closes_after_quiet_period_rather_than_staying_dead_forever() {
        let mut b = EventBreaker::default();
        let last = trip(&mut b, t0());

        // Still open just before the reset window elapses.
        assert!(matches!(
            b.check(EV, last + Duration::minutes(BREAKER_RESET_MINUTES - 1)),
            BreakerDecision::Open { .. }
        ));

        // Closed once it does.
        assert_eq!(
            b.check(EV, last + Duration::minutes(BREAKER_RESET_MINUTES)),
            BreakerDecision::Pass
        );

        // And the channel is genuinely usable again, not merely one-shot.
        let reopened = last + Duration::minutes(BREAKER_RESET_MINUTES);
        b.record_fire(EV.to_string(), reopened);
        assert_eq!(b.entries[EV].fires, 1);
        assert!(!b.entries[EV].open_reported);
    }

    /// A day later — the state the live process was stuck in — must be usable.
    #[test]
    fn recovers_a_day_after_tripping() {
        let mut b = EventBreaker::default();
        let last = trip(&mut b, t0());
        assert_eq!(
            b.check(EV, last + Duration::hours(24)),
            BreakerDecision::Pass
        );
    }

    #[test]
    fn quiet_channel_decays_before_reaching_the_limit() {
        let mut b = EventBreaker::default();
        b.record_fire(EV.to_string(), t0());
        b.record_fire(
            EV.to_string(),
            t0() + Duration::minutes(EVENT_COOLDOWN_MINUTES),
        );
        assert_eq!(b.entries[EV].fires, 2);

        // A quiet gap wipes the partial count, so unrelated fires spread over
        // weeks never accumulate into a trip.
        assert_eq!(
            b.check(EV, t0() + Duration::minutes(BREAKER_RESET_MINUTES * 2)),
            BreakerDecision::Pass
        );
        assert_eq!(b.entries[EV].fires, 0);
    }

    #[test]
    fn open_transition_is_reported_once_then_stays_quiet() {
        let mut b = EventBreaker::default();
        trip(&mut b, t0());

        assert!(b.should_report_open(EV), "first trip must be reported");
        assert!(
            !b.should_report_open(EV),
            "repeat suppressions must not re-report"
        );
    }

    #[test]
    fn recovery_rearms_the_open_report() {
        let mut b = EventBreaker::default();
        let last = trip(&mut b, t0());
        assert!(b.should_report_open(EV));

        // Decay clears the reported flag, so a second outage is announced too.
        assert_eq!(
            b.check(EV, last + Duration::minutes(BREAKER_RESET_MINUTES)),
            BreakerDecision::Pass
        );
        let last2 = trip(&mut b, last + Duration::minutes(BREAKER_RESET_MINUTES));
        assert!(matches!(
            b.check(EV, last2 + Duration::minutes(EVENT_COOLDOWN_MINUTES)),
            BreakerDecision::Open { .. }
        ));
        assert!(
            b.should_report_open(EV),
            "a second outage must be announced"
        );
    }

    #[test]
    fn breakers_are_tracked_per_event_type() {
        let mut b = EventBreaker::default();
        trip(&mut b, t0());
        let other = "outside_event_b";

        // One dead channel must not silence a different event type.
        assert_eq!(
            b.check(other, t0() + Duration::hours(3)),
            BreakerDecision::Pass
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ConversationTrust, InteractionSource};

    /// One of each event that reports the pulse's own activity.
    fn self_events() -> Vec<PulseEvent> {
        vec![
            PulseEvent::PostInteraction {
                source: InteractionSource::Chat {
                    channel: "discord".to_string(),
                },
                trust: ConversationTrust::Owner,
                summary: "Discussed architecture.".to_string(),
                input_tokens: 5_000,
                output_tokens: 2_000,
            },
            PulseEvent::PipelineAlert {
                document: "LEARNING".to_string(),
                count: 40,
                hard_limit: 30,
            },
            PulseEvent::PipelineFrozen {
                sessions_without_movement: 9,
            },
            PulseEvent::CognitiveHealthChanged {
                previous: "HEALTHY".to_string(),
                current: "ALERT".to_string(),
                suggestions: vec!["slow down".to_string()],
            },
            PulseEvent::PipelineConversionLow {
                conversations_7d: 12,
                pipeline_updates_7d: 0,
            },
        ]
    }

    #[test]
    fn self_events_never_translate_to_intents() {
        for event in self_events() {
            assert!(is_self_event(&event), "{}", event.event_type());
            assert!(translate_event(&event).is_none(), "{}", event.event_type());
        }
        assert!(!is_self_event(&salience(SalienceKind::Finding)));
    }

    /// The listener drops self-events before the breaker and the evaluator
    /// state: no intent is queued and `scheduler_state.json` is not touched.
    #[tokio::test]
    async fn self_events_create_no_intents_and_write_no_scheduler_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state_path = root.join("scheduler_state.json");
        SchedulerState::default().save(&root).unwrap();
        let state_before = std::fs::read(&state_path).unwrap();

        let queue = Arc::new(RwLock::new(IntentQueue::load(&root)));
        let config = Arc::new(crate::config::test_support::minimal_config());
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let listener = tokio::spawn(event_listener(rx, Arc::clone(&queue), config, root.clone()));

        // Twice each: a second arrival would have hit a breaker entry or an
        // evaluator record if the first had left one.
        for _ in 0..2 {
            for event in self_events() {
                tx.send(event).unwrap();
            }
        }
        drop(tx);
        listener.await.unwrap();

        assert!(queue.read().await.is_empty());
        assert!(IntentQueue::load(&root).is_empty());
        assert_eq!(std::fs::read(&state_path).unwrap(), state_before);
    }

    fn salience(kind: SalienceKind) -> PulseEvent {
        PulseEvent::Salience {
            kind,
            thread_id: None,
            headline: "The gate rejects self-authored evidence".to_string(),
            evidence: "src/outreach/mod.rs:312\nCost: read".to_string(),
            confidence: 0.8,
        }
    }

    #[test]
    fn salience_still_becomes_a_share_intent() {
        let intent = translate_event(&salience(SalienceKind::Finding))
            .expect("an admitted salience event must produce an intent");
        assert_eq!(intent.output_routing, IntentOutput::Share);
        assert!(intent.description.starts_with("Outreach (finding)"));
        assert!(intent.prompt.contains("[SHARE:]"));
        assert!(
            matches!(&intent.source, IntentSource::Event(name) if name == "salience_finding"),
            "{:?}",
            intent.source
        );
    }

    #[test]
    fn salience_carries_the_referent_and_the_cost_into_the_prompt() {
        // The referent is the part D can check and the pulse did not author;
        // dropping it here would undo gate 2 one step after it passed.
        let intent = translate_event(&salience(SalienceKind::Callback)).unwrap();
        assert!(intent.prompt.contains("src/outreach/mod.rs:312"));
        assert!(intent.prompt.contains("Cost: read"));
    }

    #[test]
    fn blocking_outreach_jumps_the_queue() {
        let blocking = translate_event(&salience(SalienceKind::Blocking)).unwrap();
        assert_eq!(blocking.priority, IntentPriority::Urgent);

        let finding = translate_event(&salience(SalienceKind::Finding)).unwrap();
        assert_eq!(finding.priority, IntentPriority::Normal);
    }

    #[test]
    fn salience_is_not_gated_by_the_generic_events_config() {
        // Outreach is governed by [outreach] enabled, never by
        // [autonomy.events]: the only switch left there (provider_error)
        // does not reach translation at all, and Salience is not a
        // self-event, so the listener lets it through.
        let event = salience(SalienceKind::Finding);
        assert!(!is_self_event(&event));
        assert!(translate_event(&event).is_some());
    }

    #[test]
    fn salience_thread_id_is_mentioned_only_when_present() {
        let mut event = salience(SalienceKind::Development);
        assert!(!translate_event(&event)
            .unwrap()
            .prompt
            .contains("continues thread"));

        if let PulseEvent::Salience {
            ref mut thread_id, ..
        } = event
        {
            *thread_id = Some("tension-7".to_string());
        }
        assert!(translate_event(&event)
            .unwrap()
            .prompt
            .contains("tension-7"));
    }
}
