//! Paired facts (PN-144): does the pulse remember what its owner told it,
//! and admit what it was never told? App extension of E5 (mobile-app spec
//! §1a.2 "Remembers").
//!
//! ```text
//!  probe facts draw            runtime picks a pair and the side to plant  → facts: draw
//!       │  owner mentions that side, in normal conversation with the pulse
//!  probe facts plant <id> <v>  runtime finds <v> in an owner message after
//!                              the draw (WAL, then archives)               → facts: plant
//!       │
//!  probe run (1, 7, 30, 90 days after the plant)
//!       both sides asked, each its own turn, pulse arm only               → items + facts: ask
//! ```
//!
//! The side never planted is the baseline that can lose (G4): any answer to
//! it other than an abstention is made up. A twin whose words show up in an
//! owner message is flagged and its answers are void. Every value and
//! every row lives in the held-out record.

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::owner;
use super::record::{Record, Stream, RECORD_VERSION};
use super::score::{self, AnswerKind, Outcome};
use super::scratch::hex;
use super::sealed::InstalledSets;
use crate::cli_provider::adapter::ToolCall;

/// What the fresh arm is told before the planted question in the forced
/// guess (R3-A): no UNKNOWN to fall back on, so its hits are what a model
/// with no memory gets by guessing (the chance rate). Pre-registered on the
/// paired-facts methods page; its digest is in the set's definition.
pub const GUESS_INSTRUCTIONS: &str = "Guess: you were never told the answer to the question \
below. Reply with just your best guess at it, a single value. Do not reply UNKNOWN: here a \
guess is always the answer.";

/// The digest of [`GUESS_INSTRUCTIONS`].
#[must_use]
pub fn guess_sha256() -> String {
    hex(&Sha256::digest(GUESS_INSTRUCTIONS.as_bytes()))
}

/// Days the owner has to plant a drawn fact before the draw is abandoned.
/// INITIAL GUESS: two weeks is long enough to bring a topic up in normal
/// conversation without forcing it, and short enough that an abandoned
/// draw does not hold one of the few in-flight slots for long.
pub const PLANT_WINDOW_DAYS: i64 = 14;

/// Offsets whose results are exploratory in v1, not confirmatory: with at
/// most three facts in flight a batch of ten at 30 or 90 days takes about
/// a year to fill (`docs/methods/paired-facts.md`, "Settings"), so no
/// decision is read off them; "Remembers" is decided on 1 and 7 days.
pub const EXPLORATORY_OFFSETS_DAYS: &[i64] = &[30, 90];

/// Is `offset` one of the v1 offsets read as exploratory only?
#[must_use]
pub fn is_exploratory(offset: i64) -> bool {
    EXPLORATORY_OFFSETS_DAYS.contains(&offset)
}

/// The sealed file of the pair set.
pub const PAIRS_FILE: &str = "paired-facts.toml";

/// One side of a pair.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Side {
    /// What the side is, for receipts and `probe facts list`.
    pub slot: String,
    /// How the owner is asked to mention it ("your favourite sundial").
    pub ask_owner: String,
    /// What the pulse is asked.
    pub question: String,
    /// Phrases of two words or more that, in an owner message, mean the
    /// side was talked about ("favourite sundial", not "sundial").
    pub keywords: Vec<String>,
}

/// A pair of sides; one gets planted, the other is the twin.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    pub id: String,
    pub a: Side,
    pub b: Side,
}

/// The pair set as written in its file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairSet {
    pub set: String,
    pub version: u32,
    pub min_n: u32,
    pub offsets_days: Vec<i64>,
    pub max_active: usize,
    pub instructions: String,
    #[serde(rename = "pair")]
    pub pairs: Vec<Pair>,
}

/// The parsed pair set, its file's digest, and the key the record's
/// assignments are committed with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedPairs {
    pub set: PairSet,
    /// The set's definition: its file, the fresh arm's line the chance asks
    /// are put with, and the forced guess's instructions.
    pub sha256: String,
    /// The set file's own digest.
    pub file_sha256: String,
    fact_key: String,
}

impl LoadedPairs {
    /// The installed (sealed) pair set.
    pub fn from_installed(installed: &InstalledSets) -> Result<Self, String> {
        let mut pairs = Self::parse(installed.text(PAIRS_FILE).map_err(|e| e.to_string())?)?;
        pairs.fact_key = installed.fact_key().to_string();
        Ok(pairs)
    }

    /// The commitment a draw row carries for `pair` and its planted side:
    /// SHA-256 over the fact key, the fact and the assignment. A reader of
    /// the record learns nothing of which pair was drawn or which side is
    /// the never-told twin; the runner, holding the key, recovers both
    /// (audit R2-11). The live pulse's uid cannot read the record.
    fn assignment(&self, fact: &str, pair: &str, planted: Which) -> String {
        let side = match planted {
            Which::A => "a",
            Which::B => "b",
        };
        hex(&Sha256::digest(
            format!(
                "pulse-null probe fact v1\n{}\n{fact}\n{pair}\n{side}",
                self.fact_key
            )
            .as_bytes(),
        ))
    }

    /// The pair and planted side behind a draw row's commitment.
    fn resolve(&self, fact: &str, assignment: &str) -> Option<(String, Which)> {
        self.set.pairs.iter().find_map(|pair| {
            [Which::A, Which::B]
                .into_iter()
                .find(|w| self.assignment(fact, &pair.id, *w) == assignment)
                .map(|w| (pair.id.clone(), w))
        })
    }

    /// The fixture pairs, for tests.
    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self::from_installed(&super::sealed::tests::fixture_sets()).unwrap()
    }

    fn parse(text: &str) -> Result<Self, String> {
        let set: PairSet =
            toml::from_str(text).map_err(|e| crate::plugins::config_error::describe(&e))?;
        if set.pairs.is_empty() || set.offsets_days.is_empty() {
            return Err("paired-facts set has no pairs or no offsets".into());
        }
        for side in set.pairs.iter().flat_map(|p| [&p.a, &p.b]) {
            if side.keywords.is_empty()
                || side
                    .keywords
                    .iter()
                    .any(|k| k.split_whitespace().count() < 2)
            {
                return Err(format!(
                    "paired-facts side '{}' needs keyword phrases of two words or more",
                    side.slot
                ));
            }
        }
        let file_sha256 = hex(&Sha256::digest(text.as_bytes()));
        let definition = format!(
            "{file_sha256}\nfresh {}\nguess {}",
            super::sets::fresh_sha256(),
            guess_sha256()
        );
        Ok(Self {
            set,
            sha256: hex(&Sha256::digest(definition.as_bytes())),
            file_sha256,
            fact_key: String::new(),
        })
    }

    #[must_use]
    pub fn label(&self) -> String {
        format!("{}/v{}", self.set.set, self.set.version)
    }

    fn pair(&self, id: &str) -> Option<&Pair> {
        self.set.pairs.iter().find(|p| p.id == id)
    }

    /// The prompt for one side.
    #[must_use]
    pub fn prompt(&self, side: &Side) -> String {
        format!("{}\n\n{}", self.set.instructions.trim(), side.question)
    }

    /// The forced-guess prompt for `side`: [`GUESS_INSTRUCTIONS`], then the
    /// side's question.
    #[must_use]
    pub fn guess_prompt(side: &Side) -> String {
        format!("{GUESS_INSTRUCTIONS}\n\n{}", side.question)
    }
}

/// Which side of the pair was planted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Which {
    A,
    B,
}

/// A fact as its rows in `facts.jsonl` describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub fact: String,
    pub pair: String,
    pub planted: Which,
    pub drawn_at: DateTime<Utc>,
    /// The value the owner planted, once confirmed.
    pub value: Option<String>,
    pub planted_at: Option<DateTime<Utc>>,
    /// Offsets (days) already asked or passed over.
    pub asked: Vec<i64>,
}

impl Fact {
    /// In flight: drawn and planted within [`PLANT_WINDOW_DAYS`], and not
    /// yet asked at the last offset. A draw never planted in time is
    /// abandoned: it no longer holds a slot (its pair stays retired).
    #[must_use]
    pub fn active(&self, offsets: &[i64], now: DateTime<Utc>) -> bool {
        if self.value.is_none() {
            return now - self.drawn_at < Duration::days(PLANT_WINDOW_DAYS);
        }
        let last = offsets.iter().max().copied().unwrap_or(0);
        !self.asked.contains(&last)
    }
}

/// Rebuild the facts from the verified rows of `facts.jsonl`.
#[must_use]
pub fn facts_from_rows(rows: &[Value], pairs: &LoadedPairs) -> Vec<Fact> {
    let mut facts: Vec<Fact> = Vec::new();
    for row in rows {
        let Some(id) = row["fact"].as_str() else {
            continue;
        };
        let at = row["at"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        match row["event"].as_str() {
            Some("draw") => {
                let (Some(drawn_at), Some((pair, planted))) = (
                    at,
                    row["assignment"]
                        .as_str()
                        .and_then(|a| pairs.resolve(id, a)),
                ) else {
                    tracing::warn!("facts.jsonl: draw {id} does not resolve with this fact key");
                    continue;
                };
                facts.push(Fact {
                    fact: id.to_string(),
                    pair,
                    planted,
                    drawn_at,
                    value: None,
                    planted_at: None,
                    asked: Vec::new(),
                });
            }
            Some("plant") => {
                if let Some(fact) = facts.iter_mut().find(|f| f.fact == id) {
                    fact.value = row["value"].as_str().map(str::to_string);
                    fact.planted_at = row["planted_at"]
                        .as_str()
                        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                        .map(|t| t.with_timezone(&Utc));
                }
            }
            Some("ask") | Some("missed") => {
                if let (Some(fact), Some(offset)) = (
                    facts.iter_mut().find(|f| f.fact == id),
                    row["offset_days"].as_i64(),
                ) {
                    fact.asked.push(offset);
                }
            }
            _ => {}
        }
    }
    facts
}

/// Why a draw or plant was refused.
#[derive(Debug, thiserror::Error)]
pub enum FactError {
    #[error("{0}")]
    Refused(String),
    #[error("record: {0}")]
    Record(#[from] std::io::Error),
}

/// The facts rows, refused when the chain is broken: rows lost to a break
/// could reuse a pair or plant an abandoned draw (fail closed).
fn intact_facts(record: &Record) -> Result<Vec<Value>, FactError> {
    let read = record.read(Stream::Facts)?;
    match read.broken_at {
        Some(line) => Err(FactError::Refused(format!(
            "record/probes/facts.jsonl is broken at line {line}: no draw or plant until it is \
             inspected"
        ))),
        None => Ok(read.rows),
    }
}

/// The record's run lock, so a draw or plant never interleaves with a run
/// reading and appending `facts.jsonl`.
fn run_lock(record: &Record) -> Result<super::record::RunLock, FactError> {
    record.try_lock()?.ok_or_else(|| {
        FactError::Refused("a probe run is in progress; try again once it finishes".into())
    })
}

/// What a draw asks the owner to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drawn {
    pub fact: String,
    pub ask_owner: String,
    /// A phrase the owner's message must contain, with the value.
    pub say: String,
    /// The twin, which the owner should not talk about until the last ask.
    pub avoid: String,
}

/// Draw a pair no fact has ever used, and which side the owner plants. The
/// randomness is the runtime's (a fresh UUID), and the draw is logged
/// before the owner does anything.
pub fn draw(record: &Record, pairs: &LoadedPairs, now: DateTime<Utc>) -> Result<Drawn, FactError> {
    let _lock = run_lock(record)?;
    let facts = facts_from_rows(&intact_facts(record)?, pairs);
    let offsets = &pairs.set.offsets_days;
    let active: Vec<&Fact> = facts.iter().filter(|f| f.active(offsets, now)).collect();
    if active.len() >= pairs.set.max_active {
        return Err(FactError::Refused(format!(
            "{} facts are already in flight (max {}); draw again after one finishes",
            active.len(),
            pairs.set.max_active
        )));
    }
    // A pair is used once, ever: after a fact on it, the twin has been
    // asked about and is no longer "never told".
    let free: Vec<&Pair> = pairs
        .set
        .pairs
        .iter()
        .filter(|p| !facts.iter().any(|f| f.pair == p.id))
        .collect();
    if free.is_empty() {
        return Err(FactError::Refused(format!(
            "all {} pairs of {} have been used; a new set version brings new ones",
            pairs.set.pairs.len(),
            pairs.label()
        )));
    }
    let random = uuid::Uuid::new_v4();
    let bytes = random.as_bytes();
    let pair = free[usize::from(bytes[0]) % free.len()];
    let planted = if bytes[1].is_multiple_of(2) {
        Which::A
    } else {
        Which::B
    };
    let (side, twin) = match planted {
        Which::A => (&pair.a, &pair.b),
        Which::B => (&pair.b, &pair.a),
    };
    let fact = format!("f{}", &random.simple().to_string()[..8]);
    record.append(
        Stream::Facts,
        json!({
            "v": RECORD_VERSION,
            "event": "draw",
            "fact": fact,
            "at": now.to_rfc3339(),
            "set": pairs.label(),
            "set_sha256": pairs.sha256,
            "assignment": pairs.assignment(&fact, &pair.id, planted),
        }),
    )?;
    Ok(Drawn {
        fact,
        ask_owner: side.ask_owner.clone(),
        say: side.keywords.first().cloned().unwrap_or_default(),
        avoid: twin.ask_owner.clone(),
    })
}

/// Where a mention was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub at: DateTime<Utc>,
    pub source: String,
}

/// The drawn fact `fact_id`, if it can still be planted: drawn, not yet
/// planted, and inside its plant window.
fn plantable<'f>(
    facts: &'f [Fact],
    fact_id: &str,
    offsets_days: &[i64],
    now: DateTime<Utc>,
) -> Result<&'f Fact, FactError> {
    let fact = facts
        .iter()
        .find(|f| f.fact == fact_id)
        .ok_or_else(|| FactError::Refused(format!("no drawn fact '{fact_id}'")))?;
    if fact.value.is_some() {
        return Err(FactError::Refused(format!(
            "'{fact_id}' is already planted"
        )));
    }
    if !fact.active(offsets_days, now) {
        return Err(FactError::Refused(format!(
            "'{fact_id}' was drawn more than {PLANT_WINDOW_DAYS} days ago and is abandoned; \
             draw a new fact"
        )));
    }
    Ok(fact)
}

/// Why `value` cannot be planted for `fact`: no owner message since the
/// draw gives it with one of the side's phrases.
fn no_mention(fact: &Fact, side: &Side, value: &str) -> FactError {
    FactError::Refused(format!(
        "no owner message since the draw ({}) gives '{value}' together with {}: mention it \
         to the pulse in conversation, then plant it before that conversation is archived",
        fact.drawn_at.format("%Y-%m-%d %H:%M UTC"),
        side.keywords
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(" or ")
    ))
}

/// Record the planted value, once an owner message after the draw is found
/// to contain it. Reports whether the twin was talked about too.
pub fn plant(
    record: &Record,
    live_root: &Path,
    pairs: &LoadedPairs,
    fact_id: &str,
    value: &str,
    now: DateTime<Utc>,
) -> Result<(Mention, bool), FactError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(FactError::Refused("the value is empty".into()));
    }
    let _lock = run_lock(record)?;
    let facts = facts_from_rows(&intact_facts(record)?, pairs);
    let fact = plantable(&facts, fact_id, &pairs.set.offsets_days, now)?;
    let pair = pairs
        .pair(&fact.pair)
        .ok_or_else(|| FactError::Refused(format!("pair '{}' is not in this set", fact.pair)))?;
    let side = planted_side(pair, fact.planted);
    let mention = find_plant(live_root, fact.drawn_at, side, value)
        .ok_or_else(|| no_mention(fact, side, value))?;
    let twin_mentioned = twin_was_mentioned(live_root, twin_of(pair, fact.planted));
    record.append(
        Stream::Facts,
        json!({
            "v": RECORD_VERSION,
            "event": "plant",
            "fact": fact_id,
            "at": now.to_rfc3339(),
            "value": value,
            "planted_at": mention.at.to_rfc3339(),
            "source": mention.source,
            "twin_mentioned": twin_mentioned,
        }),
    )?;
    Ok((mention, twin_mentioned))
}

fn twin_of(pair: &Pair, planted: Which) -> &Side {
    match planted {
        Which::A => &pair.b,
        Which::B => &pair.a,
    }
}

fn planted_side(pair: &Pair, planted: Which) -> &Side {
    match planted {
        Which::A => &pair.a,
        Which::B => &pair.b,
    }
}

/// Has the owner ever named the twin's slot? Any owner message, in the WAL
/// or in any archive, before the draw or after it, that names one of the
/// twin's keyword phrases: a pulse told its owner's favourite lighthouse a year
/// ago was never "never told" (audit R2-9). A false void costs a sample, a
/// missed mention would corrupt the baseline.
#[must_use]
pub fn twin_was_mentioned(live_root: &Path, twin: &Side) -> bool {
    owner_messages_ever(live_root)
        .iter()
        .any(|m| names_side(&m.text, twin))
}

/// Every owner message the runtime can see: the WAL and every archive.
fn owner_messages_ever(live_root: &Path) -> Vec<owner::OwnerMessage> {
    owner::wal_messages(live_root)
        .into_iter()
        .chain(owner::archived_messages(
            live_root,
            DateTime::<Utc>::MIN_UTC,
        ))
        .collect()
}

/// Does `text` name one of `side`'s keyword phrases?
fn names_side(text: &str, side: &Side) -> bool {
    side.keywords.iter().any(|k| contains_phrase(text, k))
}

/// The first owner message after `since` that gives `value` for `side`: the
/// value and one of the side's keyword phrases in the same message, in the
/// owner session's WAL (whose entries carry their own time; an archived
/// message has none, so it cannot time a plant).
#[must_use]
pub fn find_plant(
    live_root: &Path,
    since: DateTime<Utc>,
    side: &Side,
    value: &str,
) -> Option<Mention> {
    owner::wal_messages(live_root)
        .into_iter()
        .find(|m| m.at >= since && contains_phrase(&m.text, value) && names_side(&m.text, side))
        .map(|m| Mention {
            at: m.at,
            source: m.source,
        })
}

/// Does `text` contain `phrase` as whole words (a trailing `s` allowed)?
fn contains_phrase(text: &str, phrase: &str) -> bool {
    let words = |t: &str| -> Vec<String> {
        t.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let text = words(text);
    let phrase = words(phrase);
    !phrase.is_empty()
        && text.windows(phrase.len()).any(|window| {
            window
                .iter()
                .zip(&phrase)
                .enumerate()
                .all(|(i, (w, p))| w == p || (i == phrase.len() - 1 && *w == format!("{p}s")))
        })
}

/// One side to ask at one offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub fact: String,
    pub pair: String,
    pub offset_days: i64,
    pub side: SideRole,
    pub question: String,
    /// The planted value (planted side only).
    pub value: Option<String>,
    /// Twin side: the owner talked about it after the draw (set by
    /// [`mark_contaminated`], after the run's gate and lock).
    pub contaminated: bool,
    /// When the fact was drawn.
    pub drawn_at: DateTime<Utc>,
    /// When the owner's mention planted it.
    pub planted_at: DateTime<Utc>,
    /// The side's keyword phrases (a tool call naming one looked it up).
    pub keywords: Vec<String>,
    /// Earlier offsets that came due and were passed over.
    pub missed: Vec<i64>,
}

/// Which side an ask is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SideRole {
    /// The planted side, asked of the pulse.
    Planted,
    /// The never-planted side, asked of the pulse.
    Twin,
    /// The planted side asked of the fresh arm (same CLI and model, no
    /// memory, no scaffold) as the pulse is asked it, UNKNOWN allowed: how
    /// often a model with no memory volunteers the value.
    Chance,
    /// The planted side asked of the fresh arm with no UNKNOWN to fall back
    /// on ([`GUESS_INSTRUCTIONS`]): its unhedged hits are the chance rate.
    Guess,
}

impl SideRole {
    /// The side's label on rows (`planted`, `twin`, `chance`, `guess`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planted => "planted",
            Self::Twin => "twin",
            Self::Chance => "chance",
            Self::Guess => "guess",
        }
    }

    /// Does this ask carry the planted value?
    #[must_use]
    pub fn knows_value(self) -> bool {
        matches!(self, Self::Planted | Self::Chance | Self::Guess)
    }
}

/// Asks per due fact: planted, twin, volunteered, guess.
pub const ASKS_PER_FACT: usize = 4;

/// The asks due at `now`: for each planted fact, the latest offset that has
/// come due and not been asked, as [`ASKS_PER_FACT`] asks in this order: the
/// planted side and the twin, of the pulse, then the planted side of the
/// fresh arm, UNKNOWN allowed (volunteered) and as a forced guess (the
/// chance rate for "remembered"). Earlier unasked offsets are
/// reported as missed (asked late would not be that offset).
#[must_use]
pub fn due(pairs: &LoadedPairs, facts: &[Fact], now: DateTime<Utc>) -> Vec<Ask> {
    let mut asks = Vec::new();
    for fact in facts {
        let (Some(value), Some(planted_at), Some(pair)) =
            (&fact.value, fact.planted_at, pairs.pair(&fact.pair))
        else {
            continue;
        };
        let came_due: Vec<i64> = pairs
            .set
            .offsets_days
            .iter()
            .copied()
            .filter(|d| !fact.asked.contains(d) && planted_at + Duration::days(*d) <= now)
            .collect();
        let Some(&offset) = came_due.iter().max() else {
            continue;
        };
        let missed: Vec<i64> = came_due.iter().copied().filter(|d| *d < offset).collect();
        let twin = twin_of(pair, fact.planted);
        let planted = planted_side(pair, fact.planted);
        for (role, side) in [
            (SideRole::Planted, planted),
            (SideRole::Twin, twin),
            (SideRole::Chance, planted),
            (SideRole::Guess, planted),
        ] {
            let question = if role == SideRole::Guess {
                LoadedPairs::guess_prompt(side)
            } else {
                pairs.prompt(side)
            };
            asks.push(Ask {
                fact: fact.fact.clone(),
                pair: fact.pair.clone(),
                offset_days: offset,
                side: role,
                question,
                value: role.knows_value().then(|| value.clone()),
                contaminated: false,
                drawn_at: fact.drawn_at,
                planted_at,
                keywords: side.keywords.clone(),
                missed: missed.clone(),
            });
        }
    }
    asks
}

/// Mark every twin ask whose keyword phrases any owner message ever used.
/// It reads the owner's messages (WAL and archives), so the runner calls it
/// only once the run has passed its gate and holds its lock.
pub fn mark_contaminated(asks: &mut [Ask], live_root: &Path) {
    if !asks.iter().any(|a| a.side == SideRole::Twin) {
        return;
    }
    let messages = owner_messages_ever(live_root);
    for ask in asks.iter_mut().filter(|a| a.side == SideRole::Twin) {
        ask.contaminated = messages
            .iter()
            .any(|m| ask.keywords.iter().any(|k| contains_phrase(&m.text, k)));
    }
}

/// Score one answer by its kind ([`answer_kind`]), the same check on every
/// side: a bare abstention offers nothing; a hedge and a value both offer
/// one. Planted side (and its chance ask): remembered (correct) only when
/// the answer is not an abstention and holds the value (as words); a bare
/// abstention is unknown; anything else is wrong. Twin side: a bare
/// abstention is correct; anything else offers a value, and is made up
/// (wrong).
#[must_use]
pub fn score_answer(ask: &Ask, answer: &str) -> Outcome {
    let answer = answer.trim();
    if answer.is_empty() {
        return Outcome::Unanswered;
    }
    let kind = answer_kind(ask, answer);
    match (ask.side, &ask.value) {
        (SideRole::Twin, _) if kind == AnswerKind::Abstained => Outcome::Correct,
        (SideRole::Twin, _) => Outcome::Wrong,
        (_, Some(_)) if kind == AnswerKind::Abstained => Outcome::Unknown,
        (_, Some(value)) if contains_phrase(answer, value) => Outcome::Correct,
        (_, Some(_)) => Outcome::Wrong,
        (_, None) => Outcome::NoTruth,
    }
}

/// The kind of `answer` to `ask`, read against the question it was asked
/// (on every item row as `answer_kind`, and what [`score_answer`] scores).
#[must_use]
pub fn answer_kind(ask: &Ask, answer: &str) -> AnswerKind {
    score::answer_kind(answer, &ask.question)
}

/// Where an answer came from (the P1 caveat on C5): `lookup` when a tool
/// call's result carries the planted value or a call's input names the
/// side's slot, `context` when the turn made no such call, `unobserved`
/// when the CLI does not show its tool calls.
#[must_use]
pub fn answered_from(ask: &Ask, calls: &[ToolCall], observes_tools: bool) -> &'static str {
    if !observes_tools {
        return "unobserved";
    }
    let carries_value = |call: &ToolCall| {
        ask.value
            .as_deref()
            .zip(call.result.as_deref())
            .is_some_and(|(value, result)| contains_phrase(result, value))
    };
    let names_slot = |call: &ToolCall| {
        let input = call.input.to_string();
        ask.keywords.iter().any(|k| contains_phrase(&input, k))
    };
    if calls.iter().any(|c| carries_value(c) || names_slot(c)) {
        "lookup"
    } else {
        "context"
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A draw row as `draw` writes it, for the fixture pairs and key.
    pub(crate) fn draw_row(fact: &str, pair: &str, planted: Which, at: DateTime<Utc>) -> Value {
        let pairs = LoadedPairs::fixture();
        json!({
            "event": "draw",
            "fact": fact,
            "at": at.to_rfc3339(),
            "set": pairs.label(),
            "assignment": pairs.assignment(fact, pair, planted),
        })
    }

    fn pulse_with_owner_says(said: &[(&str, DateTime<Utc>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = dir.path().join("sessions/wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let lines: Vec<String> = said
            .iter()
            .map(|(text, ts)| owner::tests::wal_line(text, *ts, "tui", "user"))
            .collect();
        std::fs::write(wal_dir.join("owner.jsonl"), lines.join("\n") + "\n").unwrap();
        dir
    }

    fn record() -> (tempfile::TempDir, Record) {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        (dir, record)
    }

    fn t(day: u32, hour: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0).unwrap()
    }

    /// R3-C: the set's definition covers the fresh arm's line its chance ask
    /// is put with, and (R3-A) the forced guess's instructions; its file
    /// digest is the file alone.
    #[test]
    fn the_paired_definition_covers_the_fresh_line() {
        let pairs = LoadedPairs::fixture();
        let file = &super::super::sealed::tests::fixture_sets();
        let text = file.text("paired-facts.toml").unwrap();
        assert_eq!(pairs.file_sha256, hex(&Sha256::digest(text.as_bytes())));
        let definition = format!(
            "{}\nfresh {}\nguess {}",
            pairs.file_sha256,
            super::super::sets::fresh_sha256(),
            guess_sha256()
        );
        assert_eq!(pairs.sha256, hex(&Sha256::digest(definition.as_bytes())));
    }

    /// R3-A: the forced guess's instructions are pre-registered word for
    /// word on the methods page, and offer no UNKNOWN.
    #[test]
    fn the_forced_guess_is_pre_registered_and_offers_no_unknown() {
        let page = include_str!("../../../docs/methods/paired-facts.md");
        assert!(page.contains(GUESS_INSTRUCTIONS), "{GUESS_INSTRUCTIONS}");
        assert!(GUESS_INSTRUCTIONS.starts_with("Guess: "));
        assert!(GUESS_INSTRUCTIONS.contains("Do not reply UNKNOWN"));
        assert_eq!(
            guess_sha256(),
            hex(&Sha256::digest(GUESS_INSTRUCTIONS.as_bytes()))
        );
    }

    #[test]
    fn the_fixture_pairs_parse() {
        let pairs = LoadedPairs::fixture();
        assert_eq!(pairs.label(), "paired-facts/v1");
        assert_eq!(pairs.set.offsets_days, [1, 7, 30, 90]);
        assert!(pairs.set.pairs.len() >= 4);
        assert!(pairs.prompt(&pairs.set.pairs[0].a).contains("UNKNOWN"));
    }

    #[test]
    fn a_draw_is_logged_before_the_owner_acts_and_capped() {
        let (_dir, record) = record();
        let pairs = LoadedPairs::fixture();
        let drawn = draw(&record, &pairs, t(1, 9)).unwrap();
        let rows = record.read(Stream::Facts).unwrap().rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["event"], "draw");
        assert_eq!(rows[0]["fact"], drawn.fact);
        assert!(rows[0].get("value").is_none());
        // R2-11: the row says neither the pair nor the side, nor any slot;
        // the runner recovers both from the commitment with the key.
        let row = rows[0].to_string();
        for leak in ["\"pair\"", "\"planted\"", "slot", "keepsakes", "fixture-"] {
            assert!(!row.contains(leak), "{leak} in {row}");
        }
        assert_eq!(rows[0]["assignment"].as_str().unwrap().len(), 64);
        let facts = facts_from_rows(&rows, &pairs);
        assert_eq!(facts.len(), 1);
        let other_key = LoadedPairs {
            fact_key: "another-key-of-at-least-thirty-two-chars".into(),
            ..pairs.clone()
        };
        assert!(
            facts_from_rows(&rows, &other_key).is_empty(),
            "without the key the row says nothing"
        );
        draw(&record, &pairs, t(1, 9)).unwrap();
        draw(&record, &pairs, t(1, 9)).unwrap();
        let facts = facts_from_rows(&record.read(Stream::Facts).unwrap().rows, &pairs);
        let mut pair_ids: Vec<&str> = facts.iter().map(|f| f.pair.as_str()).collect();
        pair_ids.sort_unstable();
        pair_ids.dedup();
        assert_eq!(
            pair_ids.len(),
            3,
            "a pair is never drawn twice while in flight"
        );
        assert!(matches!(
            draw(&record, &pairs, t(1, 9)),
            Err(FactError::Refused(_))
        ));
    }

    /// Audit R2-8: an abandoned draw cannot be planted, and a broken facts
    /// chain stops draws and plants.
    #[test]
    fn an_abandoned_draw_cannot_be_planted_and_a_broken_chain_stops_all() {
        let (dir, record) = record();
        let pairs = LoadedPairs::fixture();
        let drawn = draw(&record, &pairs, t(1, 9)).unwrap();
        let live = tempfile::tempdir().unwrap();
        let late = t(1, 9) + Duration::days(PLANT_WINDOW_DAYS + 1);
        assert!(matches!(
            plant(&record, live.path(), &pairs, &drawn.fact, "x", late),
            Err(FactError::Refused(r)) if r.contains("abandoned")
        ));
        let path = dir.path().join("record/probes/facts.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen("\"draw\"", "\"drew\"", 1)).unwrap();
        assert!(matches!(
            draw(&record, &pairs, t(2, 9)),
            Err(FactError::Refused(r)) if r.contains("broken")
        ));
    }

    /// Audit #22: a draw never planted stops holding a slot after the
    /// plant window (and its pair stays retired).
    #[test]
    fn an_abandoned_draw_frees_its_slot() {
        let (_dir, record) = record();
        let pairs = LoadedPairs::fixture();
        for _ in 0..pairs.set.max_active {
            draw(&record, &pairs, t(1, 9)).unwrap();
        }
        assert!(draw(&record, &pairs, t(2, 9)).is_err(), "three in flight");
        let later = t(1, 9) + Duration::days(PLANT_WINDOW_DAYS);
        draw(&record, &pairs, later).unwrap();
        let facts = facts_from_rows(&record.read(Stream::Facts).unwrap().rows, &pairs);
        let mut used: Vec<&str> = facts.iter().map(|f| f.pair.as_str()).collect();
        used.sort_unstable();
        used.dedup();
        assert_eq!(used.len(), facts.len(), "no pair used twice");
    }

    #[test]
    fn the_long_offsets_are_exploratory_only() {
        let pairs = LoadedPairs::fixture();
        let confirmatory: Vec<i64> = pairs
            .set
            .offsets_days
            .iter()
            .copied()
            .filter(|d| !is_exploratory(*d))
            .collect();
        assert_eq!(confirmatory, [1, 7]);
    }

    /// Audit #5: a pair is retired after one use, finished or not.
    #[test]
    fn a_pair_is_never_drawn_again_once_used() {
        let (_dir, record) = record();
        let pairs = LoadedPairs::fixture();
        let mut seen = std::collections::BTreeSet::new();
        // Each draw is abandoned before the next (never planted).
        let at = |n: usize| t(1, 9) + Duration::days((PLANT_WINDOW_DAYS + 1) * n as i64);
        for n in 0..pairs.set.pairs.len() {
            let drawn = draw(&record, &pairs, at(n)).unwrap();
            let fact = facts_from_rows(&record.read(Stream::Facts).unwrap().rows, &pairs)
                .into_iter()
                .find(|f| f.fact == drawn.fact)
                .unwrap();
            assert!(seen.insert(fact.pair.clone()), "{} drawn twice", fact.pair);
        }
        let exhausted = draw(&record, &pairs, at(pairs.set.pairs.len()));
        assert!(
            matches!(&exhausted, Err(FactError::Refused(r)) if r.contains("have been used")),
            "{exhausted:?}"
        );
    }

    /// Audit #2: a draw or plant never runs while a probe run holds the lock.
    #[test]
    fn draw_and_plant_wait_for_a_running_probe() {
        let (_dir, record) = record();
        let pairs = LoadedPairs::fixture();
        let drawn = draw(&record, &pairs, t(1, 9)).unwrap();
        let held = record.try_lock().unwrap().unwrap();
        assert!(matches!(
            draw(&record, &pairs, t(1, 9)),
            Err(FactError::Refused(r)) if r.contains("in progress")
        ));
        let live = tempfile::tempdir().unwrap();
        assert!(matches!(
            plant(&record, live.path(), &pairs, &drawn.fact, "x", t(1, 10)),
            Err(FactError::Refused(r)) if r.contains("in progress")
        ));
        drop(held);
        assert_eq!(
            facts_from_rows(&record.read(Stream::Facts).unwrap().rows, &pairs).len(),
            1,
            "nothing was written while the run held the lock"
        );
    }

    #[test]
    fn a_plant_is_refused_with_its_reason() {
        let (_dir, record) = record();
        let pairs = LoadedPairs::fixture();
        let drawn = draw(&record, &pairs, t(2, 9)).unwrap();
        let live = pulse_with_owner_says(&[]);
        let refusal = |fact: &str, value: &str| match plant(
            &record,
            live.path(),
            &pairs,
            fact,
            value,
            t(2, 11),
        ) {
            Err(FactError::Refused(why)) => why,
            other => panic!("{other:?}"),
        };
        assert_eq!(refusal(&drawn.fact, "  "), "the value is empty");
        assert_eq!(refusal("f-unknown", "Lychee"), "no drawn fact 'f-unknown'");
        let no_mention = refusal(&drawn.fact, "Lychee");
        assert!(
            no_mention.starts_with(
                "no owner message since the draw (2026-10-02 09:00 UTC) gives 'Lychee' together with \""
            ),
            "{no_mention}"
        );
        assert!(no_mention.ends_with(
            ": mention it to the pulse in conversation, then plant it before that conversation \
             is archived"
        ));

        let fact = &facts_from_rows(&record.read(Stream::Facts).unwrap().rows, &pairs)[0];
        let mut planted = fact.clone();
        planted.value = Some("Lychee".into());
        let facts = [planted];
        match plantable(&facts, &fact.fact, &pairs.set.offsets_days, t(2, 11)) {
            Err(FactError::Refused(why)) => {
                assert_eq!(why, format!("'{}' is already planted", fact.fact));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_plant_is_confirmed_only_by_an_owner_message_after_the_draw() {
        let (_dir, record) = record();
        let pairs = LoadedPairs::fixture();
        let drawn = draw(&record, &pairs, t(2, 9)).unwrap();
        // The draw picks the side; the owner names its slot and a value.
        let fact = &facts_from_rows(&record.read(Stream::Facts).unwrap().rows, &pairs)[0];
        let pair = pairs.pair(&fact.pair).unwrap();
        let slot = &planted_side(pair, fact.planted).keywords[0];
        let before = format!("my {slot} is Lychee");
        let after = format!("by the way, my {slot}? lychee");
        let live = pulse_with_owner_says(&[
            (before.as_str(), t(1, 9)),
            ("lychee smoothies are the best", t(2, 9)),
            (after.as_str(), t(2, 10)),
        ]);
        assert!(matches!(
            plant(
                &record,
                live.path(),
                &pairs,
                &drawn.fact,
                "durian",
                t(2, 11)
            ),
            Err(FactError::Refused(_))
        ));
        let (mention, _) = plant(
            &record,
            live.path(),
            &pairs,
            &drawn.fact,
            "Lychee",
            t(2, 11),
        )
        .unwrap();
        assert_eq!(
            mention.at,
            t(2, 10),
            "the mention before the draw, and the value without the slot, do not count"
        );
        assert_eq!(mention.source, "sessions/wal/owner.jsonl");
        assert!(matches!(
            plant(
                &record,
                live.path(),
                &pairs,
                &drawn.fact,
                "Lychee",
                t(2, 12)
            ),
            Err(FactError::Refused(_))
        ));
        let facts = facts_from_rows(&record.read(Stream::Facts).unwrap().rows, &pairs);
        assert_eq!(facts[0].value.as_deref(), Some("Lychee"));
        assert_eq!(facts[0].planted_at, Some(t(2, 10)));
    }

    #[test]
    fn the_runtime_context_in_a_user_turn_is_not_the_owner_speaking() {
        let pairs = LoadedPairs::fixture();
        let sundial = &pairs.set.pairs[0].a;
        // The channel context line ("Pulse: what is your favourite sundial?")
        // is the runtime's; the owner only said hello.
        let live = pulse_with_owner_says(&[("hello, lychee", t(2, 10))]);
        assert!(!twin_was_mentioned(live.path(), sundial));
        assert!(find_plant(live.path(), t(1, 0), sundial, "lychee").is_none());
    }

    /// Audit #3: a single common word does not void a twin; its phrase does,
    /// in an owner message only, and pulse replies and the index never.
    #[test]
    fn a_twin_is_contaminated_by_its_phrase_in_an_owner_message() {
        let pairs = LoadedPairs::fixture();
        let umbrella = &pairs.set.pairs[0].b;
        let live = pulse_with_owner_says(&[("I never carry umbrellas, honestly", t(3, 9))]);
        assert!(!twin_was_mentioned(live.path(), umbrella));
        let dir = live.path().join("archives/conversations");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("conversation-001.md"),
            "---\nlog: 1\ndate: \"2026-10-03T09:00:00Z\"\n---\n\n### User\n\n\nUser message: hi\n\n\
             ---\n\n### Assistant\n\nWhat is your favourite umbrella?\n",
        )
        .unwrap();
        std::fs::write(dir.join("INDEX.md"), "favourite umbrella").unwrap();
        assert!(!twin_was_mentioned(live.path(), umbrella));
        std::fs::write(
            dir.join("conversation-002.md"),
            "---\nlog: 2\ndate: \"2026-10-04T09:00:00Z\"\n---\n\n### User\n\n\
             \nUser message: my favourite umbrella is a secret\n",
        )
        .unwrap();
        assert!(twin_was_mentioned(live.path(), umbrella));
    }

    /// Audit R2-9: a slot the owner named before the draw voids the twin
    /// too (the pulse was told, whenever it was).
    #[test]
    fn a_twin_named_before_the_draw_is_contaminated() {
        let pairs = LoadedPairs::fixture();
        let planted = t(5, 12);
        let live = pulse_with_owner_says(&[("my best umbrella is a secret", t(1, 9))]);
        let mut asks = due(&pairs, &[fact(planted, vec![])], t(6, 12));
        mark_contaminated(&mut asks, live.path());
        assert!(asks
            .iter()
            .any(|a| a.side == SideRole::Twin && a.contaminated));
    }

    fn fact(planted_at: DateTime<Utc>, asked: Vec<i64>) -> Fact {
        Fact {
            fact: "f1".into(),
            pair: "keepsakes".into(),
            planted: Which::A,
            drawn_at: planted_at - Duration::hours(1),
            value: Some("lychee".into()),
            planted_at: Some(planted_at),
            asked,
        }
    }

    #[test]
    fn asks_come_due_at_the_offsets_and_late_ones_are_missed() {
        let pairs = LoadedPairs::fixture();
        let planted = t(1, 12);
        assert!(due(&pairs, &[fact(planted, vec![])], t(2, 11)).is_empty());
        let asks = due(&pairs, &[fact(planted, vec![])], t(2, 12));
        assert_eq!(asks.len(), ASKS_PER_FACT);
        assert_eq!(asks[0].side, SideRole::Planted);
        assert_eq!(asks[1].side, SideRole::Twin);
        assert_eq!(asks[2].side, SideRole::Chance);
        assert_eq!(asks[3].side, SideRole::Guess);
        assert_eq!(
            asks[2].question, asks[0].question,
            "the volunteered ask is the planted question as the pulse got it"
        );
        assert_eq!(asks[2].value, asks[0].value);
        // R3-A: the forced guess asks the planted question with no UNKNOWN
        // to fall back on.
        assert!(
            asks[3].question.starts_with("Guess: "),
            "{}",
            asks[3].question
        );
        assert!(asks[3]
            .question
            .ends_with("Fixture: the owner's favourite sundial?"));
        assert!(!asks[3].question.contains("the answer, or UNKNOWN"));
        assert_eq!(asks[3].value, asks[0].value);
        assert_eq!(asks[0].offset_days, 1);
        assert!(asks[1].question.contains("umbrella"));
        assert_eq!(asks[1].value, None, "the twin has no value");
        // Nine days on, unasked: the 7-day ask is due and the 1-day one missed.
        let late = due(&pairs, &[fact(planted, vec![])], t(10, 12));
        assert_eq!(late[0].offset_days, 7);
        assert_eq!(late[0].missed, vec![1]);
        assert!(due(&pairs, &[fact(planted, vec![1, 7])], t(10, 12)).is_empty());
        assert!(fact(planted, vec![1, 7, 30]).active(&pairs.set.offsets_days, t(10, 12)));
        assert!(!fact(planted, vec![1, 7, 30, 90]).active(&pairs.set.offsets_days, t(10, 12)));
    }

    #[test]
    fn a_twin_mentioned_in_conversation_is_flagged() {
        let pairs = LoadedPairs::fixture();
        let planted = t(1, 12);
        let live = pulse_with_owner_says(&[("my best umbrella is a secret", t(3, 9))]);
        let mut asks = due(&pairs, &[fact(planted, vec![])], t(4, 12));
        assert!(
            asks.iter().all(|a| !a.contaminated),
            "due itself reads nothing"
        );
        mark_contaminated(&mut asks, live.path());
        let twin = asks.iter().find(|a| a.side == SideRole::Twin).unwrap();
        assert!(twin.contaminated, "the owner talked about the twin");
        let planted_ask = asks.iter().find(|a| a.side == SideRole::Planted).unwrap();
        assert!(!planted_ask.contaminated);
    }

    #[test]
    fn answers_are_scored_by_side() {
        let pairs = LoadedPairs::fixture();
        let asks = due(&pairs, &[fact(t(1, 12), vec![])], t(2, 12));
        let (planted, twin) = (&asks[0], &asks[1]);
        assert_eq!(score_answer(planted, "Lychee."), Outcome::Correct);
        assert_eq!(score_answer(planted, "Mango"), Outcome::Wrong);
        assert_eq!(score_answer(planted, "UNKNOWN"), Outcome::Unknown);
        assert_eq!(score_answer(twin, "UNKNOWN"), Outcome::Correct);
        assert_eq!(
            score_answer(twin, "I don't know — you never told me."),
            Outcome::Correct
        );
        assert_eq!(
            score_answer(twin, "Probably carrots."),
            Outcome::Wrong,
            "made up"
        );
        assert_eq!(score_answer(twin, ""), Outcome::Unanswered);
    }

    /// Audit #11: hedging cuts both ways and is scored the same way on both
    /// sides; the value counts wherever it is.
    #[test]
    fn hedged_answers_are_values_on_both_sides() {
        let pairs = LoadedPairs::fixture();
        let asks = due(&pairs, &[fact(t(1, 12), vec![])], t(2, 12));
        let (planted, twin) = (&asks[0], &asks[1]);
        assert_eq!(
            score_answer(planted, "I'm not sure — maybe lychee?"),
            Outcome::Correct,
            "the right value, hedged, was remembered"
        );
        assert_eq!(
            score_answer(planted, "Not sure, maybe mango"),
            Outcome::Wrong
        );
        assert_eq!(score_answer(planted, "**UNKNOWN**"), Outcome::Unknown);
        assert_eq!(
            score_answer(twin, "I'm not sure, but probably carrots."),
            Outcome::Wrong,
            "a hedged guess is still made up"
        );
        assert_eq!(score_answer(twin, "**UNKNOWN**"), Outcome::Correct);
    }

    /// R3-B: one answer-kind check on both sides. A hedge offers a value
    /// whether or not a listed guess phrase appears, so a hedged twin is made
    /// up and a hedged planted value is remembered; only a bare abstention
    /// offers nothing.
    #[test]
    fn a_hedge_offers_a_value_on_both_sides_and_only_a_bare_abstention_offers_none() {
        let pairs = LoadedPairs::fixture();
        let asks = due(&pairs, &[fact(t(1, 12), vec![])], t(2, 12));
        let (planted, twin) = (&asks[0], &asks[1]);
        let cases = [
            // (answer, planted outcome, twin outcome, kind)
            (
                "Not sure, but lychee",
                Outcome::Correct,
                Outcome::Wrong,
                AnswerKind::HedgedWithValue,
            ),
            (
                "No idea, maybe lychee",
                Outcome::Correct,
                Outcome::Wrong,
                AnswerKind::HedgedWithValue,
            ),
            (
                "Not sure, but carrots",
                Outcome::Wrong,
                Outcome::Wrong,
                AnswerKind::HedgedWithValue,
            ),
            (
                "No idea, maybe carrots",
                Outcome::Wrong,
                Outcome::Wrong,
                AnswerKind::HedgedWithValue,
            ),
            (
                "I don't know.",
                Outcome::Unknown,
                Outcome::Correct,
                AnswerKind::Abstained,
            ),
            (
                "UNKNOWN",
                Outcome::Unknown,
                Outcome::Correct,
                AnswerKind::Abstained,
            ),
            (
                "Lychee",
                Outcome::Correct,
                Outcome::Wrong,
                AnswerKind::Value,
            ),
            (
                "maybe lychee",
                Outcome::Correct,
                Outcome::Wrong,
                AnswerKind::HedgedWithValue,
            ),
            ("Carrots", Outcome::Wrong, Outcome::Wrong, AnswerKind::Value),
        ];
        for (answer, planted_outcome, twin_outcome, kind) in cases {
            assert_eq!(
                score_answer(planted, answer),
                planted_outcome,
                "planted: {answer}"
            );
            assert_eq!(score_answer(twin, answer), twin_outcome, "twin: {answer}");
            assert_eq!(answer_kind(planted, answer), kind, "{answer}");
            assert_eq!(answer_kind(twin, answer), kind, "{answer}");
        }
        // An abstention that restates the question it was asked offers
        // nothing.
        let restated = "I don't know the owner's favourite umbrella.";
        assert_eq!(answer_kind(twin, restated), AnswerKind::Abstained);
        assert_eq!(score_answer(twin, restated), Outcome::Correct);
    }

    #[test]
    fn an_answer_is_a_lookup_only_when_a_tool_found_or_sought_it() {
        let pairs = LoadedPairs::fixture();
        let asks = due(&pairs, &[fact(t(1, 12), vec![])], t(2, 12));
        let planted = &asks[0];
        let listing = ToolCall {
            result: Some("SELF.md notes".into()),
            ..ToolCall::new("Bash", serde_json::json!({"command": "ls"}))
        };
        assert_eq!(
            answered_from(planted, std::slice::from_ref(&listing), true),
            "context"
        );
        let search = ToolCall::new(
            "recall_search",
            serde_json::json!({"query": "owner favourite sundial"}),
        );
        assert_eq!(answered_from(planted, &[search], true), "lookup");
        let found = ToolCall {
            result: Some("…said lychee yesterday…".into()),
            ..ToolCall::new("Grep", serde_json::json!({"pattern": "owner"}))
        };
        assert_eq!(answered_from(planted, &[found], true), "lookup");
        assert_eq!(answered_from(planted, &[listing], false), "unobserved");
    }
}
