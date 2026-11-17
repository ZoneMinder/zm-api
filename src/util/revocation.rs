//! In-memory token revocation registry.
//!
//! JWTs are stateless, so logout / password change / user disable need a
//! server-side floor on token issue time to have any effect. ZoneMinder
//! models this as `Users.TokenMinExpiry`: tokens issued before that instant
//! are dead. This registry is the hot-path copy of that column — consulted by
//! [`crate::util::authz::enforce`] on every protected request without a
//! database round-trip.
//!
//! Durability contract:
//! - Revocations are written to `Users.TokenMinExpiry` *and* recorded here.
//! - The registry is hydrated from the DB at startup ([`AppState::new`]), so
//!   revocations survive restarts.
//! - Refresh tokens are additionally checked against the DB row inside
//!   [`crate::service::auth::refresh_token`], which loads the user anyway.
//!
//! [`AppState::new`]: crate::server::state::AppState::new

use std::collections::HashMap;
use std::sync::RwLock;

/// Per-user floor on JWT `iat` (unix seconds). Tokens issued strictly before
/// the recorded instant are rejected.
#[derive(Debug, Default)]
pub struct TokenRevocations {
    min_iat: RwLock<HashMap<u32, i64>>,
}

impl TokenRevocations {
    /// Record that tokens for `uid` issued before `min_iat` are invalid.
    /// A later floor always wins; an earlier one never lowers the bar.
    pub fn revoke(&self, uid: u32, min_iat: i64) {
        let mut map = self.min_iat.write().expect("revocation lock poisoned");
        let entry = map.entry(uid).or_insert(min_iat);
        if *entry < min_iat {
            *entry = min_iat;
        }
    }

    /// The user's floor, if they have one.
    pub fn floor(&self, uid: u32) -> Option<i64> {
        self.min_iat
            .read()
            .expect("revocation lock poisoned")
            .get(&uid)
            .copied()
    }

    /// True when a token issued at `iat` for `uid` has been revoked.
    ///
    /// Strictly-before semantics: a token minted in the same second as the
    /// revocation stays valid, so a logout immediately followed by a fresh
    /// login never invalidates the new session.
    pub fn is_revoked(&self, uid: u32, iat: i64) -> bool {
        self.min_iat
            .read()
            .expect("revocation lock poisoned")
            .get(&uid)
            .is_some_and(|min| iat < *min)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_user_is_not_revoked() {
        let r = TokenRevocations::default();
        assert!(!r.is_revoked(1, 1_000));
    }

    #[test]
    fn tokens_issued_before_the_floor_are_revoked() {
        let r = TokenRevocations::default();
        r.revoke(1, 1_000);
        assert!(r.is_revoked(1, 999));
        // Same-second and later tokens survive.
        assert!(!r.is_revoked(1, 1_000));
        assert!(!r.is_revoked(1, 1_001));
        // Other users are untouched.
        assert!(!r.is_revoked(2, 999));
    }

    #[test]
    fn later_floor_wins_and_earlier_floor_never_lowers_it() {
        let r = TokenRevocations::default();
        r.revoke(1, 2_000);
        r.revoke(1, 1_000); // stale write (e.g. startup hydration after a logout)
        assert!(r.is_revoked(1, 1_500));
        r.revoke(1, 3_000);
        assert!(r.is_revoked(1, 2_500));
    }
}
