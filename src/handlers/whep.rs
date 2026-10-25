//! WHEP endpoints (RFC 9725) for a monitor's live WebRTC: the viewer sends
//! the offer. See [`crate::streaming::live::whep`].

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;

use crate::server::state::AppState;
use crate::streaming::live::whep::{WhepError, WhepSessions};
use crate::util::claim::UserClaims;

const SDP: &str = "application/sdp";
const SDPFRAG: &str = "application/trickle-ice-sdpfrag";

fn content_type_is(headers: &HeaderMap, want: &str) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(want))
}

fn plain(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain")], msg.into()).into_response()
}

fn error(e: WhepError) -> Response {
    let status = match e {
        WhepError::NotFound(_) => StatusCode::NOT_FOUND,
        WhepError::BadOffer(_) => StatusCode::BAD_REQUEST,
        WhepError::Codec(_) => StatusCode::NOT_ACCEPTABLE,
        WhepError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        WhepError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    plain(status, e.to_string())
}

/// Start a WHEP session: the body is the viewer's SDP offer
///
/// Answers `201 Created` with the SDP answer (this side's candidates
/// included) and the session's URL in `Location`. `406` when the offer can't
/// receive the camera's codec (an H.265 camera, a browser without H.265).
#[utoipa::path(
    post,
    path = "/api/v3/live/{monitor_id}/webrtc/whep",
    operation_id = "whepOffer",
    tag = "Live Streaming",
    params(("monitor_id" = u32, Path, description = "Monitor/Camera ID")),
    request_body(content = String, content_type = "application/sdp", description = "SDP offer"),
    responses(
        (status = 201, description = "SDP answer; session URL in Location", content_type = "application/sdp", body = String),
        (status = 400, description = "Not a usable SDP offer"),
        (status = 404, description = "No stream for the monitor"),
        (status = 406, description = "The offer can't receive the camera's codec"),
        (status = 415, description = "Body is not application/sdp"),
        (status = 503, description = "Streaming off or session limit reached")
    ),
    security(("jwt" = []))
)]
pub async fn whep_offer(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    Extension(claims): Extension<UserClaims>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !content_type_is(&headers, SDP) {
        return plain(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Send the offer as application/sdp",
        );
    }
    match WhepSessions::start(&state, monitor_id, claims.uid, body).await {
        Ok((id, answer)) => Response::builder()
            .status(StatusCode::CREATED)
            .header(header::CONTENT_TYPE, SDP)
            .header(
                header::LOCATION,
                crate::configure::server::public_path(&format!(
                    "/api/v3/live/{monitor_id}/webrtc/whep/{id}"
                )),
            )
            .body(Body::from(answer))
            .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "response")),
        Err(e) => error(e),
    }
}

/// Send the viewer's trickled ICE candidates to a WHEP session
#[utoipa::path(
    patch,
    path = "/api/v3/live/{monitor_id}/webrtc/whep/{session_id}",
    operation_id = "whepTrickle",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID"),
        ("session_id" = String, Path, description = "WHEP session")
    ),
    request_body(content = String, content_type = "application/trickle-ice-sdpfrag"),
    responses(
        (status = 204, description = "Candidates added"),
        (status = 404, description = "No such session"),
        (status = 415, description = "Body is not application/trickle-ice-sdpfrag")
    ),
    security(("jwt" = []))
)]
pub async fn whep_trickle(
    State(state): State<AppState>,
    Path((monitor_id, session_id)): Path<(u32, String)>,
    Extension(claims): Extension<UserClaims>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !content_type_is(&headers, SDPFRAG) {
        return plain(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Send candidates as application/trickle-ice-sdpfrag",
        );
    }
    match state
        .whep
        .add_candidates(&session_id, monitor_id, claims.uid, &body)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error(e),
    }
}

/// End a WHEP session
#[utoipa::path(
    delete,
    path = "/api/v3/live/{monitor_id}/webrtc/whep/{session_id}",
    operation_id = "whepDelete",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID"),
        ("session_id" = String, Path, description = "WHEP session")
    ),
    responses(
        (status = 200, description = "Session ended"),
        (status = 404, description = "No such session")
    ),
    security(("jwt" = []))
)]
pub async fn whep_delete(
    State(state): State<AppState>,
    Path((monitor_id, session_id)): Path<(u32, String)>,
    Extension(claims): Extension<UserClaims>,
) -> Response {
    match state.whep.stop(&session_id, monitor_id, claims.uid) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_match_without_parameters() {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_TYPE,
            "Application/SDP; charset=utf-8".parse().unwrap(),
        );
        assert!(content_type_is(&h, SDP));
        h.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        assert!(!content_type_is(&h, SDP));
        assert!(!content_type_is(&HeaderMap::new(), SDP));
    }
}
