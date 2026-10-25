//! HTTP handlers for ONVIF camera discovery.
//!
//! These are the thin Axum adapters over [`crate::service::discovery`]:
//!
//! - `POST /api/v3/discovery/probe` — run WS-Discovery on the local network and
//!   return the discovered [`CameraCandidate`]s.
//! - `POST /api/v3/discovery/inspect` — query a single device's Device + Media
//!   services and return its identity, profiles, and resolved stream URIs.
//!
//! Both endpoints are server-side outbound-request sinks (probe multicasts;
//! inspect makes directed HTTP calls to a caller-supplied URL), so they carry
//! the same access posture as creating a monitor: a row-restricted caller must
//! not be able to enumerate or interrogate cameras on the LAN, only an
//! unrestricted (admin-equivalent) caller may. The SSRF gates on the `inspect`
//! target itself live in the service layer.

use axum::extract::State;
use axum::Json;
use garde::Validate;
use tracing::{info, instrument, warn};

use crate::dto::request::discovery::{InspectRequest, OnboardRequest, ProbeRequest};
use crate::dto::response::MonitorResponse;
use crate::error::AppError;
use crate::error::AppResponseError;
use crate::error::AppResult;
use crate::onvif::types::Credentials;
use crate::server::state::AppState;
use crate::service;
use crate::service::discovery::{CameraCandidate, InspectProfile, InspectResult};
use crate::service::monitor_acl::MonitorScope;

/// OpenAPI fragment for the ONVIF discovery endpoints.
///
/// Kept separate from the main [`crate::handlers::openapi::ApiDoc`] (which
/// cannot `#[cfg]` individual macro entries) and merged into the served
/// document at runtime only when the `onvif-discovery` feature is enabled — see
/// `crate::routes::create_router_app`.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(probe, inspect, onboard),
    components(schemas(
        ProbeRequest,
        InspectRequest,
        OnboardRequest,
        CameraCandidate,
        InspectResult,
        InspectProfile,
        MonitorResponse,
    )),
    tags((name = "Discovery", description = "ONVIF camera discovery endpoints"))
)]
pub struct DiscoveryApiDoc;

/// Run a WS-Discovery probe and return the discovered ONVIF cameras.
///
/// - Requires a valid JWT and unrestricted monitor access (discovery is a
///   whole-network operation with no per-monitor row to scope against).
#[utoipa::path(
    post,
    path = "/api/v3/discovery/probe",
    request_body = ProbeRequest,
    responses(
        (status = 200, description = "Discovered camera candidates", body = Vec<CameraCandidate>),
        (status = 400, description = "Invalid request data", body = AppResponseError),
        (status = 401, description = "Unauthorized - Invalid or missing token", body = AppResponseError),
        (status = 403, description = "Caller's monitor access is restricted", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    ),
    tag = "Discovery"
)]
#[instrument(level = "debug", skip(state, req))]
pub async fn probe(
    State(state): State<AppState>,
    scope: MonitorScope,
    Json(req): Json<ProbeRequest>,
) -> AppResult<Json<Vec<CameraCandidate>>> {
    req.validate().map_err(AppError::InvalidInputError)?;
    // Row-level ACL: discovery surfaces every ONVIF device on the LAN and is a
    // stepping stone to minting monitors, so it is restricted to unrestricted
    // (admin-equivalent) callers — mirroring `monitor::create_monitor`.
    // Feature-level RBAC is enforced by the route's `protect` layer.
    if scope.is_restricted() {
        return Err(AppError::PermissionDeniedError(
            "running discovery requires unrestricted monitor access".to_string(),
        ));
    }
    info!("Running ONVIF discovery probe.");
    let timeout = std::time::Duration::from_millis(req.timeout_ms);
    match service::discovery::probe(&state, timeout).await {
        Ok(candidates) => Ok(Json(candidates)),
        Err(e) => {
            warn!("Failed to run discovery probe: {e:?}.");
            Err(e)
        }
    }
}

/// Inspect a single ONVIF device by its device-service XAddr.
///
/// - Requires a valid JWT and unrestricted monitor access.
/// - The target URL is gated against SSRF in the service layer.
#[utoipa::path(
    post,
    path = "/api/v3/discovery/inspect",
    request_body = InspectRequest,
    responses(
        (status = 200, description = "Inspected device details", body = InspectResult),
        (status = 400, description = "Invalid request data", body = AppResponseError),
        (status = 401, description = "Unauthorized - Invalid or missing token, or device rejected credentials", body = AppResponseError),
        (status = 403, description = "Caller restricted, or target address not permitted", body = AppResponseError),
        (status = 503, description = "ONVIF device unavailable or timed out", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    ),
    tag = "Discovery"
)]
#[instrument(level = "debug", skip(state, req), fields(xaddr = %req.xaddr))]
pub async fn inspect(
    State(state): State<AppState>,
    scope: MonitorScope,
    Json(req): Json<InspectRequest>,
) -> AppResult<Json<InspectResult>> {
    req.validate().map_err(AppError::InvalidInputError)?;
    if scope.is_restricted() {
        return Err(AppError::PermissionDeniedError(
            "inspecting devices requires unrestricted monitor access".to_string(),
        ));
    }
    // An empty username means the device is queried unauthenticated.
    let creds = if req.username.is_empty() {
        None
    } else {
        Some(Credentials::new(req.username, req.password))
    };
    info!("Inspecting ONVIF device.");
    match service::discovery::inspect(&state, &req.xaddr, creds).await {
        Ok(result) => Ok(Json(result)),
        Err(e) => {
            warn!("Failed to inspect ONVIF device: {e:?}.");
            Err(e)
        }
    }
}

/// Onboard a discovered ONVIF device as a new monitor.
///
/// Inspects the device, picks a media profile's RTSP stream URI, and creates an
/// `Ffmpeg` monitor with the device's ONVIF URL + credentials. Requires
/// Monitors Create (or System Edit), as `POST /monitors` does, and
/// unrestricted monitor access; the target URL is SSRF-gated in the service
/// layer.
#[utoipa::path(
    post,
    path = "/api/v3/discovery/onboard",
    request_body = OnboardRequest,
    responses(
        (status = 200, description = "Monitor created from the discovered device", body = MonitorResponse),
        (status = 400, description = "Invalid request or no usable RTSP profile", body = AppResponseError),
        (status = 401, description = "Unauthorized, or device rejected credentials", body = AppResponseError),
        (status = 403, description = "Caller restricted, or target address not permitted", body = AppResponseError),
        (status = 503, description = "ONVIF device unavailable or timed out", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(("jwt" = [])),
    tag = "Discovery"
)]
#[instrument(level = "debug", skip(state, req), fields(xaddr = %req.xaddr))]
pub async fn onboard(
    State(state): State<AppState>,
    claims: crate::util::claim::UserClaims,
    scope: MonitorScope,
    Json(req): Json<OnboardRequest>,
) -> AppResult<Json<MonitorResponse>> {
    req.validate().map_err(AppError::InvalidInputError)?;
    // It adds a monitor: the same check as `POST /monitors`.
    if !claims.perms.can_create_monitors() {
        return Err(AppError::PermissionDeniedError(
            "Create access to Monitors required".to_string(),
        ));
    }
    if scope.is_restricted() {
        return Err(AppError::PermissionDeniedError(
            "onboarding a discovered device requires unrestricted monitor access".to_string(),
        ));
    }
    info!("Onboarding discovered ONVIF device as a monitor.");
    match service::discovery::onboard(&state, req).await {
        Ok(monitor) => Ok(Json(monitor)),
        Err(e) => {
            warn!("Failed to onboard ONVIF device: {e:?}.");
            Err(e)
        }
    }
}
