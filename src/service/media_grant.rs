//! Request authentication by signed media URL (`?st=`), for the streaming
//! routes. See [`crate::util::media_url`] for the token itself.
//!
//! [`media_grant_layer`] runs ahead of the routes' usual checks. When a
//! request carries `st`, it verifies the signature and expiry, that the token
//! was signed for the monitor or event the route serves, that the user's
//! tokens haven't been revoked since it was issued (logout, password change,
//! disable: `Users.TokenMinExpiry`), and that the user may still view that
//! monitor and has Stream (live) or Events (recording) access now. It then
//! records a [`MediaGrant`], which `protect`, `media_auth_middleware`,
//! `monitor_path_guard` and the `MonitorScope` extractor honour in place of an
//! access token. Nothing is stored: the token carries everything.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::{MatchedPath, RawPathParams, Request, State};
use axum::http::{Method, Uri};
use axum::middleware::Next;
use axum::response::Response;

use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::server::state::AppState;
use crate::service::monitor_acl::{self, MonitorScope};
use crate::streaming::hls::signed_playlist::UriSigner;
use crate::util::authz::{Feature, Level};
use crate::util::claim::UserClaims;
use crate::util::media_url::{self, MediaResource};

/// A request authenticated by a signed media URL.
#[derive(Debug, Clone)]
pub struct MediaGrant {
    pub resource: MediaResource,
    pub uid: u32,
    pub exp: i64,
    /// The `st` value the request carried, re-listed on child playlists.
    pub token: String,
}

impl MediaGrant {
    /// The only monitor the grant can see, read-only.
    pub fn scope(&self) -> MonitorScope {
        MonitorScope::Restricted(HashMap::from([(self.resource.monitor_id(), Level::View)]))
    }
}

/// The monitor or event a streaming route serves, from its path template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteResource {
    Monitor(u32),
    Event(u64),
}

fn param<'a>(params: &'a RawPathParams, name: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// `/api/v3/live/{monitor_id}/…` and `/api/v3/monitors/{monitor_id}/…` serve
/// a monitor; `/api/v3/events/{id}/…` an event. Anything else is not a route
/// a signed URL can open.
pub fn route_resource(
    template: &str,
    monitor_id: Option<&str>,
    id: Option<&str>,
) -> Option<RouteResource> {
    if template.starts_with("/api/v3/live/{monitor_id}")
        || template.starts_with("/api/v3/monitors/{monitor_id}")
    {
        return monitor_id?.parse().ok().map(RouteResource::Monitor);
    }
    if template.starts_with("/api/v3/events/{id}") {
        return id?.parse().ok().map(RouteResource::Event);
    }
    None
}

/// Whether a token signed for `signed` opens `route`.
pub fn opens(signed: MediaResource, route: RouteResource) -> bool {
    match (signed, route) {
        (MediaResource::Monitor(a), RouteResource::Monitor(b)) => a == b,
        (MediaResource::Event { event_id, .. }, RouteResource::Event(b)) => event_id == b,
        _ => false,
    }
}

/// The feature a resource's media belongs to.
fn feature_for(resource: MediaResource) -> Feature {
    match resource {
        MediaResource::Monitor(_) => Feature::Stream,
        MediaResource::Event { .. } => Feature::Events,
    }
}

/// The value of query parameter `name`, decoded.
pub fn query_param(uri: &Uri, name: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name)
            .then(|| urlencoding::decode(v).ok().map(|v| v.into_owned()))
            .flatten()
            .filter(|v| !v.is_empty())
    })
}

fn unauthorized(msg: &str) -> AppError {
    AppError::UnauthorizedError(msg.to_string())
}

/// Check a signed token for `route` and the user it names. Not found for a
/// monitor the user can't see, matching the other row-level checks.
pub async fn authorize(
    state: &AppState,
    token: &str,
    route: RouteResource,
) -> AppResult<MediaGrant> {
    let signed =
        media_url::verify(token).map_err(|_| unauthorized("Invalid or expired signed URL"))?;
    if !opens(signed.resource, route) {
        return Err(AppError::PermissionDeniedError(
            "Signed URL is for another resource".to_string(),
        ));
    }
    check_viewer(
        state,
        signed.uid,
        signed.iat,
        feature_for(signed.resource),
        signed.resource.monitor_id(),
    )
    .await?;
    Ok(MediaGrant {
        resource: signed.resource,
        uid: signed.uid,
        exp: signed.exp,
        token: token.to_string(),
    })
}

/// Whether user `uid`, holding a credential issued at `iat`, may still view
/// `monitor_id`'s media under `feature`: not revoked since `iat`, enabled,
/// `feature` View (their own level or their role's), and the monitor in their
/// scope. Not found for a monitor they can't see.
pub async fn check_viewer(
    state: &AppState,
    uid: u32,
    iat: i64,
    feature: Feature,
    monitor_id: u32,
) -> AppResult<()> {
    if state.revocations.is_revoked(uid, iat) {
        return Err(unauthorized("Signed URL has been revoked"));
    }
    let user = crate::repo::users::find_by_id(state.db(), uid)
        .await?
        .filter(|u| u.enabled != 0)
        .ok_or_else(|| unauthorized("Signed URL has been revoked"))?;
    let perms = crate::util::authz::effective_permissions(state.db(), &user).await?;
    if perms.level(feature) < Level::View {
        return Err(AppError::PermissionDeniedError(format!(
            "View access to {feature:?} required"
        )));
    }
    if !monitor_acl::resolve(state.db(), uid, perms.monitors)
        .await?
        .allows(monitor_id, Level::View)
    {
        return Err(AppError::NotFoundError(Resource {
            details: vec![("monitor_id".to_string(), monitor_id.to_string())],
            resource_type: ResourceType::Monitor,
        }));
    }
    Ok(())
}

/// Route layer for the streaming routes: authenticate `?st=` requests.
/// Requests without one pass through to the usual checks untouched.
pub async fn media_grant_layer(
    State(state): State<AppState>,
    matched: Option<MatchedPath>,
    params: RawPathParams,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let Some(token) = query_param(request.uri(), media_url::PARAM) else {
        // A login token in the URL (`?token=`, for `<img>` and `<video>`) is
        // a URL credential too: keep its response out of shared caches.
        let in_url = query_param(request.uri(), "token").is_some();
        let mut response = next.run(request).await;
        if in_url {
            harden(response.headers_mut());
        }
        return Ok(response);
    };
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return Err(AppError::PermissionDeniedError(
            "Signed URLs are read-only".to_string(),
        ));
    }
    let route = matched
        .as_ref()
        .and_then(|m| {
            route_resource(
                m.as_str(),
                param(&params, "monitor_id"),
                param(&params, "id"),
            )
        })
        .ok_or_else(|| {
            AppError::PermissionDeniedError("Signed URLs do not open this route".to_string())
        })?;
    let grant = authorize(&state, &token, route).await?;
    request.extensions_mut().insert(grant);
    let mut response = next.run(request).await;
    harden(response.headers_mut());
    Ok(response)
}

/// A response to a URL-credentialed request must not be stored by a shared
/// cache (which would replay it without the credential) or leak the URL to
/// another site through `Referer`.
fn harden(headers: &mut axum::http::HeaderMap) {
    use axum::http::header::{CACHE_CONTROL, REFERRER_POLICY};
    use axum::http::HeaderValue;
    let cache = headers
        .get(CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let private = match cache {
        Some(c) if c.contains("private") || c.contains("no-store") => None,
        Some(c) => Some(format!(
            "private, {}",
            c.replace("public, ", "").replace("public", "")
        )),
        None => Some("private".to_string()),
    };
    if let Some(v) =
        private.and_then(|v| HeaderValue::from_str(v.trim_end_matches([',', ' '])).ok())
    {
        headers.insert(CACHE_CONTROL, v);
    }
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
}

/// How to sign the URIs of a playlist served for `resource`, or `None` for a
/// header-authenticated request (whose player sends the header to every URI
/// itself). Media URIs live at most `ttl`, and never past the credential the
/// playlist was fetched with.
pub fn uri_signer(
    grant: Option<&MediaGrant>,
    uri: &Uri,
    resource: MediaResource,
    ttl: Duration,
) -> Option<UriSigner> {
    let now = crate::util::time::unix_now();
    let media_exp = |cred_exp: i64| (now + ttl.as_secs() as i64).min(cred_exp);
    if let Some(grant) = grant {
        return Some(UriSigner {
            playlist: grant.token.clone(),
            media: media_url::sign_until(resource, grant.uid, media_exp(grant.exp)),
        });
    }
    // A legacy `?token=<access JWT>` request: list signed URIs instead of
    // copying the access token into them. Child playlists last as long as
    // the token did.
    let claims = UserClaims::decode_access(&query_param(uri, "token")?).ok()?;
    Some(UriSigner {
        playlist: media_url::sign_until(resource, claims.uid, claims.exp),
        media: media_url::sign_until(resource, claims.uid, media_exp(claims.exp)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_routes_map_to_their_resource() {
        assert_eq!(
            route_resource("/api/v3/live/{monitor_id}/hls/{segment}", Some("4"), None),
            Some(RouteResource::Monitor(4))
        );
        assert_eq!(
            route_resource("/api/v3/monitors/{monitor_id}/snapshot", Some("4"), None),
            Some(RouteResource::Monitor(4))
        );
        assert_eq!(
            route_resource("/api/v3/events/{id}/stream/segment/{seq}", None, Some("77")),
            Some(RouteResource::Event(77))
        );
        assert_eq!(route_resource("/api/v3/events/synopsis", None, None), None);
        assert_eq!(route_resource("/api/v3/live/sessions", None, None), None);
        assert_eq!(
            route_resource("/api/v3/snapshots/{id}", None, Some("1")),
            None
        );
    }

    #[test]
    fn a_token_opens_only_what_it_was_signed_for() {
        let live = MediaResource::Monitor(4);
        let rec = MediaResource::Event {
            event_id: 77,
            monitor_id: 4,
        };
        assert!(opens(live, RouteResource::Monitor(4)));
        assert!(!opens(live, RouteResource::Monitor(5)));
        // A monitor's live token does not open that monitor's recordings.
        assert!(!opens(live, RouteResource::Event(77)));
        assert!(opens(rec, RouteResource::Event(77)));
        assert!(!opens(rec, RouteResource::Event(78)));
        assert!(!opens(rec, RouteResource::Monitor(4)));
    }

    #[test]
    fn a_legacy_token_playlist_lists_signatures_not_the_access_token() {
        let jwt = crate::service::token::generate_tokens(
            "viewer".to_string(),
            7,
            crate::util::authz::UserPermissions::default(),
        )
        .unwrap()
        .access_token;
        let uri: Uri = format!("/api/v3/live/4/hls/master.m3u8?token={jwt}")
            .parse()
            .unwrap();
        let signer = uri_signer(None, &uri, MediaResource::Monitor(4), media_url::LIVE_TTL)
            .expect("a URL credential gets signed URIs");
        for st in [&signer.playlist, &signer.media] {
            assert!(!st.contains(&jwt));
            let g = media_url::verify(st).expect("valid signature");
            assert_eq!((g.resource, g.uid), (MediaResource::Monitor(4), 7));
        }
        // Media signatures are short; child playlists outlive them.
        let media = media_url::verify(&signer.media).unwrap();
        let playlist = media_url::verify(&signer.playlist).unwrap();
        assert!(media.exp <= playlist.exp);
        assert!(media.exp - media.iat <= media_url::LIVE_TTL.as_secs() as i64);

        // A header-authenticated request lists bare URIs.
        let bare: Uri = "/api/v3/live/4/hls/master.m3u8".parse().unwrap();
        assert!(uri_signer(None, &bare, MediaResource::Monitor(4), media_url::LIVE_TTL).is_none());
    }

    #[test]
    fn credentialed_responses_are_private_and_send_no_referrer() {
        use axum::http::header::{CACHE_CONTROL, REFERRER_POLICY};
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            CACHE_CONTROL,
            "max-age=31536000, immutable".parse().unwrap(),
        );
        harden(&mut h);
        assert_eq!(h[CACHE_CONTROL], "private, max-age=31536000, immutable");
        assert_eq!(h[REFERRER_POLICY], "no-referrer");

        let mut h = axum::http::HeaderMap::new();
        h.insert(
            CACHE_CONTROL,
            "no-cache, no-store, must-revalidate".parse().unwrap(),
        );
        harden(&mut h);
        assert_eq!(h[CACHE_CONTROL], "no-cache, no-store, must-revalidate");

        let mut h = axum::http::HeaderMap::new();
        harden(&mut h);
        assert_eq!(h[CACHE_CONTROL], "private");
    }

    #[test]
    fn query_params_are_found_and_decoded() {
        let uri: Uri = "/x?_HLS_msn=3&st=1.m.2%2E&token=".parse().unwrap();
        assert_eq!(query_param(&uri, "st").as_deref(), Some("1.m.2."));
        assert_eq!(query_param(&uri, "token"), None);
        assert_eq!(query_param(&uri, "nope"), None);
    }
}
