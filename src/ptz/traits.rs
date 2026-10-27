//! PTZ control traits and command types

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::capabilities::PtzCapabilities;
use super::error::PtzResult;

/// Movement direction for continuous/relative movements
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MoveDirection {
    Up,
    Down,
    Left,
    Right,
    UpLeft,
    UpRight,
    DownLeft,
    DownRight,
}

impl MoveDirection {
    /// Get the pan component (-1, 0, or 1)
    pub fn pan_component(&self) -> i8 {
        match self {
            Self::Left | Self::UpLeft | Self::DownLeft => -1,
            Self::Right | Self::UpRight | Self::DownRight => 1,
            Self::Up | Self::Down => 0,
        }
    }

    /// Get the tilt component (-1, 0, or 1)
    pub fn tilt_component(&self) -> i8 {
        match self {
            Self::Up | Self::UpLeft | Self::UpRight => 1,
            Self::Down | Self::DownLeft | Self::DownRight => -1,
            Self::Left | Self::Right => 0,
        }
    }

    /// Check if this is a diagonal movement
    pub fn is_diagonal(&self) -> bool {
        matches!(
            self,
            Self::UpLeft | Self::UpRight | Self::DownLeft | Self::DownRight
        )
    }
}

/// Zoom direction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ZoomDirection {
    In,
    Out,
}

/// Focus direction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FocusDirection {
    Near,
    Far,
}

/// Iris direction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IrisDirection {
    Open,
    Close,
}

/// Parameters for movement commands
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct MoveParams {
    /// Pan speed (0-100, percentage of max speed)
    #[serde(default)]
    pub pan_speed: Option<u8>,

    /// Tilt speed (0-100, percentage of max speed)
    #[serde(default)]
    pub tilt_speed: Option<u8>,

    /// Duration in milliseconds (for timed movements)
    #[serde(default)]
    pub duration_ms: Option<u32>,

    /// Whether to auto-stop after duration
    #[serde(default = "default_true")]
    pub auto_stop: bool,
}

fn default_true() -> bool {
    true
}

/// Parameters for zoom commands
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ZoomParams {
    /// Zoom speed (0-100, percentage of max speed)
    #[serde(default)]
    pub speed: Option<u8>,

    /// Duration in milliseconds
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

/// Parameters for focus commands
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct FocusParams {
    /// Focus speed (0-100, percentage of max speed)
    #[serde(default)]
    pub speed: Option<u8>,

    /// Duration in milliseconds
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

/// Absolute position for PTZ
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct AbsolutePosition {
    /// Pan position (protocol-specific units)
    pub pan: Option<f64>,

    /// Tilt position (protocol-specific units)
    pub tilt: Option<f64>,

    /// Zoom position (protocol-specific units)
    pub zoom: Option<f64>,
}

/// Relative movement delta
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct RelativePosition {
    /// Pan delta
    pub pan_delta: Option<f64>,

    /// Tilt delta
    pub tilt_delta: Option<f64>,

    /// Zoom delta
    pub zoom_delta: Option<f64>,
}

/// PTZ command enumeration for generic command interface
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "command", content = "params", rename_all = "snake_case")]
pub enum PtzCommand {
    // Movement commands
    MoveUp(MoveParams),
    MoveDown(MoveParams),
    MoveLeft(MoveParams),
    MoveRight(MoveParams),
    MoveUpLeft(MoveParams),
    MoveUpRight(MoveParams),
    MoveDownLeft(MoveParams),
    MoveDownRight(MoveParams),
    MoveStop,

    // Zoom commands
    ZoomIn(ZoomParams),
    ZoomOut(ZoomParams),
    ZoomStop,

    // Focus commands
    FocusNear(FocusParams),
    FocusFar(FocusParams),
    FocusStop,
    FocusAuto,

    // Iris commands
    IrisOpen,
    IrisClose,
    IrisStop,
    IrisAuto,

    // Preset commands
    GotoPreset {
        preset_id: u32,
    },
    SetPreset {
        preset_id: u32,
        name: Option<String>,
    },
    ClearPreset {
        preset_id: u32,
    },
    GotoHome,

    // Absolute/Relative positioning
    MoveAbsolute(AbsolutePosition),
    MoveRelative(RelativePosition),

    // Power commands
    Wake,
    Sleep,
    Reset,
    Reboot,

    // Light and audio commands (ZoneMinder 1.39.13+, #190)
    LightOn,
    LightOff,
    IndicatorLightOn,
    IndicatorLightOff,
    /// Play one of the device's sounds, numbered as its firmware numbers them.
    AudioPlay {
        file: u32,
    },
    AudioStop,
    AudioVolumeUp,
    AudioVolumeDown,
    /// Ask the control module for the white light's state (a reply is read).
    LightStatus,
    /// Ask for the indicator light's state (a reply is read).
    IndicatorLightStatus,
}

impl PtzCommand {
    /// Get the zmcontrol.pl command name for this command
    pub fn zmcontrol_command(&self) -> &'static str {
        match self {
            Self::MoveUp(_) => "moveConUp",
            Self::MoveDown(_) => "moveConDown",
            Self::MoveLeft(_) => "moveConLeft",
            Self::MoveRight(_) => "moveConRight",
            Self::MoveUpLeft(_) => "moveConUpLeft",
            Self::MoveUpRight(_) => "moveConUpRight",
            Self::MoveDownLeft(_) => "moveConDownLeft",
            Self::MoveDownRight(_) => "moveConDownRight",
            Self::MoveStop => "moveStop",
            Self::ZoomIn(_) => "zoomConTele",
            Self::ZoomOut(_) => "zoomConWide",
            Self::ZoomStop => "zoomStop",
            Self::FocusNear(_) => "focusConNear",
            Self::FocusFar(_) => "focusConFar",
            Self::FocusStop => "focusStop",
            Self::FocusAuto => "focusAuto",
            Self::IrisOpen => "irisConOpen",
            Self::IrisClose => "irisConClose",
            Self::IrisStop => "irisStop",
            Self::IrisAuto => "irisAuto",
            Self::GotoPreset { .. } => "presetGoto",
            Self::SetPreset { .. } => "presetSet",
            Self::ClearPreset { .. } => "presetClear",
            Self::GotoHome => "presetHome",
            // The Perl bridge doesn't send these names: it splits a relative
            // move into `moveRel<Dir>` / `zoomRel*` calls and refuses an
            // absolute one (#248).
            Self::MoveAbsolute(_) => "moveAbs",
            Self::MoveRelative(_) => "moveRel",
            Self::Wake => "wake",
            Self::Sleep => "sleep",
            Self::Reset => "reset",
            Self::Reboot => "reboot",
            Self::LightOn => "lightOn",
            Self::LightOff => "lightOff",
            Self::IndicatorLightOn => "indicatorLightOn",
            Self::IndicatorLightOff => "indicatorLightOff",
            Self::AudioPlay { .. } => "audioPlay",
            Self::AudioStop => "audioStop",
            Self::AudioVolumeUp => "audioVolumeUp",
            Self::AudioVolumeDown => "audioVolumeDown",
            Self::LightStatus => "lightStatus",
            Self::IndicatorLightStatus => "indicatorLightStatus",
        }
    }
}

/// Configuration for connecting to a PTZ camera
#[derive(Debug, Clone)]
pub struct PtzConnectionConfig {
    /// Monitor ID
    pub monitor_id: u32,

    /// Control address (host:port, URL, or device path)
    pub address: String,

    /// Username for authentication
    pub username: Option<String>,

    /// Password for authentication
    pub password: Option<String>,

    /// Protocol name
    pub protocol: String,

    /// Auto-stop timeout in seconds
    pub auto_stop_timeout: Option<f64>,
}

/// Result of a PTZ command execution
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PtzCommandResult {
    /// Whether the command succeeded
    pub success: bool,

    /// Human-readable message
    pub message: String,

    /// Current position after command (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<AbsolutePosition>,
}

impl PtzCommandResult {
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
            position: None,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
            position: None,
        }
    }
}

/// The main PTZ control trait that protocol implementations must implement
#[async_trait]
pub trait PtzControl: Send + Sync {
    /// Get the capabilities of this PTZ controller
    fn capabilities(&self) -> &PtzCapabilities;

    /// Get the protocol name
    fn protocol_name(&self) -> &str;

    /// Check if this is a native Rust implementation or Perl bridge
    fn is_native(&self) -> bool;

    /// Execute a PTZ command
    async fn execute(&self, command: PtzCommand) -> PtzResult<PtzCommandResult>;

    /// Get current position (if supported)
    async fn get_position(&self) -> PtzResult<Option<AbsolutePosition>> {
        Ok(None)
    }

    /// Stop all movement
    async fn stop_all(&self) -> PtzResult<PtzCommandResult> {
        self.execute(PtzCommand::MoveStop).await
    }
}

/// Factory trait for creating PTZ control instances
pub trait PtzControlFactory: Send + Sync {
    /// Protocol name this factory handles
    fn protocol_name(&self) -> &str;

    /// Whether this is a native Rust implementation
    fn is_native(&self) -> bool;

    /// Create a PTZ control instance
    fn create(
        &self,
        config: PtzConnectionConfig,
        capabilities: PtzCapabilities,
    ) -> Box<dyn PtzControl>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_move_direction_components() {
        assert_eq!(MoveDirection::Up.pan_component(), 0);
        assert_eq!(MoveDirection::Up.tilt_component(), 1);

        assert_eq!(MoveDirection::UpRight.pan_component(), 1);
        assert_eq!(MoveDirection::UpRight.tilt_component(), 1);

        assert_eq!(MoveDirection::Left.pan_component(), -1);
        assert_eq!(MoveDirection::Left.tilt_component(), 0);
    }

    #[test]
    fn test_move_direction_diagonal() {
        assert!(MoveDirection::UpLeft.is_diagonal());
        assert!(!MoveDirection::Up.is_diagonal());
    }

    #[test]
    fn test_ptz_command_zmcontrol_mapping() {
        assert_eq!(
            PtzCommand::MoveUp(MoveParams::default()).zmcontrol_command(),
            "moveConUp"
        );
        assert_eq!(PtzCommand::ZoomStop.zmcontrol_command(), "zoomStop");
    }
}
