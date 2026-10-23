//! Probes (PN-144): measurements of a pulse that the runtime runs and
//! scores, outside the pulse.
//!
//! A probe item asks the pulse something in a scratch copy of itself, under
//! a Landlock sandbox that lets it read only system files and that copy and
//! write only inside it, and the
//! runtime scores the answer by rule. Results are held out from the pulse
//! (mobile-app spec §1a.1 G2; Concept v2 C6). This module holds the scratch
//! copies ([`scratch`]), the read fence ([`fence`]), the per-run graph on a
//! throwaway server ([`graph`]), the sealed item sets ([`sealed`]) and the
//! isolation verifier ([`verify`]).

pub mod confine;
pub mod facts;
pub mod fence;
pub mod graph;
pub mod observe;
pub mod owner;
pub mod paired;
pub mod record;
pub mod run;
pub mod schedule;
pub mod score;
pub mod scratch;
pub mod sealed;
pub mod sets;
pub mod summary;
pub mod truth;
pub mod verify;

/// Which baseline a run belongs to, derived from what the prompt builder
/// renders, never set by hand: `phase-1` once it renders the runtime's
/// situation block (Concept v2 C1), `pre-phase-1` before (Concept v2 §4
/// Phase 0), so before and after are never pooled (G6). The prompt is built
/// for an empty pulse home and without the operator's rules, so only the
/// runtime's own output decides: text a pulse (or anyone) wrote into its
/// files cannot move the label.
/// One probe turn: `provider`'s captured run on the owner's tool surface
/// ([`crate::tool_surface::owner_work`]). A probe measures the pulse as its
/// owner meets it, and it runs outside any chat turn, where an invocation
/// would get no tools at all (PN-110 fails closed). Isolation still narrows
/// the surface, read in the item's scratch copy, which has no marker; the
/// gate refuses a run while the live pulse is isolated.
pub(crate) async fn probe_turn(
    provider: &crate::cli_provider::CliProvider,
    system_prompt: &str,
    messages: &[pulse_system_types::llm::Message],
) -> Result<crate::cli_provider::CapturedRun, Box<dyn std::error::Error + Send + Sync>> {
    crate::tool_surface::owner_work(async {
        provider.invoke_captured(system_prompt, messages).await
    })
    .await
}

pub fn baseline(config: &crate::config::Config) -> Result<&'static str, String> {
    use std::os::unix::fs::DirBuilderExt;
    let empty = std::env::temp_dir().join(format!(
        "pulse-null-baseline-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&empty)
        .map_err(|e| format!("baseline: {e}"))?;
    let mut runtime_only = config.clone();
    runtime_only.pulse.rules_dir = None;
    let prompt = crate::server::prompt::build_system_prompt(&empty, &runtime_only);
    let _ = std::fs::remove_dir_all(&empty);
    let prompt = prompt.map_err(|e| format!("baseline: the prompt builder failed: {e}"))?;
    Ok(if prompt.contains(crate::server::prompt::SITUATION_TAG) {
        "phase-1"
    } else {
        "pre-phase-1"
    })
}

#[cfg(test)]
mod tests {
    /// The label follows what the runtime renders: today's builder renders
    /// no situation block, and a pulse writing the tag into its own files
    /// changes nothing.
    #[test]
    fn the_baseline_follows_the_runtime_prompt_only() {
        let config = super::truth::tests::fixture_config();
        assert_eq!(super::baseline(&config).unwrap(), "pre-phase-1");
        let pulse = super::scratch::tests::fixture_pulse();
        std::fs::write(
            pulse.path().join("SELF.md"),
            format!("{} forged", crate::server::prompt::SITUATION_TAG),
        )
        .unwrap();
        let prompt = crate::server::prompt::build_system_prompt(pulse.path(), &config).unwrap();
        assert!(
            prompt.contains(crate::server::prompt::SITUATION_TAG),
            "the forged text is in the pulse's prompt"
        );
        assert_eq!(
            super::baseline(&config).unwrap(),
            "pre-phase-1",
            "and does not move the label"
        );
    }
}
