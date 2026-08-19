//! `pulse-null outreach` — the channel's instrument panel (PN-94).
//!
//! Two things live here, and both exist because the outreach channel is
//! self-triggered and therefore cannot be trusted to report on itself:
//!
//! * `status` publishes the caps, the response rates that move them, and the
//!   rejections. A gate that never rejects is not a gate, and a rejection log
//!   nobody can read cannot tell anyone whether the gate is calibrated
//!   (spec §7.2, §8).
//! * `respond` records D's reaction, which is the only scoring signal in the
//!   system that the entity did not author (spec §2.4). Without a way to
//!   record it every cap tightens to half and stays there.

use chrono::Utc;
use console::style;

use crate::config::Config;
use crate::events::SalienceKind;
use crate::outreach::feedback::{self, KindStatus};
use crate::outreach::store::{self, Rating};

/// How many recent rejections `status` prints.
const RECENT_REJECTIONS: usize = 10;

/// How many recent messages `status` prints.
const RECENT_SENT: usize = 5;

/// Show caps, response rates, and recent rejections.
pub async fn status() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    let store = store::load(&root_dir);
    let now = Utc::now();
    let outreach = &config.outreach;

    println!();
    println!("  {}", style("Interest-Triggered Outreach").bold());
    println!();

    let enabled = if outreach.enabled {
        style("enabled").green()
    } else {
        style("disabled").red()
    };
    println!("  Channel: {} ({})", outreach.channel, enabled);

    let tz = crate::outreach::resolve_timezone(&config.scheduler.timezone);
    let local = now.with_timezone(&tz);
    let quiet_now = crate::outreach::is_quiet_hour(
        chrono::Timelike::hour(&local),
        outreach.quiet_hours_start,
        outreach.quiet_hours_end,
    );
    println!(
        "  Quiet hours: {:02}:00–{:02}:00 {} — {} now ({})",
        outreach.quiet_hours_start,
        outreach.quiet_hours_end,
        config.scheduler.timezone,
        if quiet_now { "quiet" } else { "open" },
        local.format("%H:%M"),
    );
    println!(
        "  Call routing: {}",
        if outreach.allow_call_routing {
            style("on").yellow()
        } else {
            style("off (manual only)").dim()
        }
    );
    println!();

    println!("  {}", style("Budgets").bold());
    for kind in SalienceKind::ALL {
        print_kind(&feedback::kind_status(
            &store,
            outreach,
            kind,
            &config.scheduler.timezone,
            now,
        ));
    }
    println!();

    print_recent_sent(&store);
    print_rejections(&store);

    Ok(())
}

/// One budget line per kind: what it may send, what it has sent, and what
/// D's behaviour is doing to the cap.
fn print_kind(status: &KindStatus) {
    let cap = match (status.base_cap, status.effective_cap) {
        (None, _) => style("uncapped".to_string()).cyan(),
        (Some(base), Some(effective)) if effective < base => {
            style(format!("{effective}/day (halved from {base})")).yellow()
        }
        (Some(base), _) => style(format!("{base}/day")).white(),
    };

    let rate = match status.response_rate {
        Some(rate) => format!("{:.0}% over {}", rate * 100.0, status.window_size),
        None => format!(
            "n/a ({}/{} messages — window not full)",
            status.window_size,
            status.window_size.max(1)
        ),
    };

    println!(
        "  {:<12} {} · sent today {} · response {}",
        style(status.kind.as_str()).cyan().bold(),
        cap,
        status.sent_today,
        rate,
    );

    if status.tightened && !status.announced {
        println!(
            "    {}",
            style("tightening not yet announced to D — notice will fire on the next candidate")
                .yellow()
        );
    }
}

/// Recent messages with their response state, so "latency to response"
/// (spec §2.4) is inspectable and not merely stored.
fn print_recent_sent(store: &store::OutreachStore) {
    println!("  {}", style("Recent messages").bold());
    if store.sent.is_empty() {
        println!("    none");
        println!();
        return;
    }

    for message in store.sent.iter().rev().take(RECENT_SENT) {
        let response = match (message.response_latency(), message.rating) {
            (Some(latency), Some(rating)) => {
                style(format!("{} after {}", rating, humanize(latency))).green()
            }
            (Some(latency), None) => {
                style(format!("responded after {}", humanize(latency))).green()
            }
            (None, _) => style("no response".to_string()).dim(),
        };
        println!(
            "    [{}] {} — {}",
            style(message.kind.as_str()).cyan(),
            message.headline,
            response
        );
        println!(
            "      {} · asked for {} · {}",
            style(&message.id).dim(),
            message.cost,
            style(message.sent_at.format("%Y-%m-%d %H:%M UTC").to_string()).dim(),
        );
    }
    println!();
}

/// The rejection log — the only evidence about whether the gate is calibrated.
fn print_rejections(store: &store::OutreachStore) {
    println!("  {}", style("Recent rejections").bold());
    if store.rejections.is_empty() {
        println!(
            "    {}",
            style("none — a gate that has never fired has also never been wrong").dim()
        );
        println!();
        return;
    }

    for rejection in store.rejections.iter().rev().take(RECENT_REJECTIONS) {
        println!(
            "    [{}] {} — {}",
            style(rejection.kind.as_str()).cyan(),
            rejection.headline,
            style(rejection.reason.to_string()).yellow(),
        );
        println!(
            "      {}",
            style(
                rejection
                    .rejected_at
                    .format("%Y-%m-%d %H:%M UTC")
                    .to_string()
            )
            .dim()
        );
    }
    println!("    {} rejection(s) on record", store.rejections.len());
    println!();
}

/// Record that D responded to an outreach message.
///
/// `rating` is the optional `/useful` or `/noise` from spec §2.4. Passing no
/// rating still counts as a response — a reply is a reply, and requiring D to
/// classify it would make the cheap signal expensive.
pub async fn respond(id: String, rating: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    let rating = match rating.as_deref() {
        None => None,
        Some(label) => Some(
            Rating::from_label(label)
                .ok_or_else(|| format!("Unknown rating '{label}' (useful|noise)"))?,
        ),
    };

    let recorded = feedback::record_response(&root_dir, &id, rating, Utc::now())
        .map_err(|e| format!("could not record the response: {e}"))?;

    if recorded {
        println!("  Response recorded for {}.", style(&id).cyan());
    } else {
        println!("  No outreach message with id {}.", style(&id).red());
        let store = store::load(&root_dir);
        let unanswered = store.unanswered();
        if !unanswered.is_empty() {
            println!("  Awaiting a response:");
            for message in unanswered.iter().take(RECENT_SENT) {
                println!("    {} — {}", style(&message.id).dim(), message.headline);
            }
        }
    }

    Ok(())
}

/// Coarse, readable duration — the precision that matters here is "minutes
/// or days", not seconds.
fn humanize(duration: chrono::Duration) -> String {
    let minutes = duration.num_minutes();
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 24 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / (60 * 24))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_picks_a_readable_unit() {
        assert_eq!(humanize(chrono::Duration::minutes(7)), "7m");
        assert_eq!(humanize(chrono::Duration::minutes(59)), "59m");
        assert_eq!(humanize(chrono::Duration::minutes(60)), "1h");
        assert_eq!(humanize(chrono::Duration::hours(23)), "23h");
        assert_eq!(humanize(chrono::Duration::hours(25)), "1d");
    }
}
