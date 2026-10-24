//! Native ONVIF PTZ control adapter.
//!
//! This is the integration layer between the generic PTZ trait surface
//! (`crate::ptz::traits`) and the reusable ONVIF client subsystem
//! (`crate::onvif`). [`OnvifControl`] implements [`PtzControl`] by translating
//! each [`PtzCommand`] into an ONVIF PTZ operation (`ContinuousMove`,
//! `AbsoluteMove`, `Stop`, …) and dispatching it through an
//! [`onvif::ptz::PtzClient`].
//!
//! ## Command mapping
//!
//! | `PtzCommand` family | ONVIF operation |
//! |---|---|
//! | `Move*` (directional) | `ContinuousMove` with a pan/tilt velocity vector |
//! | `ZoomIn` / `ZoomOut` | `ContinuousMove` with a zoom velocity vector |
//! | `Move{Zoom,Focus,…}Stop`, `MoveStop` | `Stop` |
//! | `MoveAbsolute` | `AbsoluteMove` with a position vector |
//! | `MoveRelative` | `RelativeMove` with a translation vector |
//!
//! Preset/power/focus/iris commands are not part of the core ONVIF PTZ service
//! surface modeled here and return [`PtzError::CommandNotSupported`]; callers
//! fall back to the Perl bridge for those via the registry.
//!
//! ## Coordinate / speed mapping
//!
//! ONVIF PTZ vectors are in the *generic normalized* space `[-1.0, 1.0]`. The
//! generic [`MoveParams`]/[`ZoomParams`] speeds are `0..=100` percentages; we
//! map a percentage `p` to a normalized magnitude `p / 100.0`, defaulting to
//! full speed (`1.0`) when no speed is supplied. Directional sign comes from the
//! command (e.g. `MoveLeft` → negative pan). [`AbsolutePosition`] /
//! [`RelativePosition`] values are assumed to already be in normalized ONVIF
//! space and are clamped to `[-1.0, 1.0]`.

use async_trait::async_trait;
use tracing::{debug, info, instrument};

use crate::onvif::error::OnvifError;
use crate::onvif::ptz::{PanTilt, PtzClient, PtzVector, Zoom};
use crate::onvif::transport::OnvifTransport;
use crate::onvif::types::Credentials;

use crate::ptz::capabilities::PtzCapabilities;
use crate::ptz::error::{PtzError, PtzResult};
use crate::ptz::traits::{
    AbsolutePosition, MoveParams, PtzCommand, PtzCommandResult, PtzConnectionConfig, PtzControl,
    PtzControlFactory, RelativePosition, ZoomParams,
};

/// Default ONVIF media profile token used to drive PTZ moves when the
/// connection config does not carry an explicit one. `Profile_1` is the
/// conventional first-profile token emitted by the vast majority of cameras.
const DEFAULT_PROFILE_TOKEN: &str = "Profile_1";

/// Native ONVIF PTZ controller for a single monitor.
///
/// Holds an [`onvif::ptz::PtzClient`] bound to the camera's PTZ-service XAddr
/// plus the media profile token that moves are issued against. Cheap to
/// construct; the underlying `reqwest` client is shared through the transport.
pub struct OnvifControl {
    /// Generic PTZ capability set (surfaced via [`PtzControl::capabilities`]).
    capabilities: PtzCapabilities,
    /// The ONVIF PTZ service client.
    client: PtzClient,
    /// Media profile token PTZ operations target.
    profile_token: String,
    /// Owning monitor id (for tracing context).
    monitor_id: u32,
}

impl OnvifControl {
    /// Build an [`OnvifControl`] from a generic [`PtzConnectionConfig`] and the
    /// camera's capability set.
    ///
    /// `config.address` is treated as the ONVIF **PTZ service XAddr** (the
    /// endpoint URL, e.g. `http://host/onvif/ptz_service`). Credentials are
    /// taken from `config.username` / `config.password`; when both are absent
    /// the client issues unauthenticated requests.
    pub fn new(config: PtzConnectionConfig, capabilities: PtzCapabilities) -> Self {
        // No redirects: the address was checked, a redirect's target wasn't.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self::with_transport(OnvifTransport::new(client), config, capabilities)
    }

    /// Build an [`OnvifControl`] over a caller-supplied [`OnvifTransport`],
    /// letting the connection pool / TLS config be shared with the rest of the
    /// ONVIF subsystem.
    pub fn with_transport(
        transport: OnvifTransport,
        config: PtzConnectionConfig,
        capabilities: PtzCapabilities,
    ) -> Self {
        let creds = match (config.username.as_ref(), config.password.as_ref()) {
            (Some(u), Some(p)) => Some(Credentials::new(u.clone(), p.clone())),
            // A username with no password (or vice versa) is still a partial
            // credential; pass it through so the device can reject it clearly.
            (Some(u), None) => Some(Credentials::new(u.clone(), String::new())),
            (None, Some(p)) => Some(Credentials::new(String::new(), p.clone())),
            (None, None) => None,
        };

        let client = PtzClient::new(transport, config.address.clone(), creds);

        Self {
            capabilities,
            client,
            profile_token: DEFAULT_PROFILE_TOKEN.to_string(),
            monitor_id: config.monitor_id,
        }
    }

    /// Override the media profile token PTZ operations target (defaults to
    /// [`DEFAULT_PROFILE_TOKEN`]). Returns `self` for builder-style chaining.
    pub fn with_profile_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        if !token.trim().is_empty() {
            self.profile_token = token;
        }
        self
    }

    /// Issue a `ContinuousMove` for a directional pan/tilt command.
    async fn continuous_pan_tilt(
        &self,
        params: &MoveParams,
        pan_dir: i8,
        tilt_dir: i8,
    ) -> PtzResult<PtzCommandResult> {
        let pan = pan_dir as f32 * speed_to_velocity(params.pan_speed);
        let tilt = tilt_dir as f32 * speed_to_velocity(params.tilt_speed);
        let velocity = PtzVector::pan_tilt(pan, tilt);
        self.client
            .continuous_move(&self.profile_token, velocity)
            .await
            .map_err(map_onvif_error)?;
        Ok(PtzCommandResult::success("ContinuousMove (pan/tilt) sent"))
    }

    /// Issue a `ContinuousMove` for a zoom command.
    async fn continuous_zoom(
        &self,
        params: &ZoomParams,
        zoom_dir: i8,
    ) -> PtzResult<PtzCommandResult> {
        let z = zoom_dir as f32 * speed_to_velocity(params.speed);
        let velocity = PtzVector::zoom(z);
        self.client
            .continuous_move(&self.profile_token, velocity)
            .await
            .map_err(map_onvif_error)?;
        Ok(PtzCommandResult::success("ContinuousMove (zoom) sent"))
    }

    /// Issue an `AbsoluteMove` to a normalized position.
    async fn absolute(&self, pos: &AbsolutePosition) -> PtzResult<PtzCommandResult> {
        let vector = position_to_vector(pos);
        if vector.pan_tilt.is_none() && vector.zoom.is_none() {
            return Err(PtzError::InvalidParameter(
                "MoveAbsolute requires at least one of pan/tilt/zoom".to_string(),
            ));
        }
        self.client
            .absolute_move(&self.profile_token, vector)
            .await
            .map_err(map_onvif_error)?;
        Ok(PtzCommandResult::success("AbsoluteMove sent"))
    }

    /// Issue a `RelativeMove` by a normalized translation.
    async fn relative(&self, pos: &RelativePosition) -> PtzResult<PtzCommandResult> {
        let vector = translation_to_vector(pos);
        if vector.pan_tilt.is_none() && vector.zoom.is_none() {
            return Err(PtzError::InvalidParameter(
                "MoveRelative requires at least one of pan/tilt/zoom delta".to_string(),
            ));
        }
        self.client
            .relative_move(&self.profile_token, vector)
            .await
            .map_err(map_onvif_error)?;
        Ok(PtzCommandResult::success("RelativeMove sent"))
    }

    /// Issue a `Stop`, selecting which axes to halt.
    async fn stop(&self, pan_tilt: bool, zoom: bool) -> PtzResult<PtzCommandResult> {
        self.client
            .stop(&self.profile_token, pan_tilt, zoom)
            .await
            .map_err(map_onvif_error)?;
        Ok(PtzCommandResult::success("Stop sent"))
    }
}

#[async_trait]
impl PtzControl for OnvifControl {
    fn capabilities(&self) -> &PtzCapabilities {
        &self.capabilities
    }

    fn protocol_name(&self) -> &str {
        "onvif"
    }

    fn is_native(&self) -> bool {
        true
    }

    #[instrument(skip(self), fields(monitor_id = self.monitor_id, protocol = "onvif", command = ?command))]
    async fn execute(&self, command: PtzCommand) -> PtzResult<PtzCommandResult> {
        debug!("dispatching ONVIF PTZ command");
        let result = match &command {
            // ---- Directional continuous pan/tilt -> ContinuousMove ---------
            PtzCommand::MoveUp(p) => self.continuous_pan_tilt(p, 0, 1).await,
            PtzCommand::MoveDown(p) => self.continuous_pan_tilt(p, 0, -1).await,
            PtzCommand::MoveLeft(p) => self.continuous_pan_tilt(p, -1, 0).await,
            PtzCommand::MoveRight(p) => self.continuous_pan_tilt(p, 1, 0).await,
            PtzCommand::MoveUpLeft(p) => self.continuous_pan_tilt(p, -1, 1).await,
            PtzCommand::MoveUpRight(p) => self.continuous_pan_tilt(p, 1, 1).await,
            PtzCommand::MoveDownLeft(p) => self.continuous_pan_tilt(p, -1, -1).await,
            PtzCommand::MoveDownRight(p) => self.continuous_pan_tilt(p, 1, -1).await,

            // ---- Continuous zoom -> ContinuousMove -------------------------
            PtzCommand::ZoomIn(p) => self.continuous_zoom(p, 1).await,
            PtzCommand::ZoomOut(p) => self.continuous_zoom(p, -1).await,

            // ---- Stops -> Stop --------------------------------------------
            // A bare MoveStop halts every axis; the axis-specific stops halt
            // only their own axis so an in-flight move on the other continues.
            PtzCommand::MoveStop => self.stop(true, true).await,
            PtzCommand::ZoomStop => self.stop(false, true).await,

            // ---- Absolute / relative positioning --------------------------
            PtzCommand::MoveAbsolute(pos) => self.absolute(pos).await,
            PtzCommand::MoveRelative(pos) => self.relative(pos).await,

            // ---- Not modeled by the core ONVIF PTZ surface ----------------
            other => Err(PtzError::CommandNotSupported(format!(
                "ONVIF native adapter does not support {}",
                other.zmcontrol_command()
            ))),
        };

        if let Ok(ref r) = result {
            info!(monitor_id = self.monitor_id, message = %r.message, "ONVIF PTZ command executed");
        }
        result
    }
}

/// Map a generic `0..=100` percentage speed to a normalized ONVIF velocity
/// magnitude in `[0.0, 1.0]`. A missing speed defaults to full speed.
fn speed_to_velocity(speed: Option<u8>) -> f32 {
    match speed {
        Some(p) => (p.min(100) as f32) / 100.0,
        None => 1.0,
    }
}

/// Clamp an `f64` to the normalized ONVIF range and narrow to `f32`.
fn clamp_norm(v: f64) -> f32 {
    v.clamp(-1.0, 1.0) as f32
}

/// Build a [`PtzVector`] from an [`AbsolutePosition`], including only the axes
/// the caller actually specified.
fn position_to_vector(pos: &AbsolutePosition) -> PtzVector {
    let pan_tilt = match (pos.pan, pos.tilt) {
        (None, None) => None,
        (pan, tilt) => Some(PanTilt::new(
            clamp_norm(pan.unwrap_or(0.0)),
            clamp_norm(tilt.unwrap_or(0.0)),
        )),
    };
    PtzVector {
        pan_tilt,
        zoom: pos.zoom.map(|z| Zoom::new(clamp_norm(z))),
    }
}

/// Build a [`PtzVector`] from a [`RelativePosition`] translation, including only
/// the axes the caller actually specified.
fn translation_to_vector(pos: &RelativePosition) -> PtzVector {
    let pan_tilt = match (pos.pan_delta, pos.tilt_delta) {
        (None, None) => None,
        (pan, tilt) => Some(PanTilt::new(
            clamp_norm(pan.unwrap_or(0.0)),
            clamp_norm(tilt.unwrap_or(0.0)),
        )),
    };
    PtzVector {
        pan_tilt,
        zoom: pos.zoom_delta.map(|z| Zoom::new(clamp_norm(z))),
    }
}

/// Translate an [`OnvifError`] into the generic [`PtzError`] surface so the PTZ
/// manager/handlers can map it to the appropriate HTTP status.
fn map_onvif_error(err: OnvifError) -> PtzError {
    match err {
        OnvifError::Auth => {
            PtzError::AuthenticationFailed("ONVIF authentication failed".to_string())
        }
        OnvifError::Soap { code, reason } => {
            // ONVIF surfaces "not authorized" as a SOAP fault; classify it so
            // the caller gets a 401 rather than a generic protocol error.
            let lc = format!("{code} {reason}").to_lowercase();
            if lc.contains("notauthorized") || lc.contains("not authorized") {
                PtzError::AuthenticationFailed(reason)
            } else if lc.contains("actionnotsupported") || lc.contains("not supported") {
                PtzError::CommandNotSupported(reason)
            } else {
                PtzError::ProtocolError(format!("SOAP fault {code}: {reason}"))
            }
        }
        OnvifError::Timeout => PtzError::CommandTimeout("ONVIF request timed out".to_string()),
        // The details (the URL, up to 512 bytes of whatever answered) go to
        // the log, not the caller: they would read back any address.
        OnvifError::Http(e) => {
            tracing::warn!(error = %e, "ONVIF PTZ transport error");
            PtzError::CameraOffline("the camera didn't answer".to_string())
        }
        OnvifError::Parse(msg) => {
            tracing::warn!(error = %msg, "ONVIF PTZ response didn't parse");
            PtzError::ProtocolError("the camera's answer wasn't ONVIF".to_string())
        }
        OnvifError::Discovery(msg) => {
            PtzError::ProtocolError(format!("ONVIF discovery error: {msg}"))
        }
    }
}

/// Factory that produces native ONVIF [`OnvifControl`] instances.
///
/// Registered in the PTZ registry as the native handler for the `onvif`
/// protocol; the registry falls back to the Perl bridge for everything else.
#[derive(Debug, Default)]
pub struct OnvifControlFactory;

impl OnvifControlFactory {
    /// Construct a new factory.
    pub fn new() -> Self {
        Self
    }
}

impl PtzControlFactory for OnvifControlFactory {
    fn protocol_name(&self) -> &str {
        "onvif"
    }

    fn is_native(&self) -> bool {
        true
    }

    fn create(
        &self,
        config: PtzConnectionConfig,
        capabilities: PtzCapabilities,
    ) -> Box<dyn PtzControl> {
        Box::new(OnvifControl::new(config, capabilities))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> PtzConnectionConfig {
        PtzConnectionConfig {
            monitor_id: 7,
            address: "http://192.168.1.10/onvif/ptz_service".to_string(),
            username: Some("admin".to_string()),
            password: Some("secret".to_string()),
            protocol: "onvif".to_string(),
            auto_stop_timeout: Some(5.0),
        }
    }

    #[test]
    fn factory_is_native_onvif() {
        let f = OnvifControlFactory::new();
        assert_eq!(f.protocol_name(), "onvif");
        assert!(f.is_native());
    }

    #[test]
    fn control_reports_native_onvif() {
        let ctrl = OnvifControl::new(test_config(), PtzCapabilities::default());
        assert_eq!(ctrl.protocol_name(), "onvif");
        assert!(ctrl.is_native());
        assert_eq!(ctrl.profile_token, DEFAULT_PROFILE_TOKEN);
    }

    #[test]
    fn factory_creates_control() {
        let f = OnvifControlFactory::new();
        let ctrl = f.create(test_config(), PtzCapabilities::default());
        assert_eq!(ctrl.protocol_name(), "onvif");
        assert!(ctrl.is_native());
    }

    #[test]
    fn profile_token_override_ignores_blank() {
        let ctrl =
            OnvifControl::new(test_config(), PtzCapabilities::default()).with_profile_token("   ");
        assert_eq!(ctrl.profile_token, DEFAULT_PROFILE_TOKEN);

        let ctrl = OnvifControl::new(test_config(), PtzCapabilities::default())
            .with_profile_token("MainP");
        assert_eq!(ctrl.profile_token, "MainP");
    }

    #[test]
    fn missing_password_still_builds_partial_credentials() {
        let mut cfg = test_config();
        cfg.password = None;
        // Should not panic; partial credentials are constructed.
        let _ctrl = OnvifControl::new(cfg, PtzCapabilities::default());
    }

    #[test]
    fn speed_mapping_full_and_percentage() {
        assert!((speed_to_velocity(None) - 1.0).abs() < 1e-6);
        assert!((speed_to_velocity(Some(0)) - 0.0).abs() < 1e-6);
        assert!((speed_to_velocity(Some(50)) - 0.5).abs() < 1e-6);
        assert!((speed_to_velocity(Some(100)) - 1.0).abs() < 1e-6);
        // Over-100 is clamped to full speed.
        assert!((speed_to_velocity(Some(200)) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn clamp_norm_bounds_range() {
        assert!((clamp_norm(2.0) - 1.0).abs() < 1e-6);
        assert!((clamp_norm(-2.0) - (-1.0)).abs() < 1e-6);
        assert!((clamp_norm(0.25) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn position_to_vector_includes_only_specified_axes() {
        let pan_only = position_to_vector(&AbsolutePosition {
            pan: Some(0.5),
            tilt: None,
            zoom: None,
        });
        assert!(pan_only.pan_tilt.is_some());
        assert!(pan_only.zoom.is_none());
        let pt = pan_only.pan_tilt.unwrap();
        assert!((pt.x - 0.5).abs() < 1e-6);
        assert!((pt.y - 0.0).abs() < 1e-6);

        let zoom_only = position_to_vector(&AbsolutePosition {
            pan: None,
            tilt: None,
            zoom: Some(1.5), // clamped
        });
        assert!(zoom_only.pan_tilt.is_none());
        assert_eq!(zoom_only.zoom, Some(Zoom::new(1.0)));

        let empty = position_to_vector(&AbsolutePosition::default());
        assert!(empty.pan_tilt.is_none());
        assert!(empty.zoom.is_none());
    }

    #[test]
    fn translation_to_vector_includes_only_specified_axes() {
        let v = translation_to_vector(&RelativePosition {
            pan_delta: None,
            tilt_delta: Some(-0.3),
            zoom_delta: Some(0.2),
        });
        let pt = v
            .pan_tilt
            .expect("tilt delta present implies pan/tilt vector");
        assert!((pt.x - 0.0).abs() < 1e-6);
        assert!((pt.y - (-0.3)).abs() < 1e-6);
        assert_eq!(v.zoom, Some(Zoom::new(0.2)));
    }

    #[test]
    fn onvif_error_maps_to_ptz_error() {
        assert!(matches!(
            map_onvif_error(OnvifError::Auth),
            PtzError::AuthenticationFailed(_)
        ));
        assert!(matches!(
            map_onvif_error(OnvifError::Timeout),
            PtzError::CommandTimeout(_)
        ));
        assert!(matches!(
            map_onvif_error(OnvifError::Soap {
                code: "ter:NotAuthorized".to_string(),
                reason: "Sender not authorized".to_string(),
            }),
            PtzError::AuthenticationFailed(_)
        ));
        assert!(matches!(
            map_onvif_error(OnvifError::Soap {
                code: "ter:ActionNotSupported".to_string(),
                reason: "Optional Action Not Supported".to_string(),
            }),
            PtzError::CommandNotSupported(_)
        ));
        assert!(matches!(
            map_onvif_error(OnvifError::Soap {
                code: "ter:Other".to_string(),
                reason: "boom".to_string(),
            }),
            PtzError::ProtocolError(_)
        ));
        assert!(matches!(
            map_onvif_error(OnvifError::Parse("bad xml".to_string())),
            PtzError::ProtocolError(_)
        ));
    }

    #[tokio::test]
    async fn unsupported_command_reports_not_supported() {
        let ctrl = OnvifControl::new(test_config(), PtzCapabilities::default());
        let res = ctrl.execute(PtzCommand::GotoPreset { preset_id: 3 }).await;
        assert!(matches!(res, Err(PtzError::CommandNotSupported(_))));
    }

    #[tokio::test]
    async fn absolute_requires_an_axis() {
        let ctrl = OnvifControl::new(test_config(), PtzCapabilities::default());
        let res = ctrl
            .execute(PtzCommand::MoveAbsolute(AbsolutePosition::default()))
            .await;
        assert!(matches!(res, Err(PtzError::InvalidParameter(_))));
    }
}
