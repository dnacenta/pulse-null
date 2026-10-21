//! Paired-fact turns (PN-144): each due side asked in its own turn (the
//! planted side and the twin of the pulse, the planted side again of the
//! fresh arm for the chance baseline), scored by side, and logged in both
//! `items.jsonl` and `facts.jsonl`.

use serde_json::json;

use super::{RunError, Settled, Tally, TurnContext, ANSWER_MAX_BYTES};
use crate::cli_provider::CapturedRun;
use crate::vigil::probe::paired::{self, Ask, LoadedPairs, SideRole};
use crate::vigil::probe::record::{Stream, RECORD_VERSION};
use crate::vigil::probe::score::Outcome;
use crate::vigil::probe::sets::Arm;

/// Why a twin's answers are void when the owner talked about it.
const TWIN_MENTIONED: &str = "an owner message named the twin's slot";

/// The arm that asks a side: the pulse for the planted side and the twin,
/// the fresh arm for the volunteered and the forced-guess asks.
fn paired_arm(side: SideRole) -> Arm {
    match side {
        SideRole::Chance | SideRole::Guess => Arm::Fresh,
        SideRole::Planted | SideRole::Twin => Arm::Pulse,
    }
}

/// One paired turn's fixed facts.
struct PairedTurn<'a> {
    ask: &'a Ask,
    label: String,
    /// `<fact>-<offset>d-<side>`.
    item: String,
    /// The fields every row of the turn carries.
    base: serde_json::Value,
    question: &'a str,
    prompt_sha256: &'a str,
    components: &'a serde_json::Value,
}

impl PairedTurn<'_> {
    /// A row of the turn: its base fields with `extra`'s added.
    fn row(&self, extra: serde_json::Value) -> serde_json::Value {
        super::merged(self.base.clone(), extra)
    }
}

impl TurnContext<'_> {
    /// Ask one side of a due paired fact. Returns its outcome, or `None` when
    /// the run stopped at this turn.
    pub(super) async fn run_paired(
        &self,
        pairs: &LoadedPairs,
        ask: &Ask,
        tally: &mut Tally,
    ) -> Result<Option<Outcome>, RunError> {
        let arm = paired_arm(ask.side);
        let asked = self.ask(arm, &ask.question, tally).await?;
        let label = pairs.label();
        let item = format!("{}-{}d-{}", ask.fact, ask.offset_days, ask.side.as_str());
        let base = self.paired_row(pairs, ask, arm, &item, &asked.prompt_sha256);
        let turn = PairedTurn {
            ask,
            label,
            item,
            base,
            question: &asked.question,
            prompt_sha256: &asked.prompt_sha256,
            components: &asked.components,
        };
        match self.settle(&turn.label, arm, asked.result, tally)? {
            Settled::Stopped => Ok(None),
            Settled::Failed(error) => self.record_paired_error(&turn, &error, tally),
            Settled::Answered(captured) => self.record_paired_answer(&turn, &captured, tally),
        }
    }

    /// The fields every row of a paired turn carries.
    fn paired_row(
        &self,
        pairs: &LoadedPairs,
        ask: &Ask,
        arm: Arm,
        item: &str,
        prompt_sha256: &str,
    ) -> serde_json::Value {
        super::merged(
            json!({
                "v": RECORD_VERSION,
                "run": self.run_id,
                "at": self.now().to_rfc3339(),
                "set": pairs.set.set,
                "set_version": pairs.set.version,
                "set_sha256": pairs.sha256,
                "file_sha256": pairs.file_sha256,
                "measure": "paired-facts",
                "item": item,
                "arm": arm,
                "side": ask.side.as_str(),
                "fact": ask.fact,
                "offset_days": ask.offset_days,
                // The label is the pre-registered offset; this is when the
                // ask actually came, which a missed run makes later.
                "elapsed_days": (self.now() - ask.planted_at).num_days(),
                "prompt_sha256": prompt_sha256,
            }),
            self.lineage.fields(),
        )
    }

    /// A later turn failed: the side's answer is an error.
    fn record_paired_error(
        &self,
        turn: &PairedTurn<'_>,
        error: &str,
        tally: &mut Tally,
    ) -> Result<Option<Outcome>, RunError> {
        self.record.append(
            Stream::Items,
            turn.row(json!({
                "outcome": Outcome::Error,
                "error": crate::utils::safe_truncate(error, ANSWER_MAX_BYTES),
            })),
        )?;
        tally.items += 1;
        Ok(Some(Outcome::Error))
    }

    /// Score the side's answer and record it with its receipt.
    fn record_paired_answer(
        &self,
        turn: &PairedTurn<'_>,
        captured: &CapturedRun,
        tally: &mut Tally,
    ) -> Result<Option<Outcome>, RunError> {
        let ask = turn.ask;
        let void = self.paired_void(ask, captured);
        let scored = paired::score_answer(ask, &captured.reply);
        let outcome = if void.is_some() {
            Outcome::Void
        } else {
            scored
        };
        let receipt = self.record.write_receipt(
            self.run_id,
            &format!("paired-facts-{}", turn.item),
            &json!({
                "run": self.run_id,
                "set": turn.label,
                "fact": ask.fact,
                "side": ask.side.as_str(),
                "offset_days": ask.offset_days,
                "system_prompt_sha256": turn.prompt_sha256,
                "system_prompt_components": turn.components,
                "question": turn.question,
                "reply": captured.reply,
                "value": ask.value,
                "served_model": captured.served_model,
                "tool_calls": captured.tool_calls,
                "void": void,
                "raw": captured.raw,
            }),
        )?;
        self.record.append(
            Stream::Items,
            turn.row(json!({
                "outcome": outcome,
                "served_model": captured.served_model,
                "answer": crate::utils::safe_truncate(captured.reply.trim(), ANSWER_MAX_BYTES),
                "truth": ask.value,
                "truth_source": "probe facts plant (confirmed in an owner message)",
                "answered_from": paired::answered_from(ask, &captured.tool_calls, self.adapter.observes_tools()),
                "answer_kind": paired::answer_kind(ask, &captured.reply),
                "tool_calls": captured.tool_calls.len(),
                "observes_tools": self.adapter.observes_tools(),
                "void_reason": void,
                "receipt": receipt.strip_prefix(self.record.dir()).unwrap_or(receipt.as_path()),
            })),
        )?;
        tally.items += 1;
        Ok(Some(outcome))
    }

    /// Why the side's answers are void: the owner talked about the twin, or
    /// a tool call the CLI showed.
    fn paired_void(&self, ask: &Ask, captured: &CapturedRun) -> Option<String> {
        if ask.contaminated {
            Some(TWIN_MENTIONED.to_string())
        } else {
            self.void_of(captured)
        }
    }

    /// Log that a fact was asked at an offset (its asks), and any offsets
    /// that came due and were passed over.
    pub(super) fn record_asked(
        &self,
        ask: &Ask,
        outcomes: [Option<Outcome>; paired::ASKS_PER_FACT],
    ) -> Result<(), RunError> {
        let [planted, twin, chance, guess] = outcomes;
        for missed in &ask.missed {
            self.record.append(
                Stream::Facts,
                json!({
                    "v": RECORD_VERSION,
                    "event": "missed",
                    "fact": ask.fact,
                    "offset_days": missed,
                    "at": self.now().to_rfc3339(),
                    "run": self.run_id,
                }),
            )?;
        }
        self.record.append(
            Stream::Facts,
            json!({
                "v": RECORD_VERSION,
                "event": "ask",
                "fact": ask.fact,
                "offset_days": ask.offset_days,
                "at": self.now().to_rfc3339(),
                "run": self.run_id,
                "planted": planted,
                "twin": twin,
                "chance": chance,
                "guess": guess,
            }),
        )?;
        Ok(())
    }
}
