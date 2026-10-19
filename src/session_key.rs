//! The resolved identity a turn runs as, and the session it lands in.
//!
//! `resolve_sender` maps a channel and sender to one of three identity
//! classes, written as the session store's key: `owner`, `peer:<name>` or
//! `guest:<sender>`. Every trust decision downstream (tool surface, refusal
//! fallback, isolation commands, prompt guards, session limits) asks this
//! type, never the raw string (PN-110 audit R2-11).

use std::fmt;

const OWNER: &str = "owner";
const PEER_PREFIX: &str = "peer:";
const GUEST_PREFIX: &str = "guest:";

/// Who a resolved session key says is speaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal<'a> {
    /// The pulse's owner.
    Owner,
    /// A sibling pulse, by name.
    Peer(&'a str),
    /// Anyone else, by the sender the transport reported.
    Guest(&'a str),
}

/// A resolved session key: `owner`, `peer:<name>` or `guest:<sender>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey(String);

impl SessionKey {
    /// The owner's key.
    #[must_use]
    pub fn owner() -> Self {
        Self(OWNER.to_string())
    }

    /// A sibling pulse's key.
    #[must_use]
    pub fn peer(name: &str) -> Self {
        Self(format!("{PEER_PREFIX}{name}"))
    }

    /// A guest's key.
    #[must_use]
    pub fn guest(sender: &str) -> Self {
        Self(format!("{GUEST_PREFIX}{sender}"))
    }

    /// The key as the session store writes it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Who this key says is speaking. A key in none of the three shapes
    /// (an old-format key, say) is a guest: unknown is never trusted.
    #[must_use]
    pub fn principal(&self) -> Principal<'_> {
        if self.0 == OWNER {
            Principal::Owner
        } else if let Some(name) = self.0.strip_prefix(PEER_PREFIX) {
            Principal::Peer(name)
        } else {
            Principal::Guest(self.0.strip_prefix(GUEST_PREFIX).unwrap_or(&self.0))
        }
    }

    #[must_use]
    pub fn is_owner(&self) -> bool {
        self.principal() == Principal::Owner
    }

    #[must_use]
    pub fn is_peer(&self) -> bool {
        matches!(self.principal(), Principal::Peer(_))
    }

    #[must_use]
    pub fn is_guest(&self) -> bool {
        matches!(self.principal(), Principal::Guest(_))
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SessionKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<SessionKey> for String {
    fn from(key: SessionKey) -> Self {
        key.0
    }
}

impl PartialEq<str> for SessionKey {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for SessionKey {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_shape_has_its_principal() {
        assert_eq!(SessionKey::owner().principal(), Principal::Owner);
        assert_eq!(
            SessionKey::peer("Synth").principal(),
            Principal::Peer("Synth")
        );
        assert_eq!(
            SessionKey::guest("2002").principal(),
            Principal::Guest("2002")
        );
        assert!(SessionKey::owner().is_owner());
        assert!(SessionKey::peer("Synth").is_peer());
        assert!(SessionKey::guest("2002").is_guest());
    }

    #[test]
    fn keys_are_written_as_before() {
        assert_eq!(SessionKey::owner(), "owner");
        assert_eq!(SessionKey::peer("Synth"), "peer:Synth");
        assert_eq!(SessionKey::guest("2002").as_str(), "guest:2002");
        assert_eq!(SessionKey::guest("x").to_string(), "guest:x");
    }

    #[test]
    fn a_guest_named_owner_is_still_a_guest() {
        let key = SessionKey::guest("owner");
        assert_eq!(key, "guest:owner");
        assert!(!key.is_owner());
        assert!(SessionKey::peer("owner").is_peer());
    }
}
