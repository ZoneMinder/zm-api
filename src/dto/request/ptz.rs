//! PTZ request DTOs

use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::ptz::traits::{FocusParams, MoveParams, ZoomParams};

/// Request for continuous movement
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema, Validate)]
pub struct PtzMoveRequest {
    /// Pan speed (0-100 percent)
    #[garde(range(max = 100))]
    #[serde(default)]
    pub pan_speed: Option<u8>,

    /// Tilt speed (0-100 percent)
    #[garde(range(max = 100))]
    #[serde(default)]
    pub tilt_speed: Option<u8>,

    /// Duration in milliseconds (optional, for timed movements)
    #[garde(skip)]
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

impl From<PtzMoveRequest> for MoveParams {
    fn from(req: PtzMoveRequest) -> Self {
        Self {
            pan_speed: req.pan_speed,
            tilt_speed: req.tilt_speed,
            duration_ms: req.duration_ms,
            auto_stop: true,
        }
    }
}

/// Request for zoom operations
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema, Validate)]
pub struct PtzZoomRequest {
    /// Zoom speed (0-100 percent)
    #[garde(range(max = 100))]
    #[serde(default)]
    pub speed: Option<u8>,

    /// Duration in milliseconds
    #[garde(skip)]
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

impl From<PtzZoomRequest> for ZoomParams {
    fn from(req: PtzZoomRequest) -> Self {
        Self {
            speed: req.speed,
            duration_ms: req.duration_ms,
        }
    }
}

/// Request for focus operations
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema, Validate)]
pub struct PtzFocusRequest {
    /// Focus speed (0-100 percent)
    #[garde(range(max = 100))]
    #[serde(default)]
    pub speed: Option<u8>,

    /// Duration in milliseconds
    #[garde(skip)]
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

impl From<PtzFocusRequest> for FocusParams {
    fn from(req: PtzFocusRequest) -> Self {
        Self {
            speed: req.speed,
            duration_ms: req.duration_ms,
        }
    }
}

/// Request for preset operations
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, Validate)]
pub struct PtzPresetRequest {
    /// Optional name for the preset (when setting). The character set is
    /// restricted to keep names safe to log, store, and pass downstream as
    /// a single argv element to zmcontrol.pl: NUL truncates C strings,
    /// newlines enable log injection, and slashes have no business in a
    /// preset label. The first character may not be `-` either, since a
    /// leading hyphen makes the value indistinguishable from a flag to the
    /// Perl Getopt::Long parser on the receiving end. Any printable,
    /// non-control, non-slash character is OK after that, including Unicode
    /// letters (so non-Latin names still work).
    #[garde(length(max = 64))]
    #[garde(pattern(r"^([^\x00-\x1F\x7F/\\-][^\x00-\x1F\x7F/\\]*)?$"))]
    #[serde(default)]
    pub name: Option<String>,
}

/// Request for absolute positioning
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema, Validate)]
pub struct PtzAbsoluteRequest {
    /// Pan position. Through ZoneMinder's control daemon (a control with no
    /// native implementation) absolute moves answer 400: no ZoneMinder
    /// control module takes a position that way.
    #[garde(skip)]
    #[serde(default)]
    pub pan: Option<f64>,

    /// Tilt position
    #[garde(skip)]
    #[serde(default)]
    pub tilt: Option<f64>,

    /// Zoom position
    #[garde(skip)]
    #[serde(default)]
    pub zoom: Option<f64>,
}

/// Request for relative movement
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema, Validate)]
pub struct PtzRelativeRequest {
    /// Pan delta, positive right. Through ZoneMinder's control daemon it is a
    /// whole step in the control's pan step range, sent as `moveRel<Dir>`
    /// with `panstep`; with native ONVIF, a normalized translation.
    #[garde(skip)]
    #[serde(default)]
    pub pan_delta: Option<f64>,

    /// Tilt delta, positive up, in the same units as `pan_delta`.
    #[garde(skip)]
    #[serde(default)]
    pub tilt_delta: Option<f64>,

    /// Zoom delta, positive in (tele). Through ZoneMinder's control daemon,
    /// `zoomRelTele` / `zoomRelWide` with `step`, sent after any pan or tilt.
    #[garde(skip)]
    #[serde(default)]
    pub zoom_delta: Option<f64>,
}

/// Generic PTZ command request
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, Validate)]
pub struct PtzGenericCommandRequest {
    /// Command name (e.g., "moveConUp", "zoomIn")
    #[garde(length(min = 1, max = 64))]
    pub command: String,

    /// Optional parameters as key-value pairs
    #[garde(skip)]
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(name: Option<&str>) -> PtzPresetRequest {
        PtzPresetRequest {
            name: name.map(|s| s.to_string()),
        }
    }

    #[test]
    fn preset_name_accepts_normal_labels() {
        // Common preset names from real-world setups.
        for n in [
            "Front Door",
            "Driveway North",
            "Garden #3",
            "Position 1.5",
            "屋外カメラ",
            "Cam-01_west",
            "",
        ] {
            assert!(
                preset(Some(n)).validate().is_ok(),
                "expected {:?} to pass",
                n
            );
        }
        // None is always fine
        assert!(preset(None).validate().is_ok());
    }

    #[test]
    fn preset_name_rejects_control_chars_and_separators() {
        // Newlines/CR enable log injection
        assert!(preset(Some("Front\nDoor")).validate().is_err());
        assert!(preset(Some("Front\rDoor")).validate().is_err());
        // Tab is a control char (some logging stacks split on it)
        assert!(preset(Some("Front\tDoor")).validate().is_err());
        // NUL truncates C strings downstream
        assert!(preset(Some("Front\0Door")).validate().is_err());
        // DEL is a control char
        assert!(preset(Some("Front\x7FDoor")).validate().is_err());
        // Path separators don't belong in a label
        assert!(preset(Some("Front/Door")).validate().is_err());
        assert!(preset(Some("Front\\Door")).validate().is_err());
        // Path traversal attempt (uses both / and ..)
        assert!(preset(Some("../../etc/passwd")).validate().is_err());
    }

    #[test]
    fn preset_name_enforces_length_limit() {
        let too_long = "a".repeat(65);
        assert!(preset(Some(&too_long)).validate().is_err());
        let just_right = "a".repeat(64);
        assert!(preset(Some(&just_right)).validate().is_ok());
    }

    #[test]
    fn preset_name_rejects_leading_hyphen() {
        // A leading `-` makes the value look like a flag to the downstream
        // Perl Getopt::Long parser. Hyphens elsewhere are fine.
        assert!(preset(Some("-front")).validate().is_err());
        assert!(preset(Some("--filter_id=1")).validate().is_err());
        assert!(preset(Some("-")).validate().is_err());
        // Mid-string and trailing hyphens stay legal.
        assert!(preset(Some("Cam-01_west")).validate().is_ok());
        assert!(preset(Some("a-")).validate().is_ok());
    }
}
