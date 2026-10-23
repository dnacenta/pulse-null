//! Pre-registered probe item sets (G7), sealed.
//!
//! Each set is a TOML file whose text is not in the repo: the binary pins
//! its SHA-256 (`assets/probes/v1/sealed.toml`) and reads it from the
//! pulse's held-out record ([`super::sealed`]). The schema, truths and rules
//! are here and on the methods pages; the item wording is not. The file's
//! digest is recorded on every row, so an edit shows up even if `version`
//! was not bumped.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::scratch::hex;
use super::sealed::InstalledSets;

/// The sealed files of the item sets, in run order.
pub const SET_FILES: [&str; 2] = ["known-answer.toml", "situation-use.toml"];

/// The v1 input-only facts template.
pub const FACTS_V1: &str = include_str!("../../../assets/probes/v1/facts.md");

/// The fresh arm's system prompt: one neutral line, passed the same way the
/// pulse's own is, so the CLI's default prompt (which may carry the date or
/// the model) never stands in for "no scaffold" (audit #10).
pub const FRESH_SYSTEM_V1: &str = include_str!("../../../assets/probes/v1/fresh-system.md");

/// Which measure a set feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Measure {
    /// Known-answer self-questions: a regression check, not evidence.
    KnownAnswer,
    /// Tasks that need a situation fact to be done right. v1 is prompted
    /// retrieval (each request names the fact it needs): a check that the
    /// pulse gives the fact when asked, not evidence that it acts on its
    /// situation, and not a test of Concept v2 Phase 1's exit.
    SituationUse,
}

/// How a set's items reach the pulse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Delivery {
    /// Every item in one numbered turn per arm.
    #[default]
    Batched,
    /// One item per family, drawn per run, each its own turn per arm.
    PerItem,
}

/// Who answers an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Arm {
    /// The pulse: its full scaffold in a scratch copy of its home.
    Pulse,
    /// The same CLI and model in an empty directory, given only the runtime
    /// facts in a fixed template. In v1 the facts state every answer, so
    /// this arm sits at the ceiling: a check of the scorer and the facts,
    /// not a baseline that can lose.
    InputOnly,
    /// The same CLI and model in an empty directory with nothing from the
    /// runtime but a neutral one-line system prompt: a labelled reference,
    /// never the headline comparison.
    Fresh,
}

impl Arm {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pulse => "pulse",
            Self::InputOnly => "input-only",
            Self::Fresh => "fresh",
        }
    }
}

/// Where an item's truth comes from. Every source is the runtime's; the
/// methods page names the file or call behind each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TruthKey {
    PulseName,
    OwnerName,
    Adapter,
    ConfiguredModel,
    ServedModel,
    Today,
    Weekday,
    OwnerLastActive,
    RuntimeStarted,
    LastTaskName,
    LastTaskDate,
    /// Today + 1 in the owner's timezone.
    Tomorrow,
    /// The first Friday after today.
    ComingFriday,
    /// Whole days since the owner last wrote (only when at least one).
    DaysAway,
    /// `RESTARTED` when the daemon started after the owner last wrote,
    /// else `SAME`.
    RestartedSinceContact,
}

/// How an answer is compared with its truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    /// The truth's words appear, in order, among the answer's words.
    Contains,
    /// The answer names the truth's model, its name words and version
    /// parts exactly (release date left out), and no other versioned model.
    Model,
    /// The answer names exactly one date, and it is the truth.
    Date,
    /// The answer names exactly one weekday, and it is the truth.
    Weekday,
    /// The answer names at least one span of days or weeks, and every one
    /// is within a day of the truth.
    Days,
    /// The answer's first word is the truth.
    Choice,
}

/// One question.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub id: String,
    /// Per-item sets draw one item per family per run.
    #[serde(default)]
    pub family: Option<String>,
    pub question: String,
    pub truth: TruthKey,
    pub rule: Rule,
}

/// A set as written in its file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemSet {
    pub set: String,
    pub version: u32,
    pub measure: Measure,
    #[serde(default)]
    pub delivery: Delivery,
    /// Below this many scored answers a measure reads "not enough yet" (G5).
    pub min_n: u32,
    pub arms: Vec<Arm>,
    pub instructions: String,
    #[serde(rename = "item")]
    pub items: Vec<Item>,
}

/// A parsed set and its digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSet {
    pub set: ItemSet,
    /// The set file's own digest.
    pub file_sha256: String,
    /// The set's definition: the file and, for a set with an input-only
    /// arm, the facts template that arm is given. Results are grouped by it.
    pub sha256: String,
}

/// The digest of the input-only facts template, on every row.
#[must_use]
pub fn facts_sha256() -> String {
    hex(&Sha256::digest(FACTS_V1.as_bytes()))
}

/// The digest of the fresh arm's system prompt.
#[must_use]
pub fn fresh_sha256() -> String {
    hex(&Sha256::digest(FRESH_SYSTEM_V1.as_bytes()))
}

/// A set's items are usable: ids unique, at least one item and one arm,
/// and a family on every item of a per-item set.
fn check_items(set: &ItemSet) -> Result<(), String> {
    let mut ids: Vec<&str> = set.items.iter().map(|i| i.id.as_str()).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(format!("set {} repeats an item id", set.set));
    }
    if set.items.is_empty() || set.arms.is_empty() {
        return Err(format!("set {} has no items or no arms", set.set));
    }
    if set.delivery == Delivery::PerItem && set.items.iter().any(|i| i.family.is_none()) {
        return Err(format!(
            "per-item set {} has an item without a family",
            set.set
        ));
    }
    Ok(())
}

/// The digest of what a set's results depend on: its file, plus the facts
/// template when it has an input-only arm and the fresh-arm prompt when it
/// has a fresh arm. Just the file's digest when it has neither.
fn definition_sha256(file_sha256: &str, arms: &[Arm]) -> String {
    let mut definition = file_sha256.to_string();
    if arms.contains(&Arm::InputOnly) {
        definition.push_str(&format!("\nfacts {}", facts_sha256()));
    }
    if arms.contains(&Arm::Fresh) {
        definition.push_str(&format!("\nfresh {}", fresh_sha256()));
    }
    if definition == file_sha256 {
        definition
    } else {
        hex(&Sha256::digest(definition.as_bytes()))
    }
}

impl LoadedSet {
    fn parse(text: &str) -> Result<Self, String> {
        let set: ItemSet =
            toml::from_str(text).map_err(|e| crate::plugins::config_error::describe(&e))?;
        check_items(&set)?;
        let file_sha256 = hex(&Sha256::digest(text.as_bytes()));
        let sha256 = definition_sha256(&file_sha256, &set.arms);
        Ok(Self {
            set,
            file_sha256,
            sha256,
        })
    }

    /// `<set>/v<version>`, the label every row and report carries.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}/v{}", self.set.set, self.set.version)
    }

    /// The items one run asks, as [`draw_scorable`](Self::draw_scorable), skipping what
    /// could not be scored: for a per-item set, a family's variants are
    /// tried from the hash pick onwards and the first whose truth the run
    /// has is asked; a family with none is not asked at all, so no turn is
    /// spent on an answer that cannot count (audit #17).
    #[must_use]
    pub fn draw_scorable(&self, run_id: &str, can_score: impl Fn(TruthKey) -> bool) -> Vec<&Item> {
        if self.set.delivery == Delivery::Batched {
            return self.set.items.iter().collect();
        }
        self.families()
            .into_iter()
            .filter_map(|family| {
                let variants = self.variants(family);
                let start = pick(run_id, family, variants.len())?;
                (0..variants.len())
                    .map(|offset| variants[(start + offset) % variants.len()])
                    .find(|item| can_score(item.truth))
            })
            .collect()
    }

    /// The families of a per-item set, in file order.
    fn families(&self) -> Vec<&str> {
        let mut families: Vec<&str> = Vec::new();
        for item in &self.set.items {
            let family = item.family.as_deref().unwrap_or_default();
            if !families.contains(&family) {
                families.push(family);
            }
        }
        families
    }

    fn variants(&self, family: &str) -> Vec<&Item> {
        self.set
            .items
            .iter()
            .filter(|i| i.family.as_deref() == Some(family))
            .collect()
    }

    /// The items one run asks, in order: every item for a batched set; for
    /// a per-item set, one per family, picked by a hash of the run id and
    /// the family (so the draw is fixed before any answer exists).
    #[cfg(test)]
    #[must_use]
    pub fn draw(&self, run_id: &str) -> Vec<&Item> {
        self.draw_scorable(run_id, |_| true)
    }

    /// Whether `arm` asks `item`. The input-only arm is handed the facts
    /// template, which is rendered before the turn. The served model is
    /// known only from that turn's stream, so the ceiling does not ask for
    /// it: it covers the facts the template states.
    #[must_use]
    pub fn asks(&self, arm: Arm, item: &Item) -> bool {
        !(arm == Arm::InputOnly && item.truth == TruthKey::ServedModel)
    }

    /// Model turns asking `drawn` items. An input-only arm skips the served
    /// model, so a per-item set spends one fewer turn for each such item.
    #[must_use]
    pub fn turns_for(&self, drawn: &[&Item]) -> u32 {
        let arms = self.set.arms.len();
        match self.set.delivery {
            Delivery::Batched => u32::try_from(arms).unwrap_or(u32::MAX),
            Delivery::PerItem => {
                let mut turns = arms.saturating_mul(drawn.len());
                if self.set.arms.contains(&Arm::InputOnly) {
                    let skipped = drawn
                        .iter()
                        .filter(|item| !self.asks(Arm::InputOnly, item))
                        .count();
                    turns = turns.saturating_sub(skipped);
                }
                u32::try_from(turns).unwrap_or(u32::MAX)
            }
        }
    }

    /// The prompt for one drawn item of a per-item set.
    #[must_use]
    pub fn item_prompt(&self, item: &Item) -> String {
        format!("{}\n\n{}", self.set.instructions.trim(), item.question)
    }

    /// The one batched prompt that asks every item at once.
    #[must_use]
    pub fn batched_prompt(&self) -> String {
        let items: Vec<&Item> = self.set.items.iter().collect();
        self.prompt_for(&items)
    }

    /// A batched prompt for `items`, numbered from 1 in this order.
    #[must_use]
    pub fn prompt_for(&self, items: &[&Item]) -> String {
        let mut prompt = self.set.instructions.trim().to_string();
        prompt.push_str("\n\n");
        for (index, item) in items.iter().enumerate() {
            prompt.push_str(&format!("{}. {}\n", index + 1, item.question));
        }
        prompt
    }
}

/// The variant index a run's hash picks in a family of `len` variants.
fn pick(run_id: &str, family: &str, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let digest = Sha256::digest(format!("{run_id}:{family}").as_bytes());
    let value = u64::from_be_bytes(digest[..8].try_into().ok()?);
    usize::try_from(value % len as u64).ok()
}

/// Every installed item set, in run order.
pub fn all(installed: &InstalledSets) -> Result<Vec<LoadedSet>, String> {
    SET_FILES
        .iter()
        .map(|file| {
            let text = installed.text(file).map_err(|e| e.to_string())?;
            LoadedSet::parse(text).map_err(|e| format!("{file}: {e}"))
        })
        .collect()
}

/// The sets `only` names (by set name), or all of them when it is empty.
/// A name no set has is an error, not an empty run.
pub fn select(only: &[String], installed: &InstalledSets) -> Result<Vec<LoadedSet>, String> {
    let sets = all(installed)?;
    if only.is_empty() {
        return Ok(sets);
    }
    for name in only {
        if !sets
            .iter()
            .any(|s| &s.set.set == name || s.set.set.starts_with(name.as_str()))
        {
            let known: Vec<&str> = sets.iter().map(|s| s.set.set.as_str()).collect();
            return Err(format!(
                "no probe set '{name}' (sets: {})",
                known.join(", ")
            ));
        }
    }
    Ok(sets
        .into_iter()
        .filter(|s| {
            only.iter()
                .any(|name| &s.set.set == name || s.set.set.starts_with(name.as_str()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vigil::probe::sealed::tests::fixture_sets;

    fn all() -> Result<Vec<LoadedSet>, String> {
        super::all(&fixture_sets())
    }

    fn select(only: &[String]) -> Result<Vec<LoadedSet>, String> {
        super::select(only, &fixture_sets())
    }

    #[test]
    fn the_shipped_sets_parse_and_are_labelled() {
        let sets = all().unwrap();
        let known = &sets[0];
        assert_eq!(known.label(), "known-answer/v1");
        assert_eq!(known.set.measure, Measure::KnownAnswer);
        assert_eq!(known.set.arms, [Arm::Pulse, Arm::InputOnly, Arm::Fresh]);
        assert_eq!(known.set.items.len(), 11);
        assert_eq!(known.sha256.len(), 64);
        assert_ne!(
            known.sha256, known.file_sha256,
            "an input-only set's definition includes the facts template"
        );
    }

    #[test]
    fn situation_use_draws_one_item_per_family_fixed_by_the_run() {
        let sets = all().unwrap();
        let situation = &sets[1];
        assert_eq!(situation.label(), "situation-use/v1");
        assert_eq!(situation.set.delivery, Delivery::PerItem);
        let drawn = situation.draw("r");
        assert_eq!(
            situation.turns_for(&drawn),
            11,
            "four families × three arms, minus the served model on input-only"
        );
        assert_eq!(sets[0].turns_for(&sets[0].draw("r")), 3);
        assert!(drawn.iter().any(|item| item.truth == TruthKey::ServedModel));
        assert!(drawn.iter().all(|item| situation.asks(Arm::Pulse, item)));
        assert!(drawn
            .iter()
            .filter(|item| item.truth == TruthKey::ServedModel)
            .all(|item| !situation.asks(Arm::InputOnly, item)));
        let drawn = situation.draw("3fa2b1c4d5e6");
        let families: Vec<&str> = drawn.iter().filter_map(|i| i.family.as_deref()).collect();
        assert_eq!(families, ["tomorrow", "gap", "restart", "model"]);
        assert_eq!(
            drawn,
            situation.draw("3fa2b1c4d5e6"),
            "the same run draws the same"
        );
        let variety: std::collections::BTreeSet<&str> = (0..40)
            .flat_map(|n| situation.draw(&format!("run{n}")))
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(variety.len(), 7, "every variant gets drawn");
        let prompt = situation.item_prompt(drawn[0]);
        assert!(prompt.starts_with("Fixture instructions"));
        assert!(prompt.ends_with(&drawn[0].question));
    }

    #[test]
    fn a_family_without_a_truth_is_not_asked() {
        let situation = &all().unwrap()[1];
        let run = "3fa2b1c4d5e6";
        let everything = situation.draw_scorable(run, |_| true);
        assert_eq!(
            everything,
            situation.draw(run),
            "with every truth, the same draw"
        );
        // No owner message: neither gap variant can be scored.
        let no_gap = situation.draw_scorable(run, |key| {
            !matches!(key, TruthKey::DaysAway | TruthKey::OwnerLastActive)
        });
        let families: Vec<&str> = no_gap.iter().filter_map(|i| i.family.as_deref()).collect();
        assert_eq!(families, ["tomorrow", "restart", "model"]);
        // Only one gap variant scorable: that one is asked, whatever the hash.
        for n in 0..10 {
            let drawn = situation.draw_scorable(&format!("r{n}"), |key| key != TruthKey::DaysAway);
            let gap = drawn
                .iter()
                .find(|i| i.family.as_deref() == Some("gap"))
                .unwrap();
            assert_eq!(gap.truth, TruthKey::OwnerLastActive);
        }
    }

    #[test]
    fn the_batched_prompt_numbers_every_question() {
        let known = &all().unwrap()[0];
        let prompt = known.batched_prompt();
        assert!(prompt.contains("1. Fixture question about the name."));
        assert!(prompt.contains("11. Fixture question about the last task's date."));
        assert!(prompt.contains("UNKNOWN"));
    }

    #[test]
    fn sets_carry_no_answers() {
        // A set never holds a truth value: truths come from the runtime.
        for set in all().unwrap() {
            for item in &set.set.items {
                assert!(
                    !item.question.to_lowercase().contains("answer:"),
                    "{}",
                    item.id
                );
            }
        }
    }

    #[test]
    fn only_selects_by_name_or_prefix_and_refuses_unknown_names() {
        assert_eq!(select(&[]).unwrap().len(), all().unwrap().len());
        assert_eq!(
            select(&["known".into()]).unwrap()[0].set.set,
            "known-answer"
        );
        assert!(select(&["nope".into()]).is_err());
    }

    #[test]
    fn a_repeated_id_is_refused() {
        let text = r#"
set = "x"
version = 1
measure = "known-answer"
min_n = 1
arms = ["pulse"]
instructions = "i"
[[item]]
id = "a"
question = "q"
truth = "today"
rule = "date"
[[item]]
id = "a"
question = "q2"
truth = "today"
rule = "date"
"#;
        assert!(LoadedSet::parse(text).is_err());
    }

    /// A one-item set: `arms` and `delivery` as given, the item with
    /// `family` (a TOML line, or empty).
    fn one_item_set(arms: &str, delivery: &str, family: &str) -> String {
        format!(
            "set = \"x\"\nversion = 1\nmeasure = \"known-answer\"\ndelivery = \"{delivery}\"\n\
             min_n = 1\narms = [{arms}]\ninstructions = \"i\"\n\
             [[item]]\nid = \"a\"\n{family}\nquestion = \"q\"\ntruth = \"today\"\nrule = \"date\"\n"
        )
    }

    #[test]
    fn a_set_without_arms_or_items_and_a_per_item_set_without_families_are_refused() {
        let error = |text: &str| LoadedSet::parse(text).unwrap_err();
        assert_eq!(
            error(&one_item_set("", "batched", "")),
            "set x has no items or no arms"
        );
        let no_items = "set = \"x\"\nversion = 1\nmeasure = \"known-answer\"\nmin_n = 1\n\
                        arms = [\"pulse\"]\ninstructions = \"i\"\nitem = []\n";
        assert_eq!(error(no_items), "set x has no items or no arms");
        assert_eq!(
            error(&one_item_set("\"pulse\"", "per-item", "")),
            "per-item set x has an item without a family"
        );
        assert!(LoadedSet::parse(&one_item_set("\"pulse\"", "per-item", "family = \"f\"")).is_ok());
        assert!(LoadedSet::parse("set = ").is_err(), "not TOML");
    }

    #[test]
    fn a_sets_definition_covers_the_facts_template_and_fresh_prompt_its_arms_use() {
        let parsed = |arms: &str| LoadedSet::parse(&one_item_set(arms, "batched", "")).unwrap();
        let pulse_only = parsed("\"pulse\"");
        assert_eq!(
            pulse_only.sha256, pulse_only.file_sha256,
            "a pulse-only set is its file"
        );
        let with_facts = parsed("\"pulse\", \"input-only\"");
        let expected = hex(&Sha256::digest(
            format!("{}\nfacts {}", with_facts.file_sha256, facts_sha256()).as_bytes(),
        ));
        assert_eq!(with_facts.sha256, expected);
        let every_arm = parsed("\"pulse\", \"input-only\", \"fresh\"");
        let expected = hex(&Sha256::digest(
            format!(
                "{}\nfacts {}\nfresh {}",
                every_arm.file_sha256,
                facts_sha256(),
                fresh_sha256()
            )
            .as_bytes(),
        ));
        assert_eq!(every_arm.sha256, expected);
    }
}
