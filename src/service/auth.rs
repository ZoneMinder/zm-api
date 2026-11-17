#![allow(clippy::result_large_err)]
use crate::dto::request::{LoginRequest, RefreshTokenRequest};
use crate::dto::response::TokenResponse;
use crate::error::AppError;
use crate::error::AppResult;
use crate::error::ToAppResult;
use crate::repo::users as user;
use crate::server::state::AppState;
use crate::service::token;
use crate::util::claim::UserClaims;
use crate::util::password;
use tracing::info;

/// Single error returned for any login failure. Identical wording, status,
/// and timing on "no such user", "user not enabled", "API access not
/// granted", and "wrong password" — see `verify_existing_or_dummy` in
/// `util::password` for the timing side of the contract.
fn invalid_credentials() -> AppError {
    AppError::UnauthorizedError("Invalid username or password".to_string())
}

pub async fn login(state: &AppState, req: LoginRequest) -> AppResult<TokenResponse> {
    info!("Login attempt for user: {}", req.username);
    let user_opt = user::find_by_username_and_status(&state.db, &req.username, true).await?;
    // Pull the hash out before consuming user_opt — verify spends bcrypt's
    // wall-clock cost on the dummy hash when the user lookup missed, so the
    // "no such user" path doesn't return faster than "wrong password".
    let user_hash = user_opt.as_ref().map(|u| u.password.clone());
    let password_ok = password::verify_existing_or_dummy(req.password, user_hash).await;

    let user = match (user_opt, password_ok) {
        (Some(u), true) => u,
        _ => return Err(invalid_credentials()),
    };

    // The user's own levels raised by their role's (#143).
    let perms = crate::util::authz::effective_permissions(&state.db, &user).await?;
    let floor = state
        .revocations
        .floor(user.id)
        .unwrap_or(0)
        .max(user.token_min_expiry as i64);
    let iat = issue_time(chrono::Utc::now().timestamp(), floor);
    let resp = token::generate_tokens_issued_at(iat, user.username, user.id, perms)?;
    Ok(resp)
}

/// When a token minted now, for a user whose revocation floor is `floor`,
/// should say it was issued.
///
/// A revocation sets the floor to the next second, so that it also catches
/// tokens minted earlier in the second it happens in. A login later in that
/// same second would then be handed a token that is already revoked: log out
/// and straight back in, or sign in just as an administrator changes your
/// role, and the first request is a 401. Such a token is dated at the floor
/// instead, one second ahead, which no check minds.
///
/// A floor further ahead is an administrator keeping the user out, and is
/// left to do that.
pub fn issue_time(now: i64, floor: i64) -> i64 {
    if floor == now + 1 {
        floor
    } else {
        now
    }
}

pub async fn refresh_token(state: &AppState, req: RefreshTokenRequest) -> AppResult<TokenResponse> {
    let user_claims = UserClaims::decode_refresh(&req.token)?.claims;
    info!("Refresh token: {user_claims:?}");
    let user = user::find_by_username_and_status(&state.db, &user_claims.user, true)
        .await?
        .to_result()?;
    // The token names its account by id. The lookup above is by username, so
    // without this an account deleted and created again under the same name
    // would be handed to whoever holds the old account's refresh token.
    if user.id != user_claims.uid {
        return Err(AppError::UnauthorizedError(
            "Token has been revoked".to_string(),
        ));
    }
    // Revocation check: refresh tokens minted before the user's
    // TokenMinExpiry floor (logout, password change) must not mint new tokens.
    // The in-memory floor is checked as well as the row's: it also holds a
    // revocation whose database write failed.
    if user_claims.iat < user.token_min_expiry as i64
        || state
            .revocations
            .is_revoked(user_claims.uid, user_claims.iat)
    {
        return Err(AppError::UnauthorizedError(
            "Token has been revoked".to_string(),
        ));
    }
    info!("Set new session for user: {}", user.id);
    // Re-read permissions from the user row (and their role) so a refresh
    // picks up any permission changes made since the previous token was issued.
    let perms = crate::util::authz::effective_permissions(&state.db, &user).await?;
    let resp = token::generate_tokens(user.username, user.id, perms)?;
    info!("Refresh token success: {user_claims:?}");
    Ok(resp)
}

/// Revoke every token issued to `uid` before this instant: persist the floor
/// to `Users.TokenMinExpiry` and mirror it into the in-memory registry that
/// [`crate::util::authz`] consults on each request.
pub async fn logout(state: &AppState, uid: u32) -> AppResult<()> {
    // Floor at now+1 so tokens minted earlier in the *current* second (e.g.
    // the one authenticating this very logout) are revoked too.
    let floor = chrono::Utc::now().timestamp() + 1;
    user::set_token_min_expiry(&state.db, uid, floor as u64).await?;
    state.revocations.revoke(uid, floor);
    info!("Revoked all tokens for user id {uid} issued before {floor}");
    Ok(())
}

/// Change the authenticated user's own password: verify the current password,
/// store a fresh bcrypt hash, and revoke every existing token so old sessions
/// (and anyone holding a stolen token) are logged out.
pub async fn change_password(
    state: &AppState,
    uid: u32,
    current_password: String,
    new_password: String,
) -> AppResult<()> {
    let user = user::find_by_id(&state.db, uid)
        .await?
        .ok_or_else(|| AppError::UnauthorizedError("User not found".to_string()))?;

    // Verify the current password before allowing the change.
    if !password::verify_existing_or_dummy(current_password, Some(user.password.clone())).await {
        return Err(AppError::UnauthorizedError(
            "Current password is incorrect".to_string(),
        ));
    }

    let new_hash = password::hash(new_password).await?;
    user::set_password(&state.db, uid, new_hash).await?;

    // A password change invalidates every outstanding session.
    logout(state, uid).await?;
    info!("Password changed for user id {uid}");
    Ok(())
}

/// Whether ZoneMinder's stock login, `admin` with the password `admin`, is
/// still good here: the account is enabled for the API and has that password.
/// Every install starts with it, and everyone knows it.
pub async fn stock_admin_login_works(users: &[crate::entity::users::Model]) -> bool {
    let Some(admin) = users
        .iter()
        .find(|u| u.username == "admin" && u.enabled != 0 && u.api_enabled != 0)
    else {
        return false;
    };
    password::verify_existing_or_dummy("admin".to_string(), Some(admin.password.clone())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_minted_in_the_second_of_a_revocation_is_not_born_revoked() {
        let now = 1_700_000_000;
        // No floor, or an old one: issued now.
        assert_eq!(issue_time(now, 0), now);
        assert_eq!(issue_time(now, now - 30), now);
        assert_eq!(issue_time(now, now), now);
        // Revoked this second: dated at the floor, so it is not caught by it.
        assert_eq!(issue_time(now, now + 1), now + 1);
        // An administrator's lockout further ahead still holds.
        assert_eq!(issue_time(now, now + 3600), now);
    }

    /// Log out and straight back in: the new token must work.
    #[tokio::test]
    async fn a_login_right_after_a_revocation_gets_a_working_token() {
        let plain = "secret".to_string();
        let hashed = crate::util::password::hash(plain.clone()).await.unwrap();
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::users::Model, _, _>(vec![vec![mk_user(
                "alice", &hashed,
            )]])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        // As logout and a role change do: the floor is the next second. The
        // password check may run into that second; the token works either way.
        let before = chrono::Utc::now().timestamp();
        state.revocations.revoke(1, before + 1);

        let resp = login(
            &state,
            LoginRequest {
                username: "alice".into(),
                password: plain,
            },
        )
        .await
        .unwrap();
        let claims = UserClaims::decode_access(&resp.access_token).unwrap();
        assert!(
            !state.revocations.is_revoked(claims.uid, claims.iat),
            "iat {} against a floor of {}",
            claims.iat,
            before + 1
        );
    }

    #[tokio::test]
    async fn the_stock_admin_login_is_noticed() {
        let stock = crate::util::password::hash("admin".to_string())
            .await
            .unwrap();
        let changed = crate::util::password::hash("correct horse".to_string())
            .await
            .unwrap();
        assert!(stock_admin_login_works(&[mk_user("admin", &stock)]).await);
        assert!(!stock_admin_login_works(&[mk_user("admin", &changed)]).await);
        // Another account with that password is not the stock login.
        assert!(!stock_admin_login_works(&[mk_user("alice", &stock)]).await);
        // Nor is a disabled admin.
        let mut disabled = mk_user("admin", &stock);
        disabled.enabled = 0;
        assert!(!stock_admin_login_works(&[disabled]).await);
    }
    use crate::error::AppError;
    use crate::server::state::AppState;
    use crate::util::authz::UserPermissions;
    use sea_orm::{DatabaseBackend, MockDatabase};

    fn mk_user(username: &str, hashed: &str) -> crate::entity::users::Model {
        use crate::entity::sea_orm_active_enums as E;
        crate::entity::users::Model {
            id: 1,
            username: username.into(),
            password: hashed.into(),
            name: "Name".into(),
            email: "a@b.com".into(),
            phone: "".into(),
            language: None,
            enabled: 1,
            stream: E::Stream::View,
            events: E::Events::View,
            control: E::Control::View,
            monitors: E::Monitors::View,
            groups: E::Groups::View,
            devices: E::Devices::View,
            snapshots: E::Snapshots::View,
            system: E::System::View,
            max_bandwidth: None,
            token_min_expiry: 0,
            api_enabled: 1,
            home_view: "console".into(),
            role_id: None,
        }
    }

    #[tokio::test]
    async fn test_login_success() {
        // Prepare a hashed password and DB returning that user
        let plain = "secret".to_string();
        let hashed = crate::util::password::hash(plain.clone()).await.unwrap();
        let user_row = mk_user("alice", &hashed);
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::users::Model, _, _>(vec![vec![user_row]])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        let req = LoginRequest {
            username: "alice".into(),
            password: plain,
        };
        let resp = login(&state, req).await.unwrap();
        assert!(!resp.access_token.is_empty());
        assert!(!resp.refresh_token.is_empty());
    }

    /// A role granting `Edit` on Events and `Create` on Monitors, `None`
    /// elsewhere.
    fn role() -> crate::entity::user_roles::Model {
        use crate::entity::sea_orm_active_enums as E;
        crate::entity::user_roles::Model {
            id: 7,
            name: "operators".into(),
            description: None,
            stream: E::Stream::None,
            events: E::Events::Edit,
            control: E::Control::None,
            monitors: E::Monitors::Create,
            groups: E::Groups::None,
            devices: E::Devices::None,
            snapshots: E::Snapshots::None,
            system: E::System::None,
        }
    }

    /// ZoneMinder grants the higher of the user's and their role's level.
    #[tokio::test]
    async fn test_login_token_carries_role_permissions() {
        let plain = "secret".to_string();
        let hashed = crate::util::password::hash(plain.clone()).await.unwrap();
        let mut user_row = mk_user("carol", &hashed);
        user_row.role_id = Some(7);
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::users::Model, _, _>(vec![vec![user_row]])
            .append_query_results(vec![vec![role()]])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        let resp = login(
            &state,
            LoginRequest {
                username: "carol".into(),
                password: plain,
            },
        )
        .await
        .unwrap();
        let claims = UserClaims::decode_access(&resp.access_token).unwrap();
        // Raised by the role.
        assert_eq!(claims.perms.events, crate::util::authz::Level::Edit);
        assert_eq!(claims.perms.monitors, crate::util::authz::Level::Create);
        // The user's own View is kept where the role grants None.
        assert_eq!(claims.perms.system, crate::util::authz::Level::View);
    }

    /// Both "wrong password" and "no such user" must surface the same
    /// generic 401-flavoured error — no leak of which side failed.
    #[tokio::test]
    async fn test_login_wrong_password_returns_unauthorized() {
        let hashed_other = crate::util::password::hash("different".to_string())
            .await
            .unwrap();
        let user_row = mk_user("bob", &hashed_other);
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::users::Model, _, _>(vec![vec![user_row]])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        let err = login(
            &state,
            LoginRequest {
                username: "bob".into(),
                password: "wrong".into(),
            },
        )
        .await
        .expect_err("should fail");
        assert!(
            matches!(err, AppError::UnauthorizedError(_)),
            "wrong-password should surface UnauthorizedError, got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_login_unknown_user_returns_unauthorized() {
        let empty: Vec<crate::entity::users::Model> = vec![];
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::users::Model, _, _>(vec![empty])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        let err = login(
            &state,
            LoginRequest {
                username: "nobody".into(),
                password: "x".into(),
            },
        )
        .await
        .expect_err("should fail");
        assert!(
            matches!(err, AppError::UnauthorizedError(_)),
            "missing-user should surface UnauthorizedError (no leak vs wrong-pw), got {err:?}"
        );
    }

    /// `APIEnabled = 0` is meant to disable API access while leaving web-UI
    /// login intact. The repo filters on the column, so a disabled-API
    /// user looks like a missing user to the auth path — same unified
    /// error.
    /// A refresh token minted before the user's `TokenMinExpiry` floor
    /// (logout / password change) must not mint new tokens.
    #[tokio::test]
    async fn test_refresh_rejected_when_issued_before_token_min_expiry() {
        let mut user_row = mk_user("dave", "irrelevant-hash");
        user_row.token_min_expiry = (chrono::Utc::now().timestamp() + 10_000) as u64;
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::users::Model, _, _>(vec![vec![user_row]])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        let tokens =
            token::generate_tokens("dave".to_string(), 1, UserPermissions::default()).unwrap();
        let err = refresh_token(
            &state,
            RefreshTokenRequest {
                token: tokens.refresh_token,
            },
        )
        .await
        .expect_err("refresh with a pre-floor token must fail");
        assert!(
            matches!(err, AppError::UnauthorizedError(_)),
            "expected UnauthorizedError, got {err:?}"
        );
    }

    /// A refresh token names the account it was issued to by id. An account
    /// deleted and created again under the same username is a different one.
    #[tokio::test]
    async fn test_refresh_rejected_for_another_account_with_the_same_username() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::users::Model, _, _>(vec![vec![mk_user(
                "dave",
                "irrelevant-hash",
            )]])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        // mk_user's row has id 1; this token was issued to id 99.
        let tokens =
            token::generate_tokens("dave".to_string(), 99, UserPermissions::default()).unwrap();
        let err = refresh_token(
            &state,
            RefreshTokenRequest {
                token: tokens.refresh_token,
            },
        )
        .await
        .expect_err("a token for another user id must not refresh");
        assert!(
            matches!(err, AppError::UnauthorizedError(_)),
            "expected UnauthorizedError, got {err:?}"
        );
    }

    /// The in-memory floor counts too: it is all that records a revocation
    /// whose database write failed or has not landed yet.
    #[tokio::test]
    async fn test_refresh_rejected_when_revoked_in_memory() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::users::Model, _, _>(vec![vec![mk_user(
                "dave",
                "irrelevant-hash",
            )]])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        let tokens =
            token::generate_tokens("dave".to_string(), 1, UserPermissions::default()).unwrap();
        state
            .revocations
            .revoke(1, chrono::Utc::now().timestamp() + 10_000);
        let err = refresh_token(
            &state,
            RefreshTokenRequest {
                token: tokens.refresh_token,
            },
        )
        .await
        .expect_err("a token revoked in memory must not refresh");
        assert!(
            matches!(err, AppError::UnauthorizedError(_)),
            "expected UnauthorizedError, got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_login_api_disabled_user_returns_unauthorized() {
        // Empty result set models how `find_by_username_and_status` will
        // behave once the `ApiEnabled = 1` filter is applied: a row with
        // ApiEnabled = 0 doesn't match, so the query returns no rows.
        let empty: Vec<crate::entity::users::Model> = vec![];
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::users::Model, _, _>(vec![empty])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        let err = login(
            &state,
            LoginRequest {
                username: "carol".into(),
                password: "anything".into(),
            },
        )
        .await
        .expect_err("api-disabled user must not authenticate");
        assert!(
            matches!(err, AppError::UnauthorizedError(_)),
            "api-disabled user should surface UnauthorizedError, got {err:?}"
        );
    }
}
