use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use tracing::{debug, warn};

use crate::{server::state::AppState, util::claim::UserClaims};

/// Middleware to verify JWT token for protected routes
///
/// It does not check token revocation. Use it only inside a router that
/// [`authz::protect`](crate::util::authz::protect) or a monitor gate wraps,
/// which do; a route with a login and no feature gate takes
/// [`authenticated_middleware`]. `tests/token_revocation.rs` walks every
/// documented route with a revoked token.
pub async fn auth_middleware(
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    // Path only: a query string can carry a token, which must not reach logs.
    debug!("Authenticating request: {}", request.uri().path());

    // Extract the authorization header
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|header| header.to_str().ok())
        .ok_or_else(|| {
            warn!("Missing Authorization header");
            (
                StatusCode::UNAUTHORIZED,
                "Missing Authorization header".to_string(),
            )
        })?;

    // Check if it's a Bearer token
    if !auth_header.starts_with("Bearer ") {
        warn!("Invalid Authorization header format");
        return Err((
            StatusCode::UNAUTHORIZED,
            "Invalid Authorization header format".to_string(),
        ));
    }

    // Extract the token
    let token = auth_header[7..].trim();

    // Verify the token
    match UserClaims::decode_access(token) {
        Ok(user_claims) => {
            // Add user claims to request extensions for handlers to access
            let mut request = request;
            request.extensions_mut().insert(user_claims);

            // Continue to the handler
            Ok(next.run(request).await)
        }
        Err(e) => {
            warn!("JWT token verification failed: {}", e);
            Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()))
        }
    }
}

/// Authentication middleware for self-service routes that are *not* wrapped by
/// [`authz::protect`](crate::util::authz::protect) — `/me`, `/me/password`,
/// `/auth/logout`. Because there is no feature gate, these routes would
/// otherwise never see the token-revocation check that `protect` performs, so
/// this middleware runs it here: header-only token, decode, reject if revoked,
/// then insert [`UserClaims`] for the handler.
pub async fn authenticated_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    let token = extract_token_from_header(&request).ok_or_else(|| {
        warn!("Missing or malformed Authorization header");
        (
            StatusCode::UNAUTHORIZED,
            "Missing Authorization header".to_string(),
        )
    })?;

    let claims = match UserClaims::decode_access(&token) {
        Ok(claims) => claims,
        Err(e) => {
            warn!("JWT token verification failed: {e}");
            return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
        }
    };

    if state.revocations.is_revoked(claims.uid, claims.iat) {
        warn!("Rejected revoked token for user id {}", claims.uid);
        return Err((
            StatusCode::UNAUTHORIZED,
            "Token has been revoked".to_string(),
        ));
    }

    let mut request = request;
    request.extensions_mut().insert(claims);
    Ok(next.run(request).await)
}

/// Middleware for media endpoints that accepts JWT via Authorization header OR query parameter.
///
/// This is necessary because HTML5 `<video>` and `<img>` elements cannot send custom headers
/// when loading media via the `src` attribute. This middleware allows authentication via:
///
/// 1. `Authorization: Bearer <token>` header (preferred)
/// 2. `?token=<token>` query parameter (fallback for media elements)
///
/// # Security Note
///
/// Query parameter tokens are visible in browser history and server logs. This is standard
/// practice for media streaming but the frontend should be aware of this trade-off.
pub async fn media_auth_middleware(
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    // Path only: media URLs may carry `?token=<JWT>`, which must not reach logs.
    debug!("Authenticating media request: {}", request.uri().path());

    // Already authenticated by a signed media URL (`media_grant_layer`).
    if request
        .extensions()
        .get::<crate::service::media_grant::MediaGrant>()
        .is_some()
    {
        return Ok(next.run(request).await);
    }

    // Try to extract token from Authorization header first
    let token = extract_token_from_header(&request).or_else(|| extract_token_from_query(&request));

    let token = token.ok_or_else(|| {
        warn!("No authentication token provided (checked header and query param)");
        (
            StatusCode::UNAUTHORIZED,
            "Authentication required. Provide Authorization header or token query parameter."
                .to_string(),
        )
    })?;

    // Verify the token
    match UserClaims::decode_access(&token) {
        Ok(user_claims) => {
            // Add user claims to request extensions for handlers to access
            let mut request = request;
            request.extensions_mut().insert(user_claims);

            // Continue to the handler
            Ok(next.run(request).await)
        }
        Err(e) => {
            warn!("JWT token verification failed: {}", e);
            Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()))
        }
    }
}

/// Extract Bearer token from Authorization header
pub(crate) fn extract_token_from_header(request: &Request) -> Option<String> {
    request
        .headers()
        .get("Authorization")
        .and_then(|header| header.to_str().ok())
        .filter(|auth| auth.starts_with("Bearer "))
        .map(|auth| auth[7..].trim().to_string())
}

/// Extract token from query parameter
pub(crate) fn extract_token_from_query(request: &Request) -> Option<String> {
    request.uri().query().and_then(|query| {
        // Parse query string to find 'token' parameter
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            if key == "token" {
                // URL decode the token value
                percent_decode(value)
            } else {
                None
            }
        })
    })
}

/// Percent-decode a URL query parameter value (form semantics: `+` is a space).
///
/// Decoded `%XX` escapes are assembled into a byte buffer and interpreted as
/// UTF-8 at the end. The previous implementation pushed each decoded byte via
/// `byte as char`, which is only correct for ASCII — a multi-byte sequence like
/// `%C3%A9` produced two garbage chars instead of `é`. Invalid hex, truncated
/// escapes, and byte sequences that aren't valid UTF-8 all yield `None`.
/// See REVIEW_FIXES_PLAN §5.4.
fn percent_decode(input: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '%' => {
                let hex: String = chars.by_ref().take(2).collect();
                if hex.len() != 2 {
                    return None; // Incomplete escape sequence
                }
                let byte = u8::from_str_radix(&hex, 16).ok()?; // Invalid hex
                bytes.push(byte);
            }
            '+' => bytes.push(b' '),
            other => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }

    String::from_utf8(bytes).ok()
}

/// Paths that still answer while the schema is behind: liveness and version,
/// so monitoring and `zm-web` can tell what is wrong.
const SCHEMA_GATE_OPEN: &[&str] = &["/api/v3/server/health_check", "/api/v3/host/getVersion"];

/// While ZoneMinder's schema is older than this build needs, every API call
/// answers 503 saying how to upgrade it (#147). The UI and the API docs are
/// still served.
pub async fn schema_gate(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = req.uri().path();
    match &state.schema_behind {
        Some(msg) if path.starts_with("/api/v3/") && !SCHEMA_GATE_OPEN.contains(&path) => {
            crate::error::AppError::ServiceUnavailableError(msg.to_string()).into_response()
        }
        _ => next.run(req).await,
    }
}

/// Route layer for `POST /monitors`. ZoneMinder splits adding monitors
/// (`Monitors: Create`, or `System: Edit` in its API) from changing them
/// (`Edit`), which is all the route's `protect` layer checks. Runs before the
/// handler so the check does not depend on the request body parsing.
pub async fn require_monitor_create(
    claims: UserClaims,
    request: Request,
    next: Next,
) -> Result<Response, crate::error::AppError> {
    if claims.perms.can_create_monitors() {
        Ok(next.run(request).await)
    } else {
        Err(crate::error::AppError::PermissionDeniedError(
            "Create access to Monitors required".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn request_with_header(value: &str) -> Request {
        Request::builder()
            .uri("/")
            .header("Authorization", value)
            .body(Body::empty())
            .unwrap()
    }

    fn request_with_uri(uri: &str) -> Request {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    #[test]
    fn header_token_requires_bearer_prefix() {
        assert_eq!(
            extract_token_from_header(&request_with_header("Bearer abc123")),
            Some("abc123".to_string())
        );
        // The `Bearer ` value is trimmed.
        assert_eq!(
            extract_token_from_header(&request_with_header("Bearer   spaced  ")),
            Some("spaced".to_string())
        );
        // A non-Bearer scheme yields nothing.
        assert_eq!(
            extract_token_from_header(&request_with_header("Basic abc123")),
            None
        );
        // A request with no Authorization header yields nothing.
        assert_eq!(extract_token_from_header(&request_with_uri("/")), None);
    }

    #[test]
    fn query_token_is_picked_out_of_the_query_string() {
        assert_eq!(
            extract_token_from_query(&request_with_uri("/stream?token=abc")),
            Some("abc".to_string())
        );
        assert_eq!(
            extract_token_from_query(&request_with_uri("/stream?foo=1&token=xyz&bar=2")),
            Some("xyz".to_string())
        );
        assert_eq!(extract_token_from_query(&request_with_uri("/stream")), None);
        assert_eq!(
            extract_token_from_query(&request_with_uri("/stream?foo=1")),
            None
        );
    }

    #[test]
    fn percent_decode_handles_escapes_plus_and_rejects_bad_input() {
        assert_eq!(percent_decode("plain"), Some("plain".to_string()));
        assert_eq!(percent_decode("a%20b"), Some("a b".to_string()));
        assert_eq!(percent_decode("a+b"), Some("a b".to_string()));
        // Multi-byte UTF-8: %C3%A9 is `é` (was mangled by the old byte-as-char).
        assert_eq!(percent_decode("caf%C3%A9"), Some("café".to_string()));
        // Incomplete escape sequence.
        assert_eq!(percent_decode("a%2"), None);
        // Invalid hex digits.
        assert_eq!(percent_decode("a%zzb"), None);
        // Bytes that don't form valid UTF-8 are rejected.
        assert_eq!(percent_decode("%80%81"), None);
    }
}
