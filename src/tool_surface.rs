//! Which tool surface a turn's agent CLI gets, decided by who the turn is
//! for (PN-110 audit P1, R2-1, R2-2).
//!
//! An agent CLI brings its own tools and runs with its permission prompts
//! off, so a guest's words reaching it with tools are one prompt injection
//! away from a shell — or, with only read tools, from reading any file the
//! pulse's user can read and saying it in the reply. The turn pipeline
//! resolves the principal first, then runs the rest of the turn inside
//! [`scope`]:
//!
//! ```text
//!   principal          normal      isolated
//!   owner              Full        Restricted
//!   guest:* / peer:*   None        None
//! ```
//!
//! [`ToolSurface::Restricted`] is the isolation surface (a read-only
//! allowlist); [`ToolSurface::None`] gives the CLI no tools at all. The CLI
//! provider reads the scope when it builds each invocation and narrows it
//! by the isolation marker ([`effective`]).
//!
//! The value is task-local, so concurrent turns never see each other's
//! surface. Work outside any turn (scheduler, intents, thinking) carries no
//! scope and is governed by isolation alone, as before.

/// The tools an agent CLI may use for one invocation, narrowest first (the
/// order is what [`effective`] narrows by).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolSurface {
    /// No tools: guests and peers. Their words never drive a tool.
    None,
    /// The CLI's read-only allowlist: the owner while isolated.
    Restricted,
    /// Everything the CLI offers: the owner's own turns.
    Full,
}

/// The session key `resolve_sender` gives the owner.
const OWNER_KEY: &str = "owner";

impl ToolSurface {
    /// The surface for a turn whose resolved session key is `resolved_key`:
    /// full for the owner, none for anyone else.
    #[must_use]
    pub fn for_principal(resolved_key: &str) -> Self {
        if resolved_key == OWNER_KEY {
            Self::Full
        } else {
            Self::None
        }
    }
}

/// The surface one invocation runs with: the turn's (`turn`, or full
/// outside any turn), narrowed to at most [`ToolSurface::Restricted`] while
/// the pulse is isolated. Isolation never widens a surface: an isolated
/// guest still gets none.
#[must_use]
pub fn effective(turn: Option<ToolSurface>, isolated: bool) -> ToolSurface {
    let surface = turn.unwrap_or(ToolSurface::Full);
    if isolated {
        surface.min(ToolSurface::Restricted)
    } else {
        surface
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_owner_gets_tools() {
        assert_eq!(ToolSurface::for_principal("owner"), ToolSurface::Full);
        for key in [
            "guest:2002",
            "guest:phone-caller-0123456789abcdef",
            "peer:Synth",
            "",
            "Owner",
        ] {
            assert_eq!(
                ToolSurface::for_principal(key),
                ToolSurface::None,
                "{key:?}"
            );
        }
    }

    #[test]
    fn isolation_narrows_to_restricted_and_never_widens() {
        use ToolSurface::{Full, None as NoTools, Restricted};
        let cases = [
            (Some(Full), false, Full),
            (Some(Full), true, Restricted),
            (Some(Restricted), false, Restricted),
            (Some(Restricted), true, Restricted),
            (Some(NoTools), false, NoTools),
            (Some(NoTools), true, NoTools),
            (Option::None, false, Full),
            (Option::None, true, Restricted),
        ];
        for (turn, isolated, expected) in cases {
            assert_eq!(effective(turn, isolated), expected, "{turn:?} {isolated}");
        }
    }

    #[tokio::test]
    async fn the_surface_is_scoped_to_the_turn() {
        assert_eq!(current(), Option::None);
        scope(ToolSurface::None, async {
            assert_eq!(current(), Some(ToolSurface::None));
            scope(ToolSurface::Full, async {
                assert_eq!(current(), Some(ToolSurface::Full));
            })
            .await;
            assert_eq!(current(), Some(ToolSurface::None));
        })
        .await;
        assert_eq!(current(), Option::None);
    }
}
