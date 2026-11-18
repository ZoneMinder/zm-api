//! Signed media URLs: short-lived, resource-scoped credentials for the URIs a
//! playlist lists (#153, #34).
//!
//! A player that can't send an `Authorization` header requests every URI a
//! playlist lists exactly as written, so the credential has to be in the URL.
//! Putting the caller's access token there would spread a credential that can
//! do anything the user can into every segment URL and every log that sees
//! one. Instead each playlist response signs its segment URIs with an HMAC
//! that opens only that monitor's live media or that event's recording, only
//! for reads, and only for a few minutes. Players refetch a live playlist
//! every few seconds, so the signatures they hold are always fresh.
//!
//! The token is one query parameter, `st`:
//!
//! ```text
//! st=1.m.<monitor>.<uid>.<iat>.<exp>.<mac>          live stream of a monitor
//! st=1.e.<event>.<monitor>.<uid>.<iat>.<exp>.<mac>  one event's recording
//! ```
//!
//! `mac` is base64url HMAC-SHA256 over everything before it. The key is
//! derived from the access-token signing key, so every zm-api instance that
//! can verify an access token can verify a signed URL, and rotating the JWT
//! keys invalidates outstanding URLs with them. `uid` and `iat` let the
//! server-side revocation floor (logout, password change) cut them off.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, KeyInit, Mac};
use once_cell::sync::Lazy;
use sha2::Sha256;
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

/// The query parameter a signed URL carries.
pub const PARAM: &str = "st";

/// How long a signed live-segment URI stays valid. A live playlist is
/// refetched every target duration, so this only has to outlast one refresh
/// plus a slow fetch.
pub const LIVE_TTL: Duration = Duration::from_secs(5 * 60);

/// How long a signed recording URI stays valid. A VOD playlist is fetched
/// once and the player then walks its segments, pausing and seeking, so it
/// has to cover a viewing session.
pub const RECORDING_TTL: Duration = Duration::from_secs(4 * 60 * 60);

/// Default lifetime of a link minted for an integration
/// (`POST /api/v3/media-links`), which stores one URL and keeps using it.
pub const LINK_DEFAULT_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The longest a minted link may live. Revoking it early means raising the
/// user's revocation floor, so give each integration its own user.
pub const LINK_MAX_TTL: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// What a signed URL opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaResource {
    /// One monitor's live media.
    Monitor(u32),
    /// One event's recording; `monitor_id` is the event's monitor, for ACLs.
    Event { event_id: u64, monitor_id: u32 },
}

impl MediaResource {
    pub fn monitor_id(&self) -> u32 {
        match *self {
            MediaResource::Monitor(id) => id,
            MediaResource::Event { monitor_id, .. } => monitor_id,
        }
    }
}

/// A verified signed URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedGrant {
    pub resource: MediaResource,
    pub uid: u32,
    pub iat: i64,
    pub exp: i64,
}

static KEY: Lazy<[u8; 32]> = Lazy::new(|| {
    let pem = crate::constant::CONFIG
        .secret
        .read_private_access_key()
        .expect("the access-token signing key is readable");
    derive_key(pem.as_bytes())
});

/// A key for signing media URLs, derived from (and never equal to) the
/// access-token signing key.
fn derive_key(signing_key: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(signing_key).expect("HMAC takes any key length");
    mac.update(b"zm-api signed media url v1");
    mac.finalize().into_bytes().into()
}

fn payload(grant: &SignedGrant) -> String {
    match grant.resource {
        MediaResource::Monitor(m) => {
            format!("1.m.{m}.{}.{}.{}", grant.uid, grant.iat, grant.exp)
        }
        MediaResource::Event {
            event_id,
            monitor_id,
        } => format!(
            "1.e.{event_id}.{monitor_id}.{}.{}.{}",
            grant.uid, grant.iat, grant.exp
        ),
    }
}

fn mac_for(key: &[u8; 32], payload: &str) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(payload.as_bytes());
    mac
}

fn sign_with(key: &[u8; 32], grant: &SignedGrant) -> String {
    let payload = payload(grant);
    let tag = mac_for(key, &payload).finalize().into_bytes();
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(tag))
}

/// Why a signed URL was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    Malformed,
    BadSignature,
    Expired,
}

fn verify_with(key: &[u8; 32], token: &str, now: i64) -> Result<SignedGrant, VerifyError> {
    let (payload, tag) = token.rsplit_once('.').ok_or(VerifyError::Malformed)?;
    let tag = URL_SAFE_NO_PAD
        .decode(tag)
        .map_err(|_| VerifyError::Malformed)?;
    mac_for(key, payload)
        .verify_slice(&tag)
        .map_err(|_| VerifyError::BadSignature)?;

    let parts: Vec<&str> = payload.split('.').collect();
    let num = |s: &str| s.parse::<i64>().map_err(|_| VerifyError::Malformed);
    let id32 = |s: &str| s.parse::<u32>().map_err(|_| VerifyError::Malformed);
    let grant = match parts.as_slice() {
        ["1", "m", m, uid, iat, exp] => SignedGrant {
            resource: MediaResource::Monitor(id32(m)?),
            uid: id32(uid)?,
            iat: num(iat)?,
            exp: num(exp)?,
        },
        ["1", "e", ev, m, uid, iat, exp] => SignedGrant {
            resource: MediaResource::Event {
                event_id: ev.parse().map_err(|_| VerifyError::Malformed)?,
                monitor_id: id32(m)?,
            },
            uid: id32(uid)?,
            iat: num(iat)?,
            exp: num(exp)?,
        },
        _ => return Err(VerifyError::Malformed),
    };
    if grant.exp <= now {
        return Err(VerifyError::Expired);
    }
    Ok(grant)
}

/// Sign `resource` for `uid`, valid for `ttl` from now.
pub fn sign(resource: MediaResource, uid: u32, ttl: Duration) -> String {
    sign_until(
        resource,
        uid,
        crate::util::time::unix_now() + ttl.as_secs() as i64,
    )
}

/// Sign `resource` for `uid`, valid until the unix time `exp`.
pub fn sign_until(resource: MediaResource, uid: u32, exp: i64) -> String {
    sign_with(
        &KEY,
        &SignedGrant {
            resource,
            uid,
            iat: crate::util::time::unix_now(),
            exp,
        },
    )
}

/// Verify a signed URL token against the current time.
pub fn verify(token: &str) -> Result<SignedGrant, VerifyError> {
    verify_with(&KEY, token, crate::util::time::unix_now())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: [u8; 32] = [7; 32];

    fn grant(resource: MediaResource, exp: i64) -> SignedGrant {
        SignedGrant {
            resource,
            uid: 3,
            iat: 1_000,
            exp,
        }
    }

    #[test]
    fn a_signed_url_round_trips_for_both_resources() {
        for resource in [
            MediaResource::Monitor(12),
            MediaResource::Event {
                event_id: 9_000_000_001,
                monitor_id: 12,
            },
        ] {
            let g = grant(resource, 2_000);
            let token = sign_with(&KEY_A, &g);
            assert_eq!(verify_with(&KEY_A, &token, 1_500), Ok(g));
        }
    }

    #[test]
    fn expiry_is_enforced() {
        let token = sign_with(&KEY_A, &grant(MediaResource::Monitor(1), 2_000));
        assert_eq!(
            verify_with(&KEY_A, &token, 2_000),
            Err(VerifyError::Expired)
        );
    }

    #[test]
    fn any_change_to_the_payload_breaks_the_signature() {
        let token = sign_with(&KEY_A, &grant(MediaResource::Monitor(1), 2_000));
        // Point the same signature at another monitor, or extend its life.
        for forged in [
            token.replacen("1.m.1.", "1.m.2.", 1),
            token.replacen(".2000.", ".9999.", 1),
        ] {
            assert_ne!(forged, token);
            assert_eq!(
                verify_with(&KEY_A, &forged, 1_500),
                Err(VerifyError::BadSignature)
            );
        }
    }

    #[test]
    fn another_key_does_not_verify() {
        let token = sign_with(&KEY_A, &grant(MediaResource::Monitor(1), 2_000));
        assert_eq!(
            verify_with(&[8; 32], &token, 1_500),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn garbage_is_malformed_not_a_panic() {
        for bad in ["", "x", "1.m.1.2.3.4", "1.m.1.2.3.4.!!!", "....."] {
            assert!(verify_with(&KEY_A, bad, 0).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_url_key_follows_the_signing_key() {
        // Instances sharing JWT keys agree on it; rotating the keys changes it.
        let a = derive_key(b"signing key one");
        assert_eq!(a, derive_key(b"signing key one"));
        assert_ne!(a, derive_key(b"signing key two"));
    }
}
