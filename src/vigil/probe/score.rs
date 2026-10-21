//! Rule-only scoring of probe answers (PN-144). No judge, no model call:
//! every rule here is pre-registered on the methods pages and tested.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

use super::sets::Rule;

/// The version of these scoring rules, on every row: a change to any rule
/// is a new version, and results are never pooled across one. 1 was the
/// build of 2026-10-17; 2 the audit's model, date and choice rules; 3 the
/// three-way abstention and markdown-stripped UNKNOWN; 4 the model rule
/// comparing name words and version parts exactly (audit R2-4); 5 whole-word
/// abstentions and dates written with "of", "the" or a time (R2-8); 6 an
/// abstention that goes on to name something offers a value, and a paired
/// answer counts as remembered only when it is not an abstention (R3-B).
pub const SCORER_VERSION: u32 = 6;
use super::truth::Truth;

/// How one answer scored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Correct,
    Wrong,
    /// The answer said UNKNOWN. Counted, never correct.
    Unknown,
    /// No answer line for the item. Counted, never correct.
    Unanswered,
    /// The runtime had no truth: not scored, not counted.
    NoTruth,
    /// The item's turn read a held-out store or called localhost: not
    /// scored, not counted, shown as void.
    Void,
    /// The turn failed: not scored, not counted.
    Error,
}

impl Outcome {
    /// Does this outcome count towards n?
    #[must_use]
    pub fn counts(self) -> bool {
        matches!(
            self,
            Self::Correct | Self::Wrong | Self::Unknown | Self::Unanswered
        )
    }
}

/// Split a batched reply into one answer per item: the text after `<n>:`
/// (or `<n>.` / `<n>)`) on the line that starts with it. Items without such
/// a line are `None`. The first answer for a number wins.
#[must_use]
pub fn split_batched(reply: &str, items: usize) -> Vec<Option<String>> {
    let mut answers = vec![None; items];
    for line in reply.lines() {
        let line = line.trim().trim_start_matches(['*', '-', '#', ' ']).trim();
        let digits: String = line.chars().take_while(char::is_ascii_digit).collect();
        let Ok(number) = digits.parse::<usize>() else {
            continue;
        };
        let rest = line[digits.len()..].trim_start_matches('*');
        let Some(answer) = rest
            .strip_prefix(':')
            .or_else(|| rest.strip_prefix('.'))
            .or_else(|| rest.strip_prefix(')'))
        else {
            continue;
        };
        if let Some(slot) = number.checked_sub(1).and_then(|i| answers.get_mut(i)) {
            if slot.is_none() {
                *slot = Some(answer.trim().trim_matches('*').trim().to_string());
            }
        }
    }
    answers
}

/// Score one answer against its truth by `rule`.
#[must_use]
pub fn score(rule: Rule, truth: Option<&Truth>, answer: Option<&str>) -> Outcome {
    let Some(truth) = truth else {
        return Outcome::NoTruth;
    };
    let Some(answer) = answer.map(str::trim).filter(|a| !a.is_empty()) else {
        return Outcome::Unanswered;
    };
    if says_unknown(answer) {
        return Outcome::Unknown;
    }
    let correct = match rule {
        Rule::Contains => contains_words(answer, &truth.value),
        Rule::Model => model_matches(answer, &truth.value),
        Rule::Date => NaiveDate::parse_from_str(&truth.value, "%Y-%m-%d")
            .is_ok_and(|t| date_matches(answer, t)),
        Rule::Weekday => {
            let days = weekdays_in(answer);
            days.len() == 1 && days[0].eq_ignore_ascii_case(&truth.value)
        }
        Rule::Days => truth.value.parse::<i64>().is_ok_and(|t| {
            let spans = day_spans_in(answer);
            !spans.is_empty() && spans.iter().all(|d| (d - t).abs() <= 1)
        }),
        Rule::Choice => first_word(answer).eq_ignore_ascii_case(&truth.value),
    };
    if correct {
        Outcome::Correct
    } else {
        Outcome::Wrong
    }
}

/// Phrases that, in an answer, mean "I was not told" (pre-registered on the
/// paired-facts methods page).
const ABSTENTIONS: &[&str] = &[
    "don't know",
    "do not know",
    "dont know",
    "not sure",
    "no idea",
    "haven't been told",
    "have not been told",
    "never told",
    "never mentioned",
    "not mentioned",
    "no record",
    "wasn't told",
    "was not told",
];

/// Phrases that, beside an abstention, offer a value anyway (pre-registered
/// on the paired-facts methods page).
const GUESSES: &[&str] = &[
    "maybe",
    "perhaps",
    "probably",
    "possibly",
    "likely",
    "might be",
    "could be",
    "my guess",
    "best guess",
    "i guess",
    "i'd guess",
    "i would guess",
    "if i had to",
    "i'd say",
    "i think it",
    "i believe it",
];

/// Words that, beside an abstention, name nothing: function words and the
/// vocabulary of "I was never told" (pre-registered on the paired-facts
/// methods page). Any other word left after an abstention offers a value.
const NO_VALUE_WORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "i",
    "me",
    "my",
    "mine",
    "you",
    "your",
    "yours",
    "it",
    "its",
    "we",
    "our",
    "us",
    "they",
    "them",
    "their",
    "this",
    "that",
    "these",
    "those",
    "there",
    "here",
    "what",
    "which",
    "who",
    "when",
    "where",
    "why",
    "how",
    "am",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "have",
    "has",
    "had",
    "do",
    "does",
    "did",
    "can",
    "could",
    "would",
    "will",
    "shall",
    "should",
    "may",
    "might",
    "must",
    "not",
    "no",
    "nor",
    "t",
    "s",
    "m",
    "re",
    "ve",
    "ll",
    "d",
    "isn",
    "aren",
    "wasn",
    "weren",
    "haven",
    "hasn",
    "hadn",
    "don",
    "doesn",
    "didn",
    "cannot",
    "couldn",
    "wouldn",
    "won",
    "shouldn",
    "to",
    "of",
    "in",
    "on",
    "at",
    "for",
    "from",
    "with",
    "without",
    "about",
    "by",
    "as",
    "so",
    "and",
    "or",
    "but",
    "if",
    "yet",
    "still",
    "just",
    "any",
    "anything",
    "sorry",
    "afraid",
    "really",
    "honestly",
    "unfortunately",
    "tell",
    "told",
    "say",
    "said",
    "share",
    "shared",
    "mention",
    "mentioned",
    "remember",
    "recall",
    "know",
    "knew",
    "idea",
    "information",
    "record",
    "memory",
    "conversation",
    "conversations",
    "unknown",
];

/// What kind of answer it is, when the answer may be "I don't know".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnswerKind {
    /// Says it doesn't know, and offers nothing.
    Abstained,
    /// Says it doesn't know, then offers a value anyway.
    HedgedWithValue,
    /// Gives a value.
    Value,
}

/// `answer` without the markdown and quoting a model wraps around it.
fn unwrapped(answer: &str) -> &str {
    answer
        .trim()
        .trim_start_matches(|c: char| "*_`#>\"' -".contains(c))
        .trim()
}

/// Does the answer start with UNKNOWN (markdown and quotes stripped)?
fn says_unknown(answer: &str) -> bool {
    unwrapped(answer)
        .to_ascii_uppercase()
        .starts_with("UNKNOWN")
}

/// Classify an answer to `question` three ways. An abstention (UNKNOWN at
/// its start, or a pre-registered "not told" phrase) is `Abstained` only
/// when it offers nothing: one that carries a guess phrase, or goes on to
/// name something ([`names_something`]: "not sure, but lychee"), is
/// `HedgedWithValue`. Anything else is a `Value`.
#[must_use]
pub fn answer_kind(answer: &str, question: &str) -> AnswerKind {
    let lower = answer.to_lowercase().replace('’', "'");
    // Whole words, in order: "no ideas" or "no recordings" are not
    // abstentions.
    let abstains = says_unknown(answer) || ABSTENTIONS.iter().any(|p| contains_words(&lower, p));
    if !abstains {
        return AnswerKind::Value;
    }
    let guesses = GUESSES.iter().any(|g| contains_words(&lower, g));
    if guesses || names_something(&lower, question) {
        AnswerKind::HedgedWithValue
    } else {
        AnswerKind::Abstained
    }
}

/// Does `answer` hold a word that is not UNKNOWN, not a word of an
/// abstention phrase, not a word of the `question` it answers (an
/// abstention often restates it: "I don't know your favourite sundial"), and
/// not a [`NO_VALUE_WORDS`] word? Such a word offers a value.
fn names_something(answer: &str, question: &str) -> bool {
    let question = words(question);
    words(answer)
        .iter()
        .any(|word| !names_nothing(word, &question))
}

fn names_nothing(word: &str, question: &[String]) -> bool {
    NO_VALUE_WORDS.contains(&word)
        || question.iter().any(|q| q == word)
        || ABSTENTIONS
            .iter()
            .any(|phrase| words(phrase).iter().any(|w| w == word))
}

/// The answer's words for reading dates: as [`words`], without the "of" and
/// "the" a date is often written with (`the 17th of October`, `October the
/// 17th`), so the day and the month sit side by side.
fn date_words(answer: &str) -> Vec<String> {
    words(answer)
        .into_iter()
        .filter(|w| w != "of" && w != "the")
        .collect()
}

/// Lowercase words of letters and digits.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Do `truth`'s words appear, contiguous and in order, among `answer`'s?
fn contains_words(answer: &str, truth: &str) -> bool {
    let answer = words(answer);
    let truth = words(truth);
    !truth.is_empty() && answer.windows(truth.len()).any(|w| w == truth.as_slice())
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

const WEEKDAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

fn month_number(word: &str) -> Option<u32> {
    let word = word.to_lowercase();
    if word.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|m| m.starts_with(&word) || word == *m)
        .and_then(|i| u32::try_from(i + 1).ok())
}

/// Every date the answer names: `YYYY-MM-DD`, `D Month YYYY` or
/// `Month D, YYYY` (month names in English, full or abbreviated).
fn dates_in(answer: &str) -> Vec<NaiveDate> {
    let mut dates = Vec::new();
    for token in answer.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')') {
        let token = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
        // `2026-10-17`, also with a time after it (`2026-10-17T10:00`).
        if let Some(date) = token
            .get(..10)
            .filter(|_| token.len() == 10 || token[10..].starts_with(['T', 't']))
            .and_then(|day| NaiveDate::parse_from_str(day, "%Y-%m-%d").ok())
        {
            dates.push(date);
        }
    }
    let tokens = date_words(answer);
    for window in tokens.windows(3) {
        let (a, b, c) = (&window[0], &window[1], &window[2]);
        let numeric = |s: &str| {
            s.trim_end_matches(|ch: char| ch.is_ascii_alphabetic())
                .parse::<u32>()
                .ok()
        };
        let year = c.parse::<i32>().ok().filter(|y| *y > 1900);
        let date = match (
            numeric(a),
            month_number(b),
            month_number(a),
            numeric(b),
            year,
        ) {
            (Some(day), Some(month), _, _, Some(year)) => NaiveDate::from_ymd_opt(year, month, day),
            (_, _, Some(month), Some(day), Some(year)) => NaiveDate::from_ymd_opt(year, month, day),
            _ => None,
        };
        if let Some(date) = date {
            if !dates.contains(&date) {
                dates.push(date);
            }
        }
    }
    dates
}

/// Model family names: one of these words starts a model mention.
const MODEL_FAMILIES: &[&str] = &[
    "claude", "opus", "sonnet", "haiku", "grok", "gpt", "gemini", "llama", "mistral", "qwen",
    "deepseek", "codex",
];

/// A model as named in text: its name words (`claude`, `opus`) and its
/// version parts (`5`, `5`), each part compared exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelName {
    names: std::collections::BTreeSet<String>,
    version: Vec<String>,
}

/// Does the answer name the truth's model, and no other? The truth and every
/// model the answer names are split into name words and version parts,
/// without a trailing release date (`claude-opus-5-5-20261001` is
/// `claude opus` `5 5`), and compared exactly: `Claude Opus 5.5` is
/// `claude-opus-5-5`, while `Opus 5.5` (a name word short), `Claude Opus 5`
/// or, for a served `claude-opus-5`, `Claude Opus 5.5` (a version part
/// more) are not. Any other versioned model the answer names (`A or B`)
/// is a hedge and wrong; a bare name that is part of the truth's
/// (`Claude Code`) is not a model mention against it.
fn model_matches(answer: &str, truth: &str) -> bool {
    let Some(truth) = parse_model(truth) else {
        return false;
    };
    let mentions = model_mentions(answer, &truth.names);
    mentions.contains(&truth)
        && mentions
            .iter()
            .all(|m| *m == truth || (m.version.is_empty() && m.names.is_subset(&truth.names)))
}

/// The truth's model: its name words and version parts. `None` when it has
/// no name word.
fn parse_model(id: &str) -> Option<ModelName> {
    let (mut names, mut version) = (std::collections::BTreeSet::new(), Vec::new());
    for part in model_parts(id) {
        if is_version_part(&part) {
            version.push(part);
        } else {
            names.insert(part);
        }
    }
    (!names.is_empty()).then_some(ModelName { names, version })
}

/// Every model the answer names: a family word (or one of the truth's own
/// name words), the name words and version parts right after it.
fn model_mentions(
    answer: &str,
    truth_names: &std::collections::BTreeSet<String>,
) -> Vec<ModelName> {
    let parts = model_parts(answer);
    let is_name = |p: &str| MODEL_FAMILIES.contains(&p) || truth_names.contains(p);
    let mut mentions = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        if !is_name(&parts[i]) {
            i += 1;
            continue;
        }
        let mut mention = ModelName {
            names: std::collections::BTreeSet::new(),
            version: Vec::new(),
        };
        while i < parts.len() {
            let part = &parts[i];
            if is_version_part(part) {
                mention.version.push(part.clone());
            } else if is_name(part) && mention.version.is_empty() {
                mention.names.insert(part.clone());
            } else {
                break;
            }
            i += 1;
        }
        mentions.push(mention);
    }
    mentions
}

/// Lowercase words and numbers of `text`, release dates left out: each
/// whitespace-separated token loses a trailing date and is split on
/// `-`, `.`, `_`, `@` and punctuation; a bare date or year is dropped.
fn model_parts(text: &str) -> Vec<String> {
    text.split_whitespace()
        .flat_map(|token| {
            let token = token
                .trim_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_lowercase();
            let undated = strip_model_date(&token).to_string();
            undated
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|p| !is_compact_date(p) && !is_year(p))
        .collect()
}

/// A version part: one or two digits, maybe a short letter suffix (`4o`).
fn is_version_part(part: &str) -> bool {
    let digits = part.chars().take_while(char::is_ascii_digit).count();
    (1..=2).contains(&digits)
        && part[digits..].len() <= 3
        && part[digits..].chars().all(|c| c.is_ascii_lowercase())
}

/// A four-digit year, which in a model answer is a date, not a version.
fn is_year(part: &str) -> bool {
    part.len() == 4 && part.starts_with("20") && part.chars().all(|c| c.is_ascii_digit())
}

/// `id` without a trailing release date: `-20261001`, `@20261001` or
/// `-2026-10-01`.
fn strip_model_date(id: &str) -> &str {
    if let Some((head, tail)) = id.rsplit_once(['-', '@', '_']) {
        if is_compact_date(tail) {
            return head;
        }
    }
    let parts: Vec<&str> = id.rsplitn(4, '-').collect();
    if let [day, month, year, head] = parts.as_slice() {
        if NaiveDate::parse_from_str(&format!("{year}-{month}-{day}"), "%Y-%m-%d").is_ok()
            && year.len() == 4
        {
            return head;
        }
    }
    id
}

/// `YYYYMMDD` for a real date in 2000–2099.
fn is_compact_date(text: &str) -> bool {
    text.len() == 8 && text.starts_with("20") && NaiveDate::parse_from_str(text, "%Y%m%d").is_ok()
}

/// The date rule: the answer names at least one date, and every date it
/// names is the truth. A date with its year counts as named; one without a
/// year counts only when it also names the weekday (`Saturday 17 October`)
/// and both match the truth. A yearless date with a wrong day, month or
/// weekday is wrong; a yearless date with no weekday names no date.
fn date_matches(answer: &str, truth: NaiveDate) -> bool {
    let full = dates_in(answer);
    let yearless = yearless_dates_in(answer);
    let full_ok = full.iter().all(|d| *d == truth);
    let yearless_ok = yearless.iter().all(|y| {
        y.day == truth.day()
            && y.month == truth.month()
            && y.weekday.is_none_or(|w| w == truth.weekday())
    });
    let named = !full.is_empty() || yearless.iter().any(|y| y.weekday.is_some());
    named && full_ok && yearless_ok
}

/// A date written without its year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct YearlessDate {
    day: u32,
    month: u32,
    weekday: Option<chrono::Weekday>,
}

/// Every `[weekday] D Month` or `[weekday] Month D` the answer names with
/// no year after it.
fn yearless_dates_in(answer: &str) -> Vec<YearlessDate> {
    let tokens = date_words(answer);
    let day_of = |t: &str| {
        t.trim_end_matches(|c: char| c.is_ascii_alphabetic())
            .parse::<u32>()
            .ok()
            .filter(|d| (1..=31).contains(d))
            .filter(|_| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
    };
    let is_year = |t: Option<&String>| {
        t.and_then(|t| t.parse::<i32>().ok())
            .is_some_and(|y| y > 1900)
    };
    let weekday_before = |i: usize| {
        let before = i.checked_sub(1).map(|j| tokens[j].as_str());
        let skip_the = if before == Some("the") {
            i.checked_sub(2)
        } else {
            i.checked_sub(1)
        };
        skip_the.and_then(|j| weekday_of(&tokens[j]))
    };
    let mut found = Vec::new();
    // A month read as part of one date never starts the next ("17 October,
    // 10:00" is one date, not also October 10).
    let mut next_free = 0;
    for i in 0..tokens.len().saturating_sub(1) {
        if i < next_free {
            continue;
        }
        let (a, b) = (&tokens[i], &tokens[i + 1]);
        let parsed = match (day_of(a), month_number(b), month_number(a), day_of(b)) {
            (Some(day), Some(month), _, _) => Some((day, month)),
            (_, _, Some(month), Some(day)) => Some((day, month)),
            _ => None,
        };
        if let Some((day, month)) = parsed {
            next_free = i + 2;
            if !is_year(tokens.get(i + 2)) {
                found.push(YearlessDate {
                    day,
                    month,
                    weekday: weekday_before(i),
                });
            }
        }
    }
    found
}

/// A weekday name, full or abbreviated.
fn weekday_of(word: &str) -> Option<chrono::Weekday> {
    use chrono::Weekday::*;
    let day = match word {
        "monday" | "mon" => Mon,
        "tuesday" | "tue" | "tues" => Tue,
        "wednesday" | "wed" => Wed,
        "thursday" | "thu" | "thur" | "thurs" => Thu,
        "friday" | "fri" => Fri,
        "saturday" | "sat" => Sat,
        "sunday" | "sun" => Sun,
        _ => return None,
    };
    Some(day)
}

/// Number words a span of days may be written in.
const NUMBER_WORDS: [&str; 21] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "twenty",
];

/// Every span of days the answer names, in days: `<n> day(s)` or
/// `<n> week(s)`, with `<n>` in digits, a number word up to twenty, or
/// `a`/`an`.
fn day_spans_in(answer: &str) -> Vec<i64> {
    let tokens = words(answer);
    tokens
        .windows(2)
        .filter_map(|pair| {
            let count = match pair[0].as_str() {
                "a" | "an" => Some(1),
                word => word.parse::<i64>().ok().or_else(|| {
                    NUMBER_WORDS
                        .iter()
                        .position(|w| *w == word)
                        .map(|n| n as i64)
                }),
            }?;
            match pair[1].as_str() {
                "day" | "days" => Some(count),
                "week" | "weeks" => Some(count * 7),
                _ => None,
            }
        })
        .collect()
}

/// The answer's first word, without markdown or punctuation around it.
fn first_word(answer: &str) -> String {
    answer
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .find(|w| !w.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Every weekday the answer names, capitalised, without repeats.
fn weekdays_in(answer: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in words(answer) {
        if let Some(day) = WEEKDAYS.iter().find(|d| **d == word) {
            let mut name = day.to_string();
            name[..1].make_ascii_uppercase();
            if !found.contains(&name) {
                found.push(name);
            }
        }
    }
    found
}

/// A proportion and its 95% Wilson score interval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Proportion {
    pub k: u32,
    pub n: u32,
    pub low: f64,
    pub high: f64,
}

/// The 95% Wilson score interval for `k` of `n` (z = 1.96). With `n = 0`
/// the interval is the whole of [0, 1].
#[must_use]
pub fn wilson(k: u32, n: u32) -> Proportion {
    if n == 0 {
        return Proportion {
            k,
            n,
            low: 0.0,
            high: 1.0,
        };
    }
    let z = 1.96_f64;
    let nf = f64::from(n);
    let p = f64::from(k) / nf;
    let denominator = 1.0 + z * z / nf;
    let centre = (p + z * z / (2.0 * nf)) / denominator;
    let half = z * ((p * (1.0 - p) / nf) + z * z / (4.0 * nf * nf)).sqrt() / denominator;
    Proportion {
        k,
        n,
        low: (centre - half).max(0.0),
        high: (centre + half).min(1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn truth(value: &str) -> Truth {
        Truth {
            value: value.into(),
            source: "test",
        }
    }

    #[test]
    fn batched_replies_split_by_number() {
        let reply = "Here you go:\n1: Echo\n2. Dee\n**3:** claude\n- 4) opus\n5: x\n5: second answer ignored\n12: out of range";
        let answers = split_batched(reply, 6);
        assert_eq!(answers[0].as_deref(), Some("Echo"));
        assert_eq!(answers[1].as_deref(), Some("Dee"));
        assert_eq!(answers[2].as_deref(), Some("claude"));
        assert_eq!(answers[3].as_deref(), Some("opus"));
        assert_eq!(answers[4].as_deref(), Some("x"));
        assert_eq!(answers[5], None);
    }

    #[test]
    fn contains_matches_whole_words_in_order() {
        let t = truth("Echo");
        assert_eq!(
            score(Rule::Contains, Some(&t), Some("My name is Echo.")),
            Outcome::Correct
        );
        assert_eq!(
            score(Rule::Contains, Some(&t), Some("Echoes")),
            Outcome::Wrong
        );
        let task = truth("morning-review");
        assert_eq!(
            score(Rule::Contains, Some(&task), Some("the morning review task")),
            Outcome::Correct
        );
        assert_eq!(
            score(Rule::Contains, Some(&task), Some("review morning")),
            Outcome::Wrong
        );
    }

    #[test]
    fn models_match_on_letters_and_digits() {
        let t = truth("claude-opus-5-5");
        assert_eq!(
            score(Rule::Model, Some(&t), Some("Claude Opus 5.5")),
            Outcome::Correct
        );
        assert_eq!(
            score(Rule::Model, Some(&t), Some("Claude Opus 5")),
            Outcome::Wrong
        );
        let grok = truth("grok-4.7");
        assert_eq!(
            score(Rule::Model, Some(&grok), Some("grok 4.7")),
            Outcome::Correct
        );
        assert_eq!(
            score(Rule::Model, Some(&grok), Some("grok-4.6")),
            Outcome::Wrong
        );
    }

    /// Audit #6: a dated id is the same model; naming two is a hedge.
    #[test]
    fn dated_model_ids_match_and_hedged_answers_do_not() {
        let dated = truth("claude-opus-5-5-20261001");
        for ok in [
            "Claude Opus 5.5",
            "claude-opus-5-5",
            "claude-opus-5-5-20261001",
            "I'm Claude Opus 5.5 (claude-opus-5-5-20261001), running in Claude Code.",
        ] {
            assert_eq!(
                score(Rule::Model, Some(&dated), Some(ok)),
                Outcome::Correct,
                "{ok}"
            );
        }
        let plain = truth("claude-opus-5-5");
        assert_eq!(
            score(Rule::Model, Some(&plain), Some("claude-opus-5-5-20261001")),
            Outcome::Correct
        );
        for hedge in [
            "Claude Opus 5.5 or Claude Sonnet 5",
            "probably claude-opus-5-5, maybe claude-opus-5-1",
        ] {
            assert_eq!(
                score(Rule::Model, Some(&plain), Some(hedge)),
                Outcome::Wrong,
                "{hedge}"
            );
        }
        let grok = truth("grok-4.7");
        assert_eq!(
            score(Rule::Model, Some(&grok), Some("grok-4.6 or grok-4.7")),
            Outcome::Wrong,
            "a hedge across versions"
        );
        assert_eq!(strip_model_date("model-2026-10-01"), "model");
        assert_eq!(
            score(
                Rule::Model,
                Some(&plain),
                Some("Claude Opus 5.5 (released 2026)")
            ),
            Outcome::Correct,
            "a year is not a version part"
        );
        assert_eq!(strip_model_date("grok-4.7"), "grok-4.7");
    }

    /// Audit R2-4: name words and version parts are compared exactly, so a
    /// truth that is a prefix of the answer (or the answer of the truth) is
    /// a different model.
    #[test]
    fn a_model_is_its_exact_name_and_version() {
        let served = truth("claude-opus-5");
        for wrong in ["Opus 5.5", "Claude Opus 5.5", "claude-opus-5-5"] {
            assert_eq!(
                score(Rule::Model, Some(&served), Some(wrong)),
                Outcome::Wrong,
                "{wrong}"
            );
        }
        assert_eq!(
            score(Rule::Model, Some(&served), Some("Claude Opus 5")),
            Outcome::Correct
        );
        let grok = truth("grok-4");
        assert_eq!(
            score(Rule::Model, Some(&grok), Some("grok 4.7")),
            Outcome::Wrong
        );
        assert_eq!(
            score(Rule::Model, Some(&grok), Some("Grok 4")),
            Outcome::Correct
        );
        let opus = truth("claude-opus-5-5");
        for wrong in [
            "Opus 5.5",
            "Opus 5.5 or Claude Opus 5.5",
            "Claude Opus 5.5 or Opus 5.5",
            "Claude Opus 5.5, or maybe Sonnet 5.5",
            "Claude 5.5",
        ] {
            assert_eq!(
                score(Rule::Model, Some(&opus), Some(wrong)),
                Outcome::Wrong,
                "{wrong}"
            );
        }
        // A truth with no known family still matches by its own name words.
        let other = truth("kimi-k2-0905");
        assert_eq!(
            score(Rule::Model, Some(&other), Some("Kimi K2 0905")),
            Outcome::Correct
        );
    }

    #[test]
    fn dates_must_name_exactly_the_truth() {
        let t = truth("2026-10-17");
        for ok in [
            "2026-10-17",
            "17 October 2026",
            "October 17, 2026",
            "Sat 17 Oct 2026",
            "17th October 2026",
        ] {
            assert_eq!(
                score(Rule::Date, Some(&t), Some(ok)),
                Outcome::Correct,
                "{ok}"
            );
        }
        for wrong in [
            "2026-10-16",
            "yesterday",
            "2026-10-16 or 2026-10-17",
            "17/10/2026",
        ] {
            assert_eq!(
                score(Rule::Date, Some(&t), Some(wrong)),
                Outcome::Wrong,
                "{wrong}"
            );
        }
    }

    /// Audit #6: a calendar line without the year is right when its weekday
    /// and day agree with the truth.
    #[test]
    fn a_date_without_a_year_counts_when_weekday_and_day_match() {
        let t = truth("2026-10-17"); // a Saturday
        for ok in [
            "Call with the accountant — Saturday 17 October, 10:00",
            "Sat, Oct 17 at 10:00",
            "Saturday the 17th October",
            "Saturday 17 October 2026",
        ] {
            assert_eq!(
                score(Rule::Date, Some(&t), Some(ok)),
                Outcome::Correct,
                "{ok}"
            );
        }
        for wrong in [
            "Friday 17 October",
            "Saturday 18 October",
            "17 October",
            "Saturday 17 October, or Sunday 18 October",
        ] {
            assert_eq!(
                score(Rule::Date, Some(&t), Some(wrong)),
                Outcome::Wrong,
                "{wrong}"
            );
        }
    }

    /// Audit R2-8: common ways of writing a date.
    #[test]
    fn dates_are_read_however_they_are_phrased() {
        let t = truth("2026-10-17"); // a Saturday
        for ok in [
            "the 17th of October 2026",
            "October the 17th, 2026",
            "Saturday the 17th of October",
            "Saturday, October 17th",
            "2026-10-17T10:00",
        ] {
            assert_eq!(
                score(Rule::Date, Some(&t), Some(ok)),
                Outcome::Correct,
                "{ok}"
            );
        }
        assert_eq!(
            score(Rule::Date, Some(&t), Some("the 18th of October 2026")),
            Outcome::Wrong
        );
    }

    #[test]
    fn abstention_phrases_are_whole_words() {
        for value in [
            "no ideas came to me: carrots",
            "there are no recordings: blue",
        ] {
            assert_eq!(answer_kind(value, ""), AnswerKind::Value, "{value}");
        }
        assert_eq!(answer_kind("No idea.", ""), AnswerKind::Abstained);
    }

    #[test]
    fn weekdays_must_name_exactly_one() {
        let t = truth("Friday");
        assert_eq!(
            score(Rule::Weekday, Some(&t), Some("It's Friday.")),
            Outcome::Correct
        );
        assert_eq!(
            score(Rule::Weekday, Some(&t), Some("Thursday")),
            Outcome::Wrong
        );
        assert_eq!(
            score(Rule::Weekday, Some(&t), Some("Thursday or Friday")),
            Outcome::Wrong,
            "hedging across days is not an answer"
        );
    }

    #[test]
    fn day_spans_must_all_be_within_a_day() {
        let t = truth("3");
        for ok in [
            "Welcome back — it's been 3 days.",
            "three days since you wrote",
            "4 days",
            "2 days ago",
        ] {
            assert_eq!(
                score(Rule::Days, Some(&t), Some(ok)),
                Outcome::Correct,
                "{ok}"
            );
        }
        for wrong in ["a week", "5 days", "it has been a while", "2 or 6 days"] {
            assert_eq!(
                score(Rule::Days, Some(&t), Some(wrong)),
                Outcome::Wrong,
                "{wrong}"
            );
        }
        assert_eq!(
            score(Rule::Days, Some(&truth("7")), Some("a week")),
            Outcome::Correct
        );
    }

    #[test]
    fn a_choice_is_the_first_word() {
        let t = truth("RESTARTED");
        assert_eq!(
            score(
                Rule::Choice,
                Some(&t),
                Some("**RESTARTED** — I lost the session.")
            ),
            Outcome::Correct
        );
        assert_eq!(
            score(Rule::Choice, Some(&t), Some("SAME. Nothing changed.")),
            Outcome::Wrong
        );
        assert_eq!(
            score(Rule::Choice, Some(&t), Some("I think RESTARTED")),
            Outcome::Wrong,
            "only the first word counts"
        );
    }

    #[test]
    fn unknown_unanswered_and_no_truth_are_distinct() {
        let t = truth("Echo");
        assert_eq!(
            score(Rule::Contains, Some(&t), Some("UNKNOWN")),
            Outcome::Unknown
        );
        assert_eq!(
            score(Rule::Contains, Some(&t), Some("unknown, sorry")),
            Outcome::Unknown
        );
        assert_eq!(score(Rule::Contains, Some(&t), None), Outcome::Unanswered);
        assert_eq!(
            score(Rule::Contains, Some(&t), Some("  ")),
            Outcome::Unanswered
        );
        assert_eq!(score(Rule::Contains, None, Some("Echo")), Outcome::NoTruth);
        assert!(Outcome::Unknown.counts() && Outcome::Unanswered.counts());
        assert!(!Outcome::NoTruth.counts() && !Outcome::Void.counts() && !Outcome::Error.counts());
    }

    #[test]
    fn abstentions_are_unknown_or_a_registered_phrase() {
        for yes in [
            "UNKNOWN",
            "**UNKNOWN**",
            "`UNKNOWN`",
            "> UNKNOWN",
            "I don't know.",
            "You never told me.",
            "I have no record of that",
            "I don’t know",
        ] {
            assert_eq!(answer_kind(yes, ""), AnswerKind::Abstained, "{yes}");
        }
        for value in ["Carrots", "I know: carrots", "Known: Lychee"] {
            assert_eq!(answer_kind(value, ""), AnswerKind::Value, "{value}");
        }
    }

    /// Audit #11: "not sure, but probably carrots" offers a value.
    #[test]
    fn an_abstention_with_a_guess_is_hedged_with_a_value() {
        for hedged in [
            "I'm not sure, but probably carrots.",
            "UNKNOWN — maybe lychee?",
            "You never told me; if I had to guess, red.",
            "I don't know. My guess would be mango.",
        ] {
            assert_eq!(
                answer_kind(hedged, ""),
                AnswerKind::HedgedWithValue,
                "{hedged}"
            );
        }
    }

    /// R3-B: an abstention that goes on to name something offers a value,
    /// with or without a listed guess phrase; one that only says it was not
    /// told, or restates the question, offers nothing.
    #[test]
    fn an_abstention_that_names_something_is_hedged_with_a_value() {
        let question = "Fixture instructions: the answer, or UNKNOWN.\n\n\
                        What is your owner's favourite sundial?";
        for hedged in [
            "Not sure, but lychee",
            "No idea — lychee?",
            "I don't know. Lychee",
            "UNKNOWN (lychee)",
            "Not sure, 42",
        ] {
            assert_eq!(
                answer_kind(hedged, question),
                AnswerKind::HedgedWithValue,
                "{hedged}"
            );
        }
        for abstained in [
            "UNKNOWN",
            "I don't know.",
            "I'm not sure what your owner's favourite sundial is.",
            "Sorry, I don't know — you never told me.",
            "I'm afraid I have no information about that; it was never mentioned.",
            "No idea, you haven't told me yet.",
        ] {
            assert_eq!(
                answer_kind(abstained, question),
                AnswerKind::Abstained,
                "{abstained}"
            );
        }
    }

    /// Audit #11: UNKNOWN wrapped in markdown is still UNKNOWN.
    #[test]
    fn markdown_unknown_is_unknown() {
        let t = truth("Echo");
        for wrapped in [
            "**UNKNOWN**",
            "_UNKNOWN_",
            "`UNKNOWN`",
            "> UNKNOWN",
            "\"UNKNOWN\"",
        ] {
            assert_eq!(
                score(Rule::Contains, Some(&t), Some(wrapped)),
                Outcome::Unknown,
                "{wrapped}"
            );
        }
    }

    #[test]
    fn wilson_matches_known_values() {
        let p = wilson(7, 10);
        assert!((p.low - 0.3968).abs() < 1e-3, "{p:?}");
        assert!((p.high - 0.8922).abs() < 1e-3, "{p:?}");
        let zero = wilson(0, 20);
        assert_eq!(zero.low, 0.0);
        assert!((zero.high - 0.1611).abs() < 1e-3, "{zero:?}");
        let none = wilson(0, 0);
        assert_eq!((none.low, none.high), (0.0, 1.0));
    }
}
