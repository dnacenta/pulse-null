//! Short, honest labels for the 6-field cron expressions in `schedule.json`.
//!
//! The scheduler speaks cron; operators reading a task list do not. This
//! module turns `0 10,30,50 * * * *` into `every 20m at :10,:30,:50`.
//!
//! The contract is deliberately narrow: every expression this module claims
//! to understand is rendered *exactly*, and anything outside that set is
//! returned unchanged rather than approximated. A wrong cadence label is
//! worse than a raw cron string — it tells an operator the task fires at a
//! time it does not. Nothing here panics, whatever the input.

/// Fields of a cron expression, in the 6-field order the scheduler uses.
struct Fields<'a> {
    second: &'a str,
    minute: &'a str,
    hour: &'a str,
    day_of_month: &'a str,
    month: &'a str,
    day_of_week: &'a str,
}

/// What the minute/hour fields describe.
enum Cadence {
    /// A repeating period: `every 6h`, `hourly at :30`. Carries no clock time,
    /// so a day qualifier attaches as a suffix (`every 6h on Mon`).
    Interval(String),
    /// One or more wall-clock times: `07:00`. A day qualifier prefixes it
    /// (`daily 07:00`, `Mon 09:00`).
    Clock(String),
}

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Render a 6-field cron expression as a short cadence label.
///
/// Returns the input unchanged (trimmed) for anything this module cannot
/// express precisely — including 5-field cron, sub-minute schedules, and
/// garbage.
///
/// ```text
/// humanize("0 0 7 * * *")        == "daily 07:00"
/// humanize("0 10,30,50 * * * *") == "every 20m at :10,:30,:50"
/// humanize("0 0 */6 * * *")      == "every 6h"
/// humanize("nonsense")           == "nonsense"
/// ```
#[must_use]
pub fn humanize(cron: &str) -> String {
    describe(cron).unwrap_or_else(|| cron.trim().to_string())
}

/// The whole translation, as a partial function: `None` means "no exact
/// label exists", which the caller turns back into the raw expression.
fn describe(cron: &str) -> Option<String> {
    let fields = split_fields(cron)?;

    // Sub-minute cadence has no short label, and a non-zero second offset
    // would be silently dropped by every label below.
    if fields.second != "0" {
        return None;
    }

    match cadence(fields.minute, fields.hour)? {
        Cadence::Interval(interval) => match interval_suffix(&fields)? {
            Some(days) => Some(format!("{interval} on {days}")),
            None => Some(interval),
        },
        Cadence::Clock(clock) => Some(format!("{} {clock}", clock_prefix(&fields)?)),
    }
}

fn split_fields(cron: &str) -> Option<Fields<'_>> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 6 {
        return None;
    }
    Some(Fields {
        second: fields[0],
        minute: fields[1],
        hour: fields[2],
        day_of_month: fields[3],
        month: fields[4],
        day_of_week: fields[5],
    })
}

/// Minute + hour → the repeating shape they describe.
fn cadence(minute: &str, hour: &str) -> Option<Cadence> {
    if is_any(hour) {
        return hourly_cadence(minute);
    }

    if let Some(step) = parse_step(hour) {
        if step == 0 || step > 23 {
            return None;
        }
        let minutes = parse_values(minute, 0, 59)?;
        let [only] = minutes[..] else { return None };
        let label = if only == 0 {
            format!("every {step}h")
        } else {
            format!("every {step}h at :{only:02}")
        };
        return Some(Cadence::Interval(label));
    }

    let hours = parse_values(hour, 0, 23)?;
    let minutes = parse_values(minute, 0, 59)?;
    // "09:00 and 09:30 and 21:00 and 21:30" is a matrix, not a clock time.
    let [minute] = minutes[..] else { return None };
    let clock = hours
        .iter()
        .map(|h| format!("{h:02}:{minute:02}"))
        .collect::<Vec<_>>()
        .join(",");
    Some(Cadence::Clock(clock))
}

/// Cadence for a wildcard hour: the minute field carries the whole period.
fn hourly_cadence(minute: &str) -> Option<Cadence> {
    if is_any(minute) {
        return Some(Cadence::Interval("every minute".to_string()));
    }

    if let Some(step) = parse_step(minute) {
        if step == 0 || step > 59 {
            return None;
        }
        return Some(Cadence::Interval(format!("every {step}m")));
    }

    let minutes = parse_values(minute, 0, 59)?;
    let at = minutes
        .iter()
        .map(|m| format!(":{m:02}"))
        .collect::<Vec<_>>()
        .join(",");

    // Evenly spaced minutes are a period, and reading one is what an operator
    // actually wants: `0 10,30,50 * * * *` is a 20-minute loop.
    match even_spacing(&minutes) {
        Some(gap) => Some(Cadence::Interval(format!("every {gap}m at {at}"))),
        None => Some(Cadence::Interval(format!("hourly at {at}"))),
    }
}

/// The day qualifier for a clock time: `daily`, `Mon`, `day 1`, `Mar 1`.
fn clock_prefix(fields: &Fields<'_>) -> Option<String> {
    let dom = is_any(fields.day_of_month);
    let month = is_any(fields.month);
    let dow = is_any(fields.day_of_week);

    match (dom, month, dow) {
        (true, true, true) => Some("daily".to_string()),
        (true, true, false) => day_of_week_label(fields.day_of_week),
        (false, true, true) => Some(format!(
            "day {}",
            numeric_label(fields.day_of_month, 1, 31)?
        )),
        (false, false, true) => Some(format!(
            "{} {}",
            month_label(fields.month)?,
            numeric_label(fields.day_of_month, 1, 31)?
        )),
        // Cron ORs day-of-month against day-of-week when both are restricted;
        // no short label states that without lying.
        _ => None,
    }
}

/// The day qualifier for an interval: `None` = no qualifier needed, the outer
/// `None` = the interval cannot be qualified honestly.
fn interval_suffix(fields: &Fields<'_>) -> Option<Option<String>> {
    if !is_any(fields.day_of_month) || !is_any(fields.month) {
        return None;
    }
    if is_any(fields.day_of_week) {
        return Some(None);
    }
    day_of_week_label(fields.day_of_week).map(Some)
}

/// `*` and `?` both mean "unrestricted" in the expressions operators write.
fn is_any(field: &str) -> bool {
    field == "*" || field == "?"
}

/// `*/n` → `n`.
fn parse_step(field: &str) -> Option<u32> {
    field.strip_prefix("*/")?.parse().ok()
}

/// A comma-separated list of in-range numbers, sorted and deduplicated.
/// Ranges, steps and names are rejected — the caller falls back to raw cron.
fn parse_values(field: &str, min: u32, max: u32) -> Option<Vec<u32>> {
    let mut values = Vec::new();
    for part in field.split(',') {
        let value: u32 = part.trim().parse().ok()?;
        if value < min || value > max {
            return None;
        }
        values.push(value);
    }
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    values.dedup();
    Some(values)
}

/// A day-of-month (or similar) field rendered verbatim: `1`, `1,15`.
fn numeric_label(field: &str, min: u32, max: u32) -> Option<String> {
    let values = parse_values(field, min, max)?;
    Some(
        values
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// The gap between evenly spaced minutes, wrap-around included, or `None`.
///
/// `[10, 30, 50]` is an even 20-minute period because 50 → 10 is also 20.
/// `[5, 10]` is not: it fires twice then waits 55 minutes.
fn even_spacing(minutes: &[u32]) -> Option<u32> {
    if minutes.len() < 2 {
        return None;
    }
    let gap = minutes[1] - minutes[0];
    if gap == 0 {
        return None;
    }
    let even = minutes.windows(2).all(|w| w[1] - w[0] == gap);
    let wraps = 60 - minutes[minutes.len() - 1] + minutes[0] == gap;
    (even && wraps).then_some(gap)
}

/// `1` → `Mon`, `1,3` → `Mon,Wed`, `1-5` → `Mon-Fri`.
fn day_of_week_label(field: &str) -> Option<String> {
    if let Some((start, end)) = field.split_once('-') {
        return Some(format!("{}-{}", day_name(start)?, day_name(end)?));
    }
    let names: Option<Vec<&str>> = field.split(',').map(day_name).collect();
    Some(names?.join(","))
}

/// `0` and `7` are both Sunday; three-letter names are accepted too.
fn day_name(token: &str) -> Option<&'static str> {
    let token = token.trim();
    if let Ok(number) = token.parse::<u32>() {
        return match number {
            0 | 7 => Some("Sun"),
            1..=6 => Some(DAY_NAMES[number as usize]),
            _ => None,
        };
    }
    DAY_NAMES
        .iter()
        .copied()
        .find(|name| name.eq_ignore_ascii_case(token))
}

/// `3` → `Mar`, `3,6` → `Mar,Jun`.
fn month_label(field: &str) -> Option<String> {
    let names: Option<Vec<&str>> = field.split(',').map(month_name).collect();
    Some(names?.join(","))
}

fn month_name(token: &str) -> Option<&'static str> {
    let token = token.trim();
    if let Ok(number) = token.parse::<u32>() {
        return match number {
            1..=12 => Some(MONTH_NAMES[number as usize - 1]),
            _ => None,
        };
    }
    MONTH_NAMES
        .iter()
        .copied()
        .find(|name| name.eq_ignore_ascii_case(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seven expressions actually present in a live `schedule.json`.
    #[test]
    fn live_schedule_forms() {
        assert_eq!(humanize("0 10,30,50 * * * *"), "every 20m at :10,:30,:50");
        assert_eq!(humanize("0 0 8 * * *"), "daily 08:00");
        assert_eq!(humanize("0 0 10 * * *"), "daily 10:00");
        assert_eq!(humanize("0 0 12 * * *"), "daily 12:00");
        assert_eq!(humanize("0 0 22 * * *"), "daily 22:00");
        assert_eq!(humanize("0 30 23 * * *"), "daily 23:30");
        assert_eq!(humanize("0 0 11 * * 7"), "Sun 11:00");
    }

    #[test]
    fn daily_clock_times() {
        assert_eq!(humanize("0 0 7 * * *"), "daily 07:00");
        assert_eq!(humanize("0 5 0 * * *"), "daily 00:05");
        assert_eq!(humanize("0 0 8,20 * * *"), "daily 08:00,20:00");
    }

    #[test]
    fn hour_steps() {
        assert_eq!(humanize("0 0 */6 * * *"), "every 6h");
        assert_eq!(humanize("0 15 */2 * * *"), "every 2h at :15");
    }

    #[test]
    fn minute_cadences() {
        assert_eq!(humanize("0 30 * * * *"), "hourly at :30");
        assert_eq!(humanize("0 0 * * * *"), "hourly at :00");
        assert_eq!(humanize("0 */20 * * * *"), "every 20m");
        assert_eq!(humanize("0 * * * * *"), "every minute");
    }

    /// An evenly spaced minute list is a period; an uneven one is not, and
    /// must not be sold as one.
    #[test]
    fn minute_lists_report_a_period_only_when_evenly_spaced() {
        assert_eq!(
            humanize("0 0,15,30,45 * * * *"),
            "every 15m at :00,:15,:30,:45"
        );
        assert_eq!(humanize("0 5,10 * * * *"), "hourly at :05,:10");
        assert_eq!(humanize("0 0,1,2 * * * *"), "hourly at :00,:01,:02");
    }

    #[test]
    fn weekdays() {
        assert_eq!(humanize("0 0 9 * * 1"), "Mon 09:00");
        assert_eq!(humanize("0 0 9 * * 0"), "Sun 09:00");
        assert_eq!(humanize("0 0 9 * * 7"), "Sun 09:00");
        assert_eq!(humanize("0 0 9 * * 1,3,5"), "Mon,Wed,Fri 09:00");
        assert_eq!(humanize("0 0 9 * * 1-5"), "Mon-Fri 09:00");
        assert_eq!(humanize("0 0 9 * * MON"), "Mon 09:00");
    }

    #[test]
    fn day_of_month_and_month_are_included() {
        assert_eq!(humanize("0 0 9 1 * *"), "day 1 09:00");
        assert_eq!(humanize("0 0 9 1,15 * *"), "day 1,15 09:00");
        assert_eq!(humanize("0 0 9 1 3 *"), "Mar 1 09:00");
        assert_eq!(humanize("0 0 9 1 JAN *"), "Jan 1 09:00");
    }

    #[test]
    fn intervals_can_carry_a_weekday() {
        assert_eq!(humanize("0 0 */6 * * 1"), "every 6h on Mon");
        assert_eq!(humanize("0 30 * * * 1-5"), "hourly at :30 on Mon-Fri");
    }

    /// Everything inexpressible comes back untouched rather than approximated.
    #[test]
    fn inexpressible_expressions_return_the_raw_cron() {
        // Day-of-month AND day-of-week restricted: cron ORs them.
        assert_eq!(humanize("0 0 9 1 * 1"), "0 0 9 1 * 1");
        // Sub-minute / second offsets.
        assert_eq!(humanize("30 0 9 * * *"), "30 0 9 * * *");
        assert_eq!(humanize("*/5 * * * * *"), "*/5 * * * * *");
        // Minute matrix under a fixed hour.
        assert_eq!(humanize("0 0,30 9 * * *"), "0 0,30 9 * * *");
        // Ranges and steps outside the supported positions.
        assert_eq!(humanize("0 0 9-17 * * *"), "0 0 9-17 * * *");
        // Standard 5-field cron is not what the scheduler parses.
        assert_eq!(humanize("0 7 * * *"), "0 7 * * *");
    }

    #[test]
    fn garbage_never_panics() {
        for input in [
            "",
            "   ",
            "nonsense",
            "0 0 0 0 0 0",
            "* * * * * * *",
            "0 99 99 99 99 99",
            "0 -1 -1 * * *",
            "0 */0 * * * *",
            "0 0 */0 * * *",
            "0 , , * * *",
            "0 0 9 * * 9",
            "0 0 9 * 13 *",
            "0 0 9 32 * *",
            "0 ␀ * * * *",
            "0\t0\t7\t*\t*\t*",
        ] {
            let out = humanize(input);
            assert!(
                !out.is_empty() || input.trim().is_empty(),
                "input: {input:?}"
            );
        }
        // Out-of-range values are returned raw, not clamped.
        assert_eq!(humanize("0 99 99 99 99 99"), "0 99 99 99 99 99");
        // Tabs are whitespace, so this one is a real expression.
        assert_eq!(humanize("0\t0\t7\t*\t*\t*"), "daily 07:00");
        // Empty input round-trips to empty.
        assert_eq!(humanize("   "), "");
    }
}
