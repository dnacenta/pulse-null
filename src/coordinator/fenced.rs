//! Fencing enforcement at the protected resource.
//!
//! Leases bound intent; this guard enforces correctness. A holder that was
//! paused past lease expiry can still present a formerly-valid token — the
//! resource, not the lease table, is the last line of defense: any write
//! bearing a token below the highest it has seen is rejected.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::lease::FencingToken;

/// A write was fenced: the presented token is stale.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("write fenced: presented token {presented:?} < high-water {high_water:?}")]
pub struct Rejected {
    pub presented: FencingToken,
    pub high_water: FencingToken,
}

/// Per-resource fencing guard. One instance protects one resource; it tracks
/// the highest token it has ever accepted, independent of the lease table
/// (which may be down, restarted, or lagging — decision 3 in the spec).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FencedResource {
    high_water: Option<FencingToken>,
}

impl FencedResource {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a guard at a known watermark (e.g. after WAL recovery).
    pub fn at(high_water: FencingToken) -> Self {
        Self {
            high_water: Some(high_water),
        }
    }

    /// Admit or fence a write. Tokens equal to the high-water are admitted —
    /// the same ownership epoch writes many times. Strictly lower is fenced.
    pub fn accept(&mut self, token: FencingToken) -> Result<(), Rejected> {
        match self.high_water {
            Some(high) if token < high => Err(Rejected {
                presented: token,
                high_water: high,
            }),
            _ => {
                self.high_water = Some(token);
                Ok(())
            }
        }
    }

    pub fn high_water(&self) -> Option<FencingToken> {
        self.high_water
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_write_is_admitted_and_recorded() {
        let mut guard = FencedResource::new();
        assert!(guard.accept(FencingToken(1)).is_ok());
        assert_eq!(guard.high_water(), Some(FencingToken(1)));
    }

    #[test]
    fn same_epoch_writes_repeatedly() {
        let mut guard = FencedResource::new();
        guard.accept(FencingToken(3)).unwrap();
        assert!(guard.accept(FencingToken(3)).is_ok());
    }

    #[test]
    fn stale_token_is_fenced() {
        let mut guard = FencedResource::new();
        guard.accept(FencingToken(2)).unwrap();
        let err = guard.accept(FencingToken(1)).unwrap_err();
        assert_eq!(
            err,
            Rejected {
                presented: FencingToken(1),
                high_water: FencingToken(2),
            }
        );
        assert_eq!(guard.high_water(), Some(FencingToken(2)));
    }

    #[test]
    fn paused_holder_wakes_stale_and_is_rejected() {
        // The race from spec decision 3, end to end at the resource:
        // A holds token 1, pauses past expiry; B reclaims with token 2 and
        // writes; A wakes and writes with 1 — fenced. B keeps writing fine.
        let mut guard = FencedResource::new();
        let a = FencingToken(1);
        let b = FencingToken(2);
        guard.accept(b).unwrap();
        assert!(guard.accept(a).is_err());
        assert!(guard.accept(b).is_ok());
    }

    #[test]
    fn recovery_constructor_fences_below_watermark() {
        let mut guard = FencedResource::at(FencingToken(7));
        assert!(guard.accept(FencingToken(6)).is_err());
        assert!(guard.accept(FencingToken(7)).is_ok());
        assert!(guard.accept(FencingToken(8)).is_ok());
    }
}
