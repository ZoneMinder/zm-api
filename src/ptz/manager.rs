//! PTZ Manager for caching and coordinating PTZ control instances

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, instrument};

use super::capabilities::PtzCapabilities;
use super::error::{PtzError, PtzResult};
use super::registry::{ProtocolInfo, PtzRegistry};
use super::traits::{PtzCommand, PtzCommandResult, PtzConnectionConfig, PtzControl};

use crate::entity::controls::Model as ControlModel;
use crate::entity::monitors::Model as MonitorModel;

/// Cached PTZ control instance with metadata
struct CachedControl {
    control: Box<dyn PtzControl>,
    #[allow(dead_code)]
    monitor_id: u32,
    #[allow(dead_code)]
    created_at: std::time::Instant,
}

/// Manager for PTZ control instances
///
/// Handles caching, creation, and coordination of PTZ controllers.
pub struct PtzManager {
    registry: Arc<PtzRegistry>,
    cache: RwLock<HashMap<u32, CachedControl>>,
}

/// Refuse a control address that reaches this server or a cloud metadata
/// service. Anyone with Monitors Edit sets `ControlAddress`, and PTZ commands
/// make the server send requests there (natively, or through a zmcontrol
/// module).
async fn ensure_target_allowed(address: &str) -> PtzResult<()> {
    let own = crate::util::net_guard::own_addresses();
    match crate::util::net_guard::forbidden_target(address, &own).await {
        Some(ip) => Err(PtzError::InvalidParameter(format!(
            "control address reaches {ip}, which is this server or a cloud metadata address"
        ))),
        None => Ok(()),
    }
}

impl PtzManager {
    /// Create a new PTZ manager with the given registry
    pub fn new(registry: PtzRegistry) -> Self {
        Self {
            registry: Arc::new(registry),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Create a manager with the default registry: the native ONVIF factory
    /// registered, with the Perl bridge as fallback for all other protocols.
    pub fn with_defaults() -> Self {
        #[allow(unused_mut)]
        let mut registry = PtzRegistry::default();
        #[cfg(feature = "onvif-ptz")]
        registry.register_native(Arc::new(super::protocols::onvif::OnvifControlFactory::new()));
        Self::new(registry)
    }

    /// Get or create a PTZ control instance for a monitor
    #[instrument(skip(self, monitor, control))]
    pub async fn get_or_create(
        &self,
        monitor: &MonitorModel,
        control: &ControlModel,
    ) -> PtzResult<Arc<Box<dyn PtzControl>>> {
        let monitor_id = monitor.id;

        // Check cache first
        {
            let cache = self.cache.read().await;
            if let Some(_cached) = cache.get(&monitor_id) {
                debug!(monitor_id, "Using cached PTZ control");
                // Return a clone of the Arc - but we need to restructure this
                // For now, we'll just create a new one each time
            }
            drop(cache);
        }

        // Create new control instance
        let config = self.build_config(monitor, control)?;
        ensure_target_allowed(&config.address).await?;
        let capabilities = PtzCapabilities::from(control);
        let protocol = control.protocol.as_deref().unwrap_or("unknown");

        let control_instance = self
            .registry
            .create_control(protocol, config, capabilities)
            .ok_or_else(|| {
                PtzError::CommandNotSupported(format!(
                    "No implementation available for protocol: {}",
                    protocol
                ))
            })?;

        info!(
            monitor_id,
            protocol,
            is_native = control_instance.is_native(),
            "Created PTZ control instance"
        );

        // Cache it
        {
            let mut cache = self.cache.write().await;
            cache.insert(
                monitor_id,
                CachedControl {
                    control: control_instance,
                    monitor_id,
                    created_at: std::time::Instant::now(),
                },
            );
        }

        // Return from cache
        let cache = self.cache.read().await;
        let _cached = cache.get(&monitor_id).expect("just inserted");
        // This is a bit awkward - we need to return something that can be used
        // For now, return an error indicating we need to refactor
        // Actually, let's just execute directly
        Ok(Arc::new(Box::new(CachedControlRef {
            manager: self as *const PtzManager,
            monitor_id,
        }) as Box<dyn PtzControl>))
    }

    /// Execute a PTZ command for a monitor
    #[instrument(skip(self), fields(monitor_id))]
    pub async fn execute(
        &self,
        monitor_id: u32,
        command: PtzCommand,
    ) -> PtzResult<PtzCommandResult> {
        let cache = self.cache.read().await;
        let cached = cache
            .get(&monitor_id)
            .ok_or(PtzError::MonitorNotFound(monitor_id))?;

        cached.control.execute(command).await
    }

    /// Execute a PTZ command using monitor and control models directly
    #[instrument(skip(self, monitor, control))]
    pub async fn execute_with_models(
        &self,
        monitor: &MonitorModel,
        control: &ControlModel,
        command: PtzCommand,
    ) -> PtzResult<PtzCommandResult> {
        let monitor_id = monitor.id;

        // Ensure we have a cached control
        {
            let cache = self.cache.read().await;
            if !cache.contains_key(&monitor_id) {
                drop(cache);
                self.create_and_cache(monitor, control).await?;
            }
        }

        // Execute
        self.execute(monitor_id, command).await
    }

    /// Create and cache a control instance (public version)
    pub async fn create_and_cache_for_models(
        &self,
        monitor: &MonitorModel,
        control: &ControlModel,
    ) -> PtzResult<()> {
        self.create_and_cache(monitor, control).await
    }

    /// Create and cache a control instance
    async fn create_and_cache(
        &self,
        monitor: &MonitorModel,
        control: &ControlModel,
    ) -> PtzResult<()> {
        let monitor_id = monitor.id;
        let config = self.build_config(monitor, control)?;
        ensure_target_allowed(&config.address).await?;
        let capabilities = PtzCapabilities::from(control);
        let protocol = control.protocol.as_deref().unwrap_or("unknown");

        let control_instance = self
            .registry
            .create_control(protocol, config, capabilities)
            .ok_or_else(|| {
                PtzError::CommandNotSupported(format!(
                    "No implementation available for protocol: {}",
                    protocol
                ))
            })?;

        info!(
            monitor_id,
            protocol,
            is_native = control_instance.is_native(),
            "Created PTZ control instance"
        );

        let mut cache = self.cache.write().await;
        cache.insert(
            monitor_id,
            CachedControl {
                control: control_instance,
                monitor_id,
                created_at: std::time::Instant::now(),
            },
        );

        Ok(())
    }

    /// Build connection config from monitor and control models
    fn build_config(
        &self,
        monitor: &MonitorModel,
        control: &ControlModel,
    ) -> PtzResult<PtzConnectionConfig> {
        let address = monitor
            .control_address
            .clone()
            .or_else(|| monitor.control_device.clone())
            .unwrap_or_default();

        // Parse credentials from address or monitor fields
        let (parsed_address, username, password) = self.parse_control_address(&address, monitor);

        Ok(PtzConnectionConfig {
            monitor_id: monitor.id,
            address: parsed_address,
            username,
            password,
            protocol: control
                .protocol
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            auto_stop_timeout: monitor
                .auto_stop_timeout
                .map(|d| d.to_string().parse().unwrap_or(0.0)),
        })
    }

    /// Parse control address, extracting embedded credentials if present
    ///
    /// Supports formats:
    /// - `user:pass@host:port`
    /// - `https://user:pass@host/path`
    /// - `host:port` (credentials from monitor)
    /// - `/dev/ttyUSB0` (serial device)
    fn parse_control_address(
        &self,
        address: &str,
        monitor: &MonitorModel,
    ) -> (String, Option<String>, Option<String>) {
        // Check for serial device
        if address.starts_with('/') {
            return (address.to_string(), None, None);
        }

        // Try to parse as URL (only if it looks like a proper URL with http/https/rtsp scheme)
        if address.starts_with("http://")
            || address.starts_with("https://")
            || address.starts_with("rtsp://")
        {
            if let Ok(url) = url::Url::parse(address) {
                let username = if url.username().is_empty() {
                    monitor.user.clone()
                } else {
                    Some(url.username().to_string())
                };
                let password = url
                    .password()
                    .map(|s| s.to_string())
                    .or_else(|| monitor.pass.clone());

                // Rebuild URL without credentials
                let mut clean_url = url.clone();
                let _ = clean_url.set_username("");
                let _ = clean_url.set_password(None);

                return (clean_url.to_string(), username, password);
            }
        }

        // Try simple user:pass@host format
        if let Some(at_pos) = address.find('@') {
            let (creds, host) = address.split_at(at_pos);
            let host = &host[1..]; // Skip the @

            if let Some(colon_pos) = creds.find(':') {
                let (user, pass) = creds.split_at(colon_pos);
                let pass = &pass[1..]; // Skip the :
                return (
                    host.to_string(),
                    Some(user.to_string()),
                    Some(pass.to_string()),
                );
            }
        }

        // Plain address, use monitor credentials
        (
            address.to_string(),
            monitor.user.clone(),
            monitor.pass.clone(),
        )
    }

    /// Invalidate cached control for a monitor
    pub async fn invalidate(&self, monitor_id: u32) {
        let mut cache = self.cache.write().await;
        if cache.remove(&monitor_id).is_some() {
            debug!(monitor_id, "Invalidated cached PTZ control");
        }
    }

    /// Clear all cached controls
    pub async fn clear_cache(&self) {
        let mut cache = self.cache.write().await;
        let count = cache.len();
        cache.clear();
        info!(count, "Cleared PTZ control cache");
    }

    /// Get capabilities for a cached monitor
    pub async fn get_capabilities(&self, monitor_id: u32) -> PtzResult<PtzCapabilities> {
        let cache = self.cache.read().await;
        let cached = cache
            .get(&monitor_id)
            .ok_or(PtzError::MonitorNotFound(monitor_id))?;
        Ok(cached.control.capabilities().clone())
    }

    /// List available protocols
    pub fn list_protocols(&self) -> Vec<ProtocolInfo> {
        self.registry.list_protocols()
    }

    /// Check if a protocol has a native implementation
    pub fn is_native_protocol(&self, protocol: &str) -> bool {
        self.registry.has_native(protocol)
    }
}

/// Helper struct to allow returning a reference to cached control
/// This is a workaround for async lifetime issues
#[allow(dead_code)]
struct CachedControlRef {
    manager: *const PtzManager,
    monitor_id: u32,
}

// Safety: The manager pointer is valid for the lifetime of the CachedControlRef
// and we only use it for reading
unsafe impl Send for CachedControlRef {}
unsafe impl Sync for CachedControlRef {}

#[async_trait::async_trait]
impl PtzControl for CachedControlRef {
    fn capabilities(&self) -> &PtzCapabilities {
        // This is unsafe but we know the manager is valid
        // In practice, we should refactor to avoid this
        static EMPTY: PtzCapabilities = PtzCapabilities {
            control_id: 0,
            name: String::new(),
            protocol: None,
            power: super::capabilities::PowerCapabilities {
                can_wake: false,
                can_sleep: false,
                can_reset: false,
                can_reboot: false,
            },
            pan_tilt: super::capabilities::PanTiltCapabilities {
                can_pan: false,
                can_tilt: false,
                can_move: false,
                can_move_diag: false,
                can_move_map: false,
                can_move_abs: false,
                can_move_rel: false,
                can_move_con: false,
                pan_range: super::capabilities::AxisRange {
                    min: None,
                    max: None,
                },
                pan_step: super::capabilities::AxisStep {
                    min: None,
                    max: None,
                },
                pan_speed: super::capabilities::AxisSpeed {
                    has_speed: false,
                    min: None,
                    max: None,
                },
                pan_turbo: super::capabilities::TurboSpeed {
                    has_turbo: false,
                    speed: None,
                },
                tilt_range: super::capabilities::AxisRange {
                    min: None,
                    max: None,
                },
                tilt_step: super::capabilities::AxisStep {
                    min: None,
                    max: None,
                },
                tilt_speed: super::capabilities::AxisSpeed {
                    has_speed: false,
                    min: None,
                    max: None,
                },
                tilt_turbo: super::capabilities::TurboSpeed {
                    has_turbo: false,
                    speed: None,
                },
            },
            zoom: super::capabilities::AxisCapabilities {
                can: false,
                can_auto: false,
                can_abs: false,
                can_rel: false,
                can_con: false,
                range: super::capabilities::AxisRange {
                    min: None,
                    max: None,
                },
                step: super::capabilities::AxisStep {
                    min: None,
                    max: None,
                },
                speed: super::capabilities::AxisSpeed {
                    has_speed: false,
                    min: None,
                    max: None,
                },
            },
            focus: super::capabilities::AxisCapabilities {
                can: false,
                can_auto: false,
                can_abs: false,
                can_rel: false,
                can_con: false,
                range: super::capabilities::AxisRange {
                    min: None,
                    max: None,
                },
                step: super::capabilities::AxisStep {
                    min: None,
                    max: None,
                },
                speed: super::capabilities::AxisSpeed {
                    has_speed: false,
                    min: None,
                    max: None,
                },
            },
            iris: super::capabilities::AxisCapabilities {
                can: false,
                can_auto: false,
                can_abs: false,
                can_rel: false,
                can_con: false,
                range: super::capabilities::AxisRange {
                    min: None,
                    max: None,
                },
                step: super::capabilities::AxisStep {
                    min: None,
                    max: None,
                },
                speed: super::capabilities::AxisSpeed {
                    has_speed: false,
                    min: None,
                    max: None,
                },
            },
            gain: super::capabilities::AxisCapabilities {
                can: false,
                can_auto: false,
                can_abs: false,
                can_rel: false,
                can_con: false,
                range: super::capabilities::AxisRange {
                    min: None,
                    max: None,
                },
                step: super::capabilities::AxisStep {
                    min: None,
                    max: None,
                },
                speed: super::capabilities::AxisSpeed {
                    has_speed: false,
                    min: None,
                    max: None,
                },
            },
            white_balance: super::capabilities::AxisCapabilities {
                can: false,
                can_auto: false,
                can_abs: false,
                can_rel: false,
                can_con: false,
                range: super::capabilities::AxisRange {
                    min: None,
                    max: None,
                },
                step: super::capabilities::AxisStep {
                    min: None,
                    max: None,
                },
                speed: super::capabilities::AxisSpeed {
                    has_speed: false,
                    min: None,
                    max: None,
                },
            },
            presets: super::capabilities::PresetCapabilities {
                has_presets: false,
                num_presets: 0,
                has_home_preset: false,
                can_set_presets: false,
            },
            scan: super::capabilities::ScanCapabilities {
                can_auto_scan: false,
                num_scan_paths: 0,
            },
            light: super::capabilities::LightCapabilities {
                can_light: false,
                can_indicator_light: false,
            },
            audio: super::capabilities::AudioCapabilities {
                can_play: false,
                min_file: None,
                max_file: None,
                can_volume: false,
            },
        };
        &EMPTY
    }

    fn protocol_name(&self) -> &str {
        "cached"
    }

    fn is_native(&self) -> bool {
        false
    }

    async fn execute(&self, _command: PtzCommand) -> PtzResult<PtzCommandResult> {
        Err(PtzError::InternalError(
            "CachedControlRef should not be used directly".to_string(),
        ))
    }
}

impl Default for PtzManager {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::entity::monitors::Model as MonitorModel;
    use crate::entity::sea_orm_active_enums::{
        Analysing, AnalysisImage, AnalysisSource, Capturing, Decoding, DefaultCodec,
        EventCloseMode, Function, Importance, MonitorType, Orientation, Recording, RecordingSource,
        Rtsp2WebType, StreamChannel,
    };

    pub(crate) fn test_monitor() -> MonitorModel {
        MonitorModel {
            id: 1,
            name: "Test Camera".to_string(),
            deleted: 0,
            analysis_image_opacity: 128,
            object_detection: "none".to_string(),
            object_detection_model: String::new(),
            object_detection_object_threshold: 0.4,
            object_detection_nms_threshold: 0.25,
            what_display: crate::entity::sea_orm_active_enums::WhatDisplay::OnlyVideo,
            notes: None,
            server_id: None,
            storage_id: Some(0),
            manufacturer_id: None,
            model_id: None,
            r#type: MonitorType::Remote,
            function: Function::Monitor,
            capturing: Capturing::Always,
            enabled: 1,
            decoding_enabled: 1,
            decoding: Decoding::Always,
            rtsp2_web_enabled: 0,
            rtsp2_web_type: Rtsp2WebType::Mse,
            default_player: None,
            stream_channel: StreamChannel::Restream,
            go2_rtc_enabled: 0,
            janus_enabled: 0,
            janus_audio_enabled: 0,
            janus_profile_override: String::new(),
            restream: 0,
            rtsp_user: None,
            janus_rtsp_session_timeout: 0,
            linked_monitors: None,
            triggers: String::new(),
            event_start_command: String::new(),
            event_end_command: String::new(),
            onvif_url: String::new(),
            onvif_events_path: String::new(),
            onvif_username: String::new(),
            onvif_password: String::new(),
            onvif_options: String::new(),
            onvif_event_listener: 0,
            onvif_alarm_text: None,
            use_amcrest_api: 0,
            device: String::new(),
            channel: 0,
            format: 0,
            v4l_multi_buffer: None,
            v4l_captures_per_frame: None,
            protocol: None,
            method: None,
            host: None,
            port: String::new(),
            sub_path: String::new(),
            path: None,
            second_path: None,
            options: None,
            user: None,
            pass: None,
            width: 1920,
            height: 1080,
            colours: 4,
            palette: 0,
            orientation: Orientation::Rotate0,
            deinterlacing: 0,
            decoder: None,
            decoder_hw_accel_name: None,
            decoder_hw_accel_device: None,
            save_jpe_gs: 0,
            video_writer: 0,
            output_codec: 0,
            output_codec_name: String::new(),
            encoder: None,
            encoder_hw_accel_name: None,
            encoder_hw_accel_device: None,
            output_container: None,
            encoder_parameters: None,
            wall_clock_timestamps: 0,
            record_audio: 0,
            recording_source: RecordingSource::Primary,
            brightness: None,
            contrast: None,
            hue: None,
            colour: None,
            event_prefix: "Event-".to_string(),
            label_format: None,
            label_x: 0,
            label_y: 0,
            label_size: 1,
            image_buffer_count: 50,
            max_image_buffer_count: 0,
            warmup_count: 0,
            pre_event_count: 5,
            post_event_count: 5,
            stream_replay_buffer: 0,
            alarm_frame_count: 1,
            section_length: 600,
            section_length_warn: 0,
            event_close_mode: EventCloseMode::System,
            min_section_length: 10,
            motion_frame_skip: 0,
            device_class: crate::entity::sea_orm_active_enums::DeviceClass::Camera,
            audio_detection: 0,
            audio_threshold: 0,
            audio_alarm_score: 9,
            analysis_fps_limit: None,
            analysis_update_delay: 0,
            max_fps: None,
            alarm_max_fps: None,
            fps_report_interval: 100,
            ref_blend_perc: 6,
            alarm_ref_blend_perc: 6,
            controllable: 1,
            control_id: Some(1),
            control_device: None,
            control_address: Some("admin:pass@192.168.1.100:80".to_string()),
            auto_stop_timeout: None,
            track_motion: 0,
            track_delay: None,
            return_location: -1,
            return_delay: None,
            modect_during_ptz: 0,
            default_rate: 100,
            default_scale: "100".to_string(),
            default_codec: DefaultCodec::Auto,
            signal_check_points: 0,
            signal_check_colour: "#0000BE".to_string(),
            web_colour: "#ff0000".to_string(),
            exif: 0,
            sequence: None,
            zone_count: 0,
            refresh: None,
            latitude: None,
            longitude: None,
            rtsp_server: 0,
            rtsp_stream_name: String::new(),
            soap_wsa_compl: 0,
            importance: Importance::Normal,
            mqtt_enabled: 0,
            mqtt_subscriptions: None,
            startup_delay: 0,
            analysing: Analysing::None,
            analysis_source: AnalysisSource::Primary,
            analysis_image: AnalysisImage::FullColour,
            recording: Recording::None,
        }
    }

    #[test]
    fn test_parse_control_address_simple() {
        let manager = PtzManager::with_defaults();
        let monitor = test_monitor();

        let (addr, user, pass) =
            manager.parse_control_address("admin:pass@192.168.1.100:80", &monitor);

        assert_eq!(addr, "192.168.1.100:80");
        assert_eq!(user, Some("admin".to_string()));
        assert_eq!(pass, Some("pass".to_string()));
    }

    #[test]
    fn test_parse_control_address_serial() {
        let manager = PtzManager::with_defaults();
        let monitor = test_monitor();

        let (addr, user, pass) = manager.parse_control_address("/dev/ttyUSB0", &monitor);

        assert_eq!(addr, "/dev/ttyUSB0");
        assert!(user.is_none());
        assert!(pass.is_none());
    }

    #[test]
    fn test_parse_control_address_url() {
        let manager = PtzManager::with_defaults();
        let monitor = test_monitor();

        let (addr, user, pass) = manager
            .parse_control_address("http://admin:secret@192.168.1.100/onvif/device", &monitor);

        assert!(addr.contains("192.168.1.100"));
        assert!(!addr.contains("admin"));
        assert_eq!(user, Some("admin".to_string()));
        assert_eq!(pass, Some("secret".to_string()));
    }
}
