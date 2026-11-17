#![allow(clippy::result_large_err)]
use std::time::Duration;

use axum_extra::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::RequestPartsExt;
use chrono::Utc;
use dashmap::DashMap;
use fake::Dummy;
use jsonwebtoken::Header;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, TokenData, Validation};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde::Serialize;
use utoipa::ToSchema;

use crate::constant::{ACCESS_TOKEN_DECODE_KEY, REFRESH_TOKEN_DECODE_KEY};
use crate::error::{AppError, AppResult};
use crate::util::authz::UserPermissions;

pub static DECODE_HEADER: Lazy<Validation> = Lazy::new(|| {
    let mut validation = Validation::new(Algorithm::RS256);
    // The library's default accepts a token for 60 seconds after it expires,
    // to allow for clock skew between issuer and verifier. Here they are the
    // same server, so an expired token is simply expired.
    validation.leeway = 0;
    validation
});
pub static ENCODE_HEADER: Lazy<Header> = Lazy::new(|| Header::new(Algorithm::RS256));

/// Which of the two token kinds a JWT is. Access and refresh tokens are
/// otherwise structurally identical `UserClaims`, separated only by the key
/// pair that signs them; `typ` makes the distinction explicit so a
/// misconfiguration that pointed both key paths at the same pair could not
/// silently let a refresh token act as an access token (or vice versa).
#[derive(
    Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Dummy, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TokenType {
    Access,
    Refresh,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone, Dummy, ToSchema)]
pub struct UserClaims {
    // issued at
    pub iat: i64,
    // expiration
    pub exp: i64,
    // token kind — asserted on decode so access and refresh tokens are not
    // interchangeable even if the signing keys were ever misconfigured.
    pub typ: TokenType,
    // username
    pub user: String,
    // numeric user id, used to resolve row-level monitor ACLs. Required at
    // the protocol level: a token without `uid` fails to deserialise and the
    // request is rejected. (The earlier `#[serde(default)]` was speculative
    // backward-compat for tokens issued before this field existed — but the
    // field was added together with the ACL machinery, so no such tokens
    // could ever have exploited the missing-uid default-allow path.)
    pub uid: u32,
    // per-feature permission snapshot (RBAC). Defaulted for backward
    // compatibility with tokens issued before RBAC existed.
    #[serde(default)]
    pub perms: UserPermissions,
}

impl UserClaims {
    pub fn new(
        duration: Duration,
        username: String,
        user_id: u32,
        perms: UserPermissions,
        typ: TokenType,
    ) -> Self {
        Self::issued_at(
            Utc::now().timestamp(),
            duration,
            username,
            user_id,
            perms,
            typ,
        )
    }

    /// [`new`](Self::new) with the issue time given.
    pub fn issued_at(
        iat: i64,
        duration: Duration,
        username: String,
        user_id: u32,
        perms: UserPermissions,
        typ: TokenType,
    ) -> Self {
        Self {
            iat,
            exp: iat + (duration.as_secs() as i64),
            typ,
            user: username,
            uid: user_id,
            perms,
        }
    }

    pub fn decode(
        token: &str,
        key: &DecodingKey,
    ) -> Result<TokenData<Self>, jsonwebtoken::errors::Error> {
        jsonwebtoken::decode::<UserClaims>(token, key, &DECODE_HEADER)
    }

    /// Decode and validate an **access** token against the access key, then
    /// assert its `typ` is [`TokenType::Access`]. A refresh token presented
    /// here is rejected as `InvalidToken`.
    ///
    /// Verified tokens are remembered, so a client's token pays for the RSA
    /// signature check once instead of on every request (and once per auth
    /// layer within a request). A remembered token still gets the same expiry
    /// check `jsonwebtoken` applies. Revocation is not part of decoding; callers
    /// check it separately on every request, as before.
    pub fn decode_access(token: &str) -> Result<Self, jsonwebtoken::errors::Error> {
        let now = jsonwebtoken::get_current_timestamp();
        if let Some(claims) = ACCESS_TOKEN_CACHE.get(token).map(|c| c.clone()) {
            if is_expired(&claims, now) {
                ACCESS_TOKEN_CACHE.remove(token);
                return Err(jsonwebtoken::errors::ErrorKind::ExpiredSignature.into());
            }
            return Ok(claims);
        }

        let claims = Self::decode(token, &ACCESS_TOKEN_DECODE_KEY)?.claims;
        if claims.typ != TokenType::Access {
            return Err(jsonwebtoken::errors::ErrorKind::InvalidToken.into());
        }
        remember_access_token(token, &claims, now);
        Ok(claims)
    }

    /// Decode and validate a **refresh** token against the refresh key, then
    /// assert its `typ` is [`TokenType::Refresh`].
    pub fn decode_refresh(token: &str) -> Result<TokenData<Self>, jsonwebtoken::errors::Error> {
        let data = Self::decode(token, &REFRESH_TOKEN_DECODE_KEY)?;
        if data.claims.typ != TokenType::Refresh {
            return Err(jsonwebtoken::errors::ErrorKind::InvalidToken.into());
        }
        Ok(data)
    }

    pub fn encode(&self, key: &EncodingKey) -> Result<String, jsonwebtoken::errors::Error> {
        jsonwebtoken::encode(&ENCODE_HEADER, self, key)
    }
}

/// Upper bound on remembered access tokens. Only tokens that passed signature
/// verification get in, so filling it takes valid credentials; when it is full,
/// expired entries are dropped first and the whole cache only if that is not
/// enough.
const ACCESS_TOKEN_CACHE_CAP: usize = 4096;

static ACCESS_TOKEN_CACHE: Lazy<DashMap<String, UserClaims>> = Lazy::new(DashMap::new);

/// The expiry rule `jsonwebtoken` applies on decode (`exp` plus the leeway),
/// repeated for tokens served from the cache.
fn is_expired(claims: &UserClaims, now: u64) -> bool {
    claims.exp < now as i64 - DECODE_HEADER.leeway as i64
}

fn remember_access_token(token: &str, claims: &UserClaims, now: u64) {
    if ACCESS_TOKEN_CACHE.len() >= ACCESS_TOKEN_CACHE_CAP {
        ACCESS_TOKEN_CACHE.retain(|_, c| !is_expired(c, now));
        if ACCESS_TOKEN_CACHE.len() >= ACCESS_TOKEN_CACHE_CAP {
            ACCESS_TOKEN_CACHE.clear();
        }
    }
    ACCESS_TOKEN_CACHE.insert(token.to_owned(), claims.clone());
}

pub trait UserClaimsRequest {
    fn get_user_name(&self) -> AppResult<String>;
    fn get_user_claims(&self) -> AppResult<UserClaims>;
}

impl UserClaimsRequest for axum::extract::Request {
    fn get_user_name(&self) -> AppResult<String> {
        self.extensions()
            .get::<UserClaims>()
            .map(|u| u.user.clone())
            .ok_or_else(|| AppError::UnauthorizedError("User Must Login".to_string()))
    }

    fn get_user_claims(&self) -> AppResult<UserClaims> {
        self.extensions()
            .get::<UserClaims>()
            .cloned()
            .ok_or_else(|| AppError::UnauthorizedError("User Must Login".to_string()))
    }
}

impl<S> FromRequestParts<S> for UserClaims
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let TypedHeader(Authorization(bearer)) = parts
            .extract::<TypedHeader<Authorization<Bearer>>>()
            .await?;
        let user_claims = UserClaims::decode_access(bearer.token())?;
        Ok(user_claims)
    }
}

#[cfg(test)]
mod tests {
    use crate::util::key::RsaPairKey;
    use fake::{Fake, Faker};

    use super::*;

    #[test]
    fn test_user_claims() {
        let username: String = Faker.fake();
        let pair_key = RsaPairKey::new(2048).unwrap();
        let claims = UserClaims::new(
            Duration::from_secs(100),
            username,
            42,
            UserPermissions::superuser(),
            TokenType::Access,
        );
        // println!(
        //     "private key: {}",
        //     String::from_utf8(pair_key.private_key.clone()).unwrap()
        // );
        // println!(
        //     "public key: {}",
        //     String::from_utf8(pair_key.public_key.clone()).unwrap()
        // );
        let token = claims
            .encode(&EncodingKey::from_rsa_pem(&pair_key.private_key).unwrap())
            .unwrap();
        let actual_claims = UserClaims::decode(
            &token,
            &DecodingKey::from_rsa_pem(&pair_key.public_key).unwrap(),
        )
        .unwrap()
        .claims;
        assert_eq!(actual_claims, claims)
    }

    fn claims_expiring_at(exp: i64) -> UserClaims {
        UserClaims {
            iat: exp - 600,
            exp,
            typ: TokenType::Access,
            user: "cache-test".into(),
            uid: 7,
            perms: UserPermissions::default(),
        }
    }

    #[test]
    fn an_expired_token_gets_no_grace_period() {
        assert_eq!(DECODE_HEADER.leeway, 0);
        let now = 1_000_000u64;
        assert!(is_expired(&claims_expiring_at(now as i64 - 1), now));
        assert!(!is_expired(&claims_expiring_at(now as i64), now));
    }

    #[test]
    fn expiry_matches_jsonwebtoken_leeway() {
        let now = 1_000_000u64;
        let leeway = DECODE_HEADER.leeway as i64;
        assert!(!is_expired(&claims_expiring_at(now as i64 + 10), now));
        // Inside the leeway still passes, exactly as a fresh decode would.
        assert!(!is_expired(&claims_expiring_at(now as i64 - leeway), now));
        assert!(is_expired(
            &claims_expiring_at(now as i64 - leeway - 1),
            now
        ));
    }

    #[test]
    fn a_cached_token_that_has_expired_is_rejected_and_forgotten() {
        let token = "cache-test-expired-token";
        let now = jsonwebtoken::get_current_timestamp();
        let expired = claims_expiring_at(now as i64 - DECODE_HEADER.leeway as i64 - 5);
        ACCESS_TOKEN_CACHE.insert(token.to_owned(), expired);

        let err = UserClaims::decode_access(token).unwrap_err();
        assert_eq!(
            *err.kind(),
            jsonwebtoken::errors::ErrorKind::ExpiredSignature
        );
        assert!(ACCESS_TOKEN_CACHE.get(token).is_none());
    }

    #[test]
    fn a_cached_live_token_is_served_without_reverifying() {
        // Not a real JWT: only a cache hit can make this decode succeed.
        let token = "cache-test-live-token";
        let now = jsonwebtoken::get_current_timestamp();
        let live = claims_expiring_at(now as i64 + 600);
        ACCESS_TOKEN_CACHE.insert(token.to_owned(), live.clone());

        assert_eq!(UserClaims::decode_access(token).unwrap(), live);
        ACCESS_TOKEN_CACHE.remove(token);
    }

    #[test]
    fn a_full_cache_drops_expired_entries_first() {
        let now = 2_000_000u64;
        let live = claims_expiring_at(now as i64 + 600);
        let dead = claims_expiring_at(0);
        // Other tests share the static cache, so only assert on our own keys.
        for i in 0..ACCESS_TOKEN_CACHE_CAP {
            ACCESS_TOKEN_CACHE.insert(format!("cache-test-dead-{i}"), dead.clone());
        }
        ACCESS_TOKEN_CACHE.insert("cache-test-keep".into(), live.clone());
        remember_access_token("cache-test-new", &live, now);

        assert!(ACCESS_TOKEN_CACHE.get("cache-test-dead-0").is_none());
        assert!(ACCESS_TOKEN_CACHE.get("cache-test-keep").is_some());
        assert!(ACCESS_TOKEN_CACHE.get("cache-test-new").is_some());
        ACCESS_TOKEN_CACHE.remove("cache-test-keep");
        ACCESS_TOKEN_CACHE.remove("cache-test-new");
    }
}
