//! Which tool surface a turn's agent CLI gets, decided by who the turn is
//! for (PN-110 audit P1).
//!
//! An agent CLI brings its own tools and runs with its permission prompts
//! off, so a guest's words reaching it with the full surface are one prompt
//! injection away from a shell. The turn pipeline resolves the principal
//! first, then runs the rest of the turn inside [`scope`]: only the owner
//! keeps the full surface, every other principal (`guest:*`, `peer:*`) gets
//! the restricted one. The CLI provider reads it when it builds each
//! invocation, alongside the isolation marker — isolation still restricts
//! everyone.
//!
//! The value is task-local, so concurrent turns never see each other's
//! surface. Work outside any turn (scheduler, intents, thinking) carries no
//! scope and is governed by isolation alone, as before.

/// The tools an agent CLI may use for one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSurface {
    /// Everything the CLI offers: the owner's own turns.
    Full,
    /// The CLI's read-only surface (the isolation-mode one).
    Restricted,
}

/// The session key `resolve_sender` gives the owner.
const OWNER_KEY: &str = "owner";

impl ToolSurface {
    /// The surface for a turn whose resolved session key is `resolved_key`:
    /// full for the owner, restricted for anyone else.
    #[must_use]
    pub fn for_principal(resolved_key: &str) -> Self {
        if resolved_key == OWNER_KEY {
            Self::Full
        } else {
            Self::Restricted
        }
    }
}

tokio::task_local! {
    static TOOL_SURFACE: ToolSurface;
}

/// Run `fut` with `surface` as the tool surface of every CLI invocation it
/// makes on this task.
pub async fn scope<F, R>(surface: ToolSurface, fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    TOOL_SURFACE.scope(surface, fut).await
}

/// The surface the current turn asked for, or `None` outside any turn.
#[must_use]
pub fn current() -> Option<ToolSurface> {
    TOOL_SURFACE.try_with(|surface| *surface).ok()
}

/// Whether the current turn must run on the restricted surface.
#[must_use]
pub fn current_is_restricted() -> bool {
    current() == Some(ToolSurface::Restricted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_owner_keeps_the_full_surface() {
        assert_eq!(ToolSurface::for_principal("owner"), ToolSurface::Full);
        for key in [
            "guest:2002",
            "guest:phone-caller",
            "peer:Synth",
            "",
            "Owner",
        ] {
            assert_eq!(
                ToolSurface::for_principal(key),
                ToolSurface::Restricted,
                "{key:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_surface_is_scoped_to_the_turn() {
        assert_eq!(current(), None);
        assert!(!current_is_restricted());
        scope(ToolSurface::Restricted, async {
            assert!(current_is_restricted());
            scope(ToolSurface::Full, async {
                assert_eq!(current(), Some(ToolSurface::Full));
            })
            .await;
            assert!(current_is_restricted());
        })
        .await;
        assert_eq!(current(), None);
    }
}
