#![allow(clippy::result_large_err)]
use crate::constant::*;
use crate::dto::response::TokenResponse;
use crate::error::AppResult;
use crate::util::authz::UserPermissions;
use crate::util::claim::{TokenType, UserClaims};

/// Issue an access + refresh token pair for a user.
///
/// `user_id` keys row-level monitor ACLs; `perms` is the RBAC permission
/// snapshot. Both are embedded in the token so authorization needs no
/// database round-trip for feature-level checks.
pub fn generate_tokens(
    username: String,
    user_id: u32,
    perms: UserPermissions,
) -> AppResult<TokenResponse> {
    generate_tokens_issued_at(chrono::Utc::now().timestamp(), username, user_id, perms)
}

/// [`generate_tokens`] with the issue time given. See
/// [`issue_time`](crate::service::auth::issue_time) for why login needs it.
pub fn generate_tokens_issued_at(
    iat: i64,
    username: String,
    user_id: u32,
    perms: UserPermissions,
) -> AppResult<TokenResponse> {
    let access_token = UserClaims::issued_at(
        iat,
        EXPIRE_BEARER_TOKEN_SECS,
        username.clone(),
        user_id,
        perms,
        TokenType::Access,
    )
    .encode(&ACCESS_TOKEN_ENCODE_KEY)?;
    let refresh_token = UserClaims::issued_at(
        iat,
        EXPIRE_REFRESH_TOKEN_SECS,
        username,
        user_id,
        perms,
        TokenType::Refresh,
    )
    .encode(&REFRESH_TOKEN_ENCODE_KEY)?;
    Ok(TokenResponse::new(
        access_token,
        refresh_token,
        EXPIRE_BEARER_TOKEN_SECS.as_secs(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_tokens_returns_bearer_tokens() {
        let out = generate_tokens("tester".into(), 1, UserPermissions::superuser()).unwrap();
        assert_eq!(out.token_type, crate::constant::BEARER);
        assert!(!out.access_token.is_empty());
        assert!(!out.refresh_token.is_empty());
        assert!(out.expire_in > 0);
    }

    /// A refresh token must not be accepted where an access token is expected,
    /// and vice versa — even though both decode against their own key. The
    /// `typ` claim is the guard: crossing the two fails.
    #[test]
    fn test_access_and_refresh_tokens_are_not_interchangeable() {
        let out = generate_tokens("tester".into(), 1, UserPermissions::superuser()).unwrap();

        // Each token validates on its own path...
        assert!(UserClaims::decode_access(&out.access_token).is_ok());
        assert!(UserClaims::decode_refresh(&out.refresh_token).is_ok());

        // ...but a refresh token is rejected as an access token, and the
        // reverse also fails (a token signed by one key won't verify under the
        // other, and even if it did the `typ` assertion would reject it).
        assert!(UserClaims::decode_access(&out.refresh_token).is_err());
        assert!(UserClaims::decode_refresh(&out.access_token).is_err());
    }
}
