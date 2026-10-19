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
//!   no scope           None        None
//! ```
//!
//! [`ToolSurface::Restricted`] is the isolation surface (a read-only
//! allowlist); [`ToolSurface::None`] gives the CLI no tools at all. The CLI
//! provider reads the scope when it builds each invocation and narrows it
//! by the isolation marker ([`effective`]).
//!
//! The value is task-local, so concurrent turns never see each other's
//! surface. It fails closed (audit R2-11): a provider call outside any scope
//! gets no tools. Work the owner configured — scheduled tasks (the thinking
//! loop among them), intents, farms — asks for [`ToolSurface::Full`]
//! explicitly with [`owner_work`]; graph extraction spawns its CLI through
//! recall-echo, not through this provider, and is not affected.

use crate::session_key::SessionKey;

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

impl ToolSurface {
    /// The surface for a turn whose resolved session key is `resolved_key`:
    /// full for the owner, none for anyone else.
    #[must_use]
    pub fn for_principal(resolved_key: &SessionKey) -> Self {
        if resolved_key.is_owner() {
            Self::Full
        } else {
            Self::None
        }
    }
}

/// The surface one invocation runs with: the turn's (`turn`, or none
/// outside any scope), narrowed to at most [`ToolSurface::Restricted`] while
/// the pulse is isolated. Isolation never widens a surface: an isolated
/// guest still gets none.
#[must_use]
pub fn effective(turn: Option<ToolSurface>, isolated: bool) -> ToolSurface {
    let surface = turn.unwrap_or(ToolSurface::None);
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

/// Run `fut` — work the owner configured, not anyone's message — on the
/// full surface. Isolation still narrows it per invocation.
///
/// The provider call must happen inside `fut`: a CLI provider reads the
/// surface when `invoke` is called, so `owner_work(provider.invoke(..))`
/// would read it outside the scope. Pass an `async fn`'s future or an
/// `async` block.
pub async fn owner_work<F, R>(fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    scope(ToolSurface::Full, fut).await
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
        assert_eq!(
            ToolSurface::for_principal(&SessionKey::owner()),
            ToolSurface::Full
        );
        for key in [
            SessionKey::guest("2002"),
            SessionKey::guest("phone-caller-0123456789abcdef"),
            SessionKey::guest("owner"),
            SessionKey::peer("Synth"),
        ] {
            assert_eq!(ToolSurface::for_principal(&key), ToolSurface::None, "{key}");
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
            (Option::None, false, NoTools),
            (Option::None, true, NoTools),
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
