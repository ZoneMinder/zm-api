// src/dto/request/monitor.rs
use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
// use sea_orm::ActiveEnum; // Unused import
use crate::entity::sea_orm_active_enums::{
    Analysing, AnalysisImage, AnalysisSource, Capturing, Decoding, DefaultCodec, DeviceClass,
    EventCloseMode, Function, Importance, MonitorType, Orientation, OutputContainer, Recording,
    RecordingSource, Rtsp2WebType,
};

// Custom validator for state actions
pub fn is_valid_state(value: &str, _ctx: &()) -> garde::Result {
    match value {
        "start" | "stop" | "restart" => Ok(()),
        _ => Err(garde::Error::new(
            "invalid state; must be 'start', 'stop', or 'restart'",
        )),
    }
}

// Custom validator for alarm actions.
//
// `cancel` is accepted as a synonym for `off`: the service already handles it
// (`service::monitor::control_alarm`) and the field's own documentation
// advertises it, but the validator used to reject it with a 422 before the
// request ever reached the service.
pub fn is_valid_alarm_action(value: &str, _ctx: &()) -> garde::Result {
    match value {
        "on" | "off" | "cancel" | "status" | "disable_alarms" | "enable_alarms" => Ok(()),
        _ => Err(garde::Error::new(
            "invalid alarm action; must be 'on', 'off', 'cancel', 'status', \
             'disable_alarms' or 'enable_alarms'",
        )),
    }
}

// Hardening for fields that flow into URLs, hostnames, file paths, or process
// argv assembled by ZoneMinder's capture daemon. We don't try to validate URL
// syntax — that's the camera library's job — but we reject inputs that have no
// business in any URL component:
//
//   * NUL, control characters, and CR/LF: header smuggling, log injection, and
//     C-string truncation downstream.
//   * Lengths beyond what any real camera URL would need: bounded memory cost
//     and a sanity cap when these values get serialised back into config or
//     argv.
//
// Used on `host`, `path`, `second_path`, `port`, and similar URL components.
pub fn is_safe_url_component(value: &str, _ctx: &()) -> garde::Result {
    if value.len() > 2048 {
        return Err(garde::Error::new("must be at most 2048 bytes"));
    }
    if value.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err(garde::Error::new(
            "must not contain NUL, control characters, or CR/LF",
        ));
    }
    Ok(())
}

// Stricter rules for fields that take a full URL (currently only `onvif_url`).
// In addition to the component-level checks, restrict the scheme to the small
// set that makes sense for a network camera, blocking SSRF gadgets like
// `file://`, `unix://`, `gopher://`, and `javascript:`.
pub fn is_safe_onvif_url(value: &str, ctx: &()) -> garde::Result {
    is_safe_url_component(value, ctx)?;
    if let Some(idx) = value.find("://") {
        let scheme = value[..idx].to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "rtsp" | "rtsps") {
            return Err(garde::Error::new(
                "unsupported URL scheme; expected http, https, rtsp, or rtsps",
            ));
        }
    }
    Ok(())
}

/// Schemes a capture `Path`/`SecondPath` may use. Anything else is refused, so
/// a path can't name a source that runs a program: go2rtc, which ZoneMinder
/// hands the raw `Path` to, runs `exec:`, `echo:` and `expr:` sources as
/// commands and builds an ffmpeg command line from `ffmpeg:` ones.
const CAPTURE_SCHEMES: &[&str] = &[
    "rtsp", "rtsps", "rtmp", "rtmps", "http", "https", "udp", "tcp", "rtp", "srt", "mmsh",
];

/// A camera source path: empty, a local path (`File` and `Remote` monitors), or
/// a `scheme://` URL with a scheme from [`CAPTURE_SCHEMES`].
pub fn is_safe_capture_path(value: &str, ctx: &()) -> garde::Result {
    is_safe_url_component(value, ctx)?;
    let v = value.trim();
    if v.is_empty() || v.starts_with('/') {
        return Ok(());
    }
    // zmc fetches whatever this names, as the server. A camera on the same
    // host is normal, so loopback stays allowed; a cloud metadata service is
    // never a camera, and it answers with the instance's credentials.
    if crate::util::net_guard::names_metadata_service(v) {
        return Err(garde::Error::new(
            "must not point at a cloud metadata service",
        ));
    }
    match v.split_once("://") {
        Some((scheme, _)) if CAPTURE_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str()) => {
            Ok(())
        }
        _ => Err(garde::Error::new(
            "must be a local path or a URL with one of: rtsp, rtsps, rtmp, rtmps, http, https, udp, tcp, rtp, srt, mmsh",
        )),
    }
}

/// A `Local` monitor's capture device. ZoneMinder puts it in a shell command
/// (`zmdc.pl start zmc -d '<Device>'`), so only a plain `/dev/...` path is
/// accepted, matching upstream's own check in `zmpkg.pl`, plus no `..`.
pub fn is_safe_device_path(value: &str, _ctx: &()) -> garde::Result {
    if value.is_empty() {
        return Ok(());
    }
    let rest = value.strip_prefix("/dev/").unwrap_or("");
    let plain = !rest.is_empty()
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/' | b'.' | b'-'));
    if plain && !value.contains("..") {
        Ok(())
    } else {
        Err(garde::Error::new(
            "must be a /dev/ path of letters, digits, '_', '-', '.' and '/'",
        ))
    }
}

/// A storage area's filesystem path. ZoneMinder runs `df <Path>` through a
/// shell, so only an absolute path of characters a shell treats literally is
/// accepted.
pub fn is_safe_storage_path(value: &str, _ctx: &()) -> garde::Result {
    if value.is_empty() {
        return Ok(());
    }
    let plain = value.starts_with('/')
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'_' | b'/' | b'.' | b'-' | b'+' | b'@' | b':' | b',')
        });
    if plain {
        Ok(())
    } else {
        Err(garde::Error::new(
            "must be an absolute path of letters, digits and _ / . - + @ : ,",
        ))
    }
}

/// A PTZ control protocol. ZoneMinder loads `ZoneMinder::Control::<Protocol>`
/// as a Perl module, so anything but a bare module name could load a file
/// from elsewhere on disk.
pub fn is_perl_module_name(value: &str, _ctx: &()) -> garde::Result {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        Ok(())
    } else {
        Err(garde::Error::new(
            "must contain only letters, digits and '_'",
        ))
    }
}

// `event_start_command` and `event_end_command` are by design shell commands
// that ZoneMinder execs, so we cannot block shell metacharacters without
// breaking the feature. We can still bound the field and reject NUL and
// newlines, which would otherwise enable C-string truncation and log
// forgery in the daemon that runs the command.
pub fn is_safe_command_string(value: &str, _ctx: &()) -> garde::Result {
    if value.len() > 4096 {
        return Err(garde::Error::new("must be at most 4096 bytes"));
    }
    if value.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r') {
        return Err(garde::Error::new(
            "must not contain NUL or newline characters",
        ));
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, ToSchema, Validate)]
pub struct CreateMonitorRequest {
    #[garde(length(chars, min = 1, max = 64))]
    pub name: String,
    #[garde(skip)] // Boolean type doesn't need validation
    pub deleted: Option<bool>,
    /// Whether the monitor is enabled (ZoneMinder's `Monitors.Enabled`).
    /// Defaults to enabled when omitted.
    #[garde(range(min = 0, max = 1))]
    pub enabled: Option<u8>,
    #[garde(skip)] // Option<String> can be None, or we could validate if Some
    pub notes: Option<String>,
    #[garde(skip)] // Option<u32> can be None, or we could validate if Some
    pub server_id: Option<u32>,
    // 0 is ZoneMinder's sentinel for the default storage (GH #19).
    #[garde(inner(range(min = 0)))]
    pub storage_id: Option<u16>,
    #[garde(skip)] // Option<u32> can be None
    pub manufacturer_id: Option<u32>,
    #[garde(skip)] // Option<u32> can be None
    pub model_id: Option<u32>,

    // Enum field validations
    #[serde(rename = "type")]
    #[garde(skip)]
    pub r#type: MonitorType,

    #[serde(rename = "function")]
    #[garde(skip)]
    pub function: Option<Function>,

    #[serde(rename = "capturing")]
    #[garde(skip)]
    pub capturing: Option<Capturing>,

    #[garde(inner(range(min = 0, max = 1)))]
    pub decoding_enabled: Option<u8>,

    #[serde(rename = "decoding")]
    #[garde(skip)]
    pub decoding: Option<Decoding>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub rtsp2_web_enabled: Option<i8>,

    #[serde(rename = "rtsp2_web_type")]
    #[garde(skip)]
    pub rtsp2_web_type: Option<Rtsp2WebType>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub janus_enabled: Option<i8>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub janus_audio_enabled: Option<i8>,

    #[garde(inner(length(chars, max = 30)))]
    pub janus_profile_override: Option<String>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub restream: Option<i8>,

    #[garde(skip)] // Option<i32> can be None
    pub rtsp_user: Option<i32>,

    #[garde(skip)] // Option<i32> can be None
    pub janus_rtsp_session_timeout: Option<i32>,

    #[garde(inner(length(chars, max = 255)))]
    pub linked_monitors: Option<String>,

    #[garde(length(min = 0))] // Empty string is valid, but validate it's a string
    pub triggers: Option<String>,

    // DB column `EventStartCommand` is varchar(255).
    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_command_string)))]
    pub event_start_command: Option<String>,

    // DB column `EventEndCommand` is varchar(255).
    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_command_string)))]
    pub event_end_command: Option<String>,

    // DB column `ONVIF_URL` is varchar(255).
    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_onvif_url)))]
    pub onvif_url: Option<String>,

    #[garde(inner(length(chars, max = 20)))]
    pub onvif_events_path: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub onvif_username: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub onvif_password: Option<String>,

    #[garde(inner(length(chars, max = 255)))]
    pub onvif_options: Option<String>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub onvif_event_listener: Option<i8>,

    #[garde(inner(length(chars, max = 30)))]
    pub onvif_alarm_text: Option<String>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub use_amcrest_api: Option<i8>,

    #[garde(inner(length(max = 255), custom(is_safe_device_path)))]
    pub device: Option<String>,

    #[garde(inner(range(min = 0)))]
    pub channel: Option<u8>,

    #[garde(skip)] // u32 doesn't need range validation in this case
    pub format: Option<u32>,

    #[garde(skip)] // Option<u8> can be None
    pub v4l_multi_buffer: Option<u8>,

    #[garde(skip)] // Option<u8> can be None
    pub v4l_captures_per_frame: Option<u8>,

    #[garde(inner(length(chars, max = 16)))]
    pub protocol: Option<String>,

    #[garde(inner(length(chars, max = 16)))]
    pub method: Option<String>,

    // DB column `Host` is varchar(64).
    #[garde(inner(length(max = 64)))]
    #[garde(inner(custom(is_safe_url_component)))]
    pub host: Option<String>,

    // DB column `Port` is varchar(8).
    #[garde(inner(length(max = 8)))]
    #[garde(inner(custom(is_safe_url_component)))]
    pub port: Option<String>,

    // DB column `SubPath` is varchar(64).
    #[garde(inner(length(max = 64)))]
    #[garde(inner(custom(is_safe_url_component)))]
    pub sub_path: Option<String>,

    // DB column `Path` is varchar(255).
    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_capture_path)))]
    pub path: Option<String>,

    // DB column `SecondPath` is varchar(255).
    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_capture_path)))]
    pub second_path: Option<String>,

    #[garde(inner(length(chars, max = 255)))]
    pub options: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub user: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub pass: Option<String>,

    #[garde(inner(range(min = 1)))]
    pub width: Option<u16>,

    #[garde(inner(range(min = 1)))]
    pub height: Option<u16>,

    #[garde(inner(range(min = 0)))]
    pub colours: Option<u8>,

    #[garde(skip)] // u32 doesn't need validation in this case
    pub palette: Option<u32>,

    #[serde(rename = "orientation")]
    #[garde(skip)]
    pub orientation: Option<Orientation>,

    #[garde(skip)] // u32 doesn't need validation in this case
    pub deinterlacing: Option<u32>,

    #[garde(inner(length(chars, max = 32)))]
    pub decoder: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub decoder_hw_accel_name: Option<String>,

    #[garde(inner(length(chars, max = 255)))]
    pub decoder_hw_accel_device: Option<String>,

    /// `Monitors.SaveJPEGs` is a two-bit mask, not a flag: bit 0 saves
    /// captured frames and bit 1 saves analysis images, so 3 means both — and
    /// 3 is the column default, which the old `max = 1` rejected (GH #39).
    #[garde(inner(range(min = 0, max = 3)))]
    pub save_jpe_gs: Option<i8>,

    /// 0 disabled, 1 encode, 2 camera passthrough.
    #[garde(inner(range(min = 0, max = 2)))]
    pub video_writer: Option<i8>,

    #[garde(skip)] // Option<u32> can be None
    pub output_codec: Option<u32>,

    #[garde(inner(length(chars, max = 32)))]
    pub encoder: Option<String>,

    // Nullable so a monitor whose stored OutputContainer is NULL round-trips
    // GET → POST without a 422 (GH #18).
    #[serde(rename = "output_container")]
    #[garde(skip)]
    pub output_container: Option<OutputContainer>,

    #[garde(skip)] // Option<String> can be None
    pub encoder_parameters: Option<String>,

    /// Boolean. The old `min = -1` accepted a value the column has no meaning for.
    #[garde(inner(range(min = 0, max = 1)))]
    pub record_audio: Option<i8>,

    #[serde(rename = "recording_source")]
    #[garde(skip)]
    pub recording_source: Option<RecordingSource>,

    /// Accepted and ignored: ZoneMinder 1.39.37 removed `RTSPDescribe`.
    #[garde(skip)]
    pub rtsp_describe: Option<u8>,

    // -1 is ZoneMinder's "leave the camera setting alone" sentinel (GH #19).
    #[garde(inner(range(min = -1, max = i32::MAX)))]
    pub brightness: Option<i32>,

    #[garde(inner(range(min = -1, max = i32::MAX)))]
    pub contrast: Option<i32>,

    #[garde(inner(range(min = -1, max = i32::MAX)))]
    pub hue: Option<i32>,

    #[garde(inner(range(min = -1, max = i32::MAX)))]
    pub colour: Option<i32>,

    #[garde(inner(length(chars, max = 32)))]
    pub event_prefix: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub label_format: Option<String>,

    #[garde(skip)] // u16 doesn't need validation in this case
    pub label_x: Option<u16>,

    #[garde(skip)] // u16 doesn't need validation in this case
    pub label_y: Option<u16>,

    #[garde(inner(range(min = 1)))]
    pub label_size: Option<u16>,

    #[garde(inner(range(min = 1)))]
    pub image_buffer_count: Option<u16>,

    // 0 means unlimited in ZoneMinder (GH #19).
    #[garde(inner(range(min = 0)))]
    pub max_image_buffer_count: Option<u16>,

    #[garde(skip)] // u16 doesn't need validation in this case
    pub warmup_count: Option<u16>,

    #[garde(skip)] // u16 doesn't need validation in this case
    pub pre_event_count: Option<u16>,

    #[garde(skip)] // u16 doesn't need validation in this case
    pub post_event_count: Option<u16>,

    #[garde(inner(range(min = 0)))]
    pub stream_replay_buffer: Option<u32>,

    #[garde(inner(range(min = 1)))]
    pub alarm_frame_count: Option<u16>,

    #[garde(inner(range(min = 1)))]
    pub section_length: Option<u32>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub section_length_warn: Option<i8>,

    #[serde(rename = "event_close_mode")]
    #[garde(skip)]
    pub event_close_mode: Option<EventCloseMode>,

    #[garde(inner(range(min = 1)))]
    pub min_section_length: Option<u32>,

    #[garde(skip)] // u16 doesn't need validation in this case
    pub motion_frame_skip: Option<u16>,

    /// Whether this monitor is a camera or an IP speaker (1.39.30). Defaults to Camera.
    #[garde(skip)]
    pub device_class: Option<DeviceClass>,
    /// Audio-level alarm detection on the recorded audio (1.39.31); 0 = off.
    #[garde(skip)]
    pub audio_detection: Option<u8>,
    /// Audio level (0-255) above which audio detection alarms.
    #[garde(skip)]
    pub audio_threshold: Option<u8>,
    /// Score an audio alarm contributes; upstream default 9.
    #[garde(skip)]
    pub audio_alarm_score: Option<u16>,

    #[garde(skip)] // Option<f64> can be None
    pub analysis_fps_limit: Option<f64>,

    #[garde(skip)] // u16 doesn't need validation in this case
    pub analysis_update_delay: Option<u16>,

    #[garde(skip)] // Option<f64> can be None
    pub max_fps: Option<f64>,

    #[garde(skip)] // Option<f64> can be None
    pub alarm_max_fps: Option<f64>,

    #[garde(skip)] // u16 doesn't need validation in this case
    pub fps_report_interval: Option<u16>,

    #[garde(inner(range(min = 0, max = 100)))]
    pub ref_blend_perc: Option<u8>,

    #[garde(inner(range(min = 0, max = 100)))]
    pub alarm_ref_blend_perc: Option<u8>,

    #[garde(inner(range(min = 0, max = 1)))]
    pub controllable: Option<u8>,

    #[garde(skip)] // Option<u32> can be None
    pub control_id: Option<u32>,

    #[garde(inner(length(chars, max = 255)))]
    pub control_device: Option<String>,

    #[garde(inner(length(chars, max = 255)))]
    pub control_address: Option<String>,

    #[garde(skip)] // Option<f64> can be None
    pub auto_stop_timeout: Option<f64>,

    #[garde(inner(range(min = 0, max = 1)))]
    pub track_motion: Option<u8>,

    #[garde(skip)] // Option<u16> can be None
    pub track_delay: Option<u16>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub return_location: Option<i8>,

    #[garde(skip)] // Option<u16> can be None
    pub return_delay: Option<u16>,

    #[garde(inner(range(min = 0, max = 1)))]
    pub modect_during_ptz: Option<u8>,

    #[garde(skip)]
    pub default_rate: Option<u16>,

    #[garde(inner(length(chars, max = 16)))]
    pub default_scale: Option<String>,

    #[serde(rename = "default_codec")]
    #[garde(skip)]
    pub default_codec: Option<DefaultCodec>,

    #[garde(skip)] // u32 doesn't need validation in this case
    pub signal_check_points: Option<u32>,

    #[garde(inner(length(chars, max = 32)))]
    pub signal_check_colour: Option<String>,

    #[garde(inner(length(chars, max = 32)))]
    pub web_colour: Option<String>,

    #[garde(inner(range(min = 0, max = 1)))]
    pub exif: Option<u8>,

    #[garde(skip)] // Option<u16> can be None
    pub sequence: Option<u16>,

    #[garde(inner(range(min = -128, max = 127)))]
    pub zone_count: Option<i8>,

    #[garde(skip)] // Option<u32> can be None
    pub refresh: Option<u32>,

    #[garde(skip)] // Option<f64> can be None
    pub latitude: Option<f64>,

    #[garde(skip)] // Option<f64> can be None
    pub longitude: Option<f64>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub rtsp_server: Option<i8>,

    #[garde(inner(length(chars, max = 255)))]
    pub rtsp_stream_name: Option<String>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub soap_wsa_compl: Option<i8>,

    // These are already enum types, so they're validated by the type system
    #[serde(rename = "importance")]
    #[garde(skip)]
    pub importance: Option<Importance>,

    #[garde(inner(range(min = -1, max = 1)))]
    pub mqtt_enabled: Option<i8>,

    #[garde(inner(length(chars, max = 255)))]
    pub mqtt_subscriptions: Option<String>,

    #[garde(skip)] // i32 doesn't need validation in this case
    pub startup_delay: Option<i32>,

    #[serde(rename = "analysing")]
    #[garde(skip)]
    pub analysing: Option<Analysing>,

    #[garde(skip)]
    pub analysis_source: Option<AnalysisSource>,

    #[garde(skip)]
    pub analysis_image: Option<AnalysisImage>,

    #[garde(skip)]
    pub recording: Option<Recording>,

    // --- Columns the response returns that had no request field (#159) ---
    /// Serve the monitor through go2rtc.
    #[garde(inner(range(min = 0, max = 1)))]
    pub go2rtc_enabled: Option<i8>,
    /// Stamp events with wall-clock rather than stream timestamps.
    #[garde(inner(range(min = 0, max = 1)))]
    pub wall_clock_timestamps: Option<i8>,
    /// Player the web UI uses for this monitor by default.
    #[garde(inner(length(chars, max = 64)))]
    pub default_player: Option<String>,
    /// Hardware encoder (`EncoderHWAccelName`).
    #[garde(inner(length(chars, max = 64)))]
    pub encoder_hw_accel_name: Option<String>,
    /// Hardware encoder device (`EncoderHWAccelDevice`).
    #[garde(inner(length(chars, max = 255)))]
    pub encoder_hw_accel_device: Option<String>,
    /// Stream source channel.
    #[garde(skip)]
    pub stream_channel: Option<crate::entity::sea_orm_active_enums::StreamChannel>,
    /// What the live view renders: video, audio visualization, or both.
    #[garde(skip)]
    pub what_display: Option<crate::entity::sea_orm_active_enums::WhatDisplay>,

    // --- Object detection (ZoneMinder 1.39.17 AI data model) ---
    /// Alpha applied when compositing the analysis image over the frame.
    #[garde(inner(range(min = 0, max = 255)))]
    pub analysis_image_opacity: Option<u8>,
    /// Detector to use, or `none` to disable object detection.
    #[garde(inner(length(max = 16)))]
    pub object_detection: Option<String>,
    /// Name of the AI model used for object detection.
    #[garde(inner(length(max = 255)))]
    pub object_detection_model: Option<String>,
    /// Object confidence threshold (0.0-1.0).
    #[garde(inner(range(min = 0.0, max = 1.0)))]
    pub object_detection_object_threshold: Option<f32>,
    /// Non-maximum-suppression threshold (0.0-1.0).
    #[garde(inner(range(min = 0.0, max = 1.0)))]
    pub object_detection_nms_threshold: Option<f32>,
}

impl Default for CreateMonitorRequest {
    /// ZoneMinder-sensible, **validation-passing** defaults for every field, so
    /// callers (e.g. ONVIF onboarding) can build a valid monitor by overriding
    /// only the handful they care about: `name`, `type`, `path`, `width`,
    /// `height`, `storage_id`, and the `onvif_*` fields. The
    /// `create_monitor_request_default_passes_validation` test guards that this
    /// satisfies every `garde` constraint.
    fn default() -> Self {
        Self {
            // Placeholder so the Default itself validates (min length 1); real
            // callers (e.g. onboarding) always override this.
            name: "Monitor".to_string(),
            deleted: Some(false),
            enabled: None,
            notes: None,
            server_id: None,
            storage_id: Some(1),
            manufacturer_id: None,
            model_id: None,
            r#type: MonitorType::Ffmpeg,
            function: Some(Function::Monitor),
            capturing: Some(Capturing::Always),
            decoding_enabled: Some(1),
            decoding: Some(Decoding::Always),
            rtsp2_web_enabled: Some(0),
            rtsp2_web_type: Some(Rtsp2WebType::Hls),
            janus_enabled: Some(0),
            janus_audio_enabled: Some(0),
            janus_profile_override: None,
            restream: Some(0),
            rtsp_user: None,
            janus_rtsp_session_timeout: None,
            linked_monitors: None,
            triggers: Some(String::new()),
            event_start_command: Some(String::new()),
            event_end_command: Some(String::new()),
            onvif_url: Some(String::new()),
            onvif_events_path: Some(String::new()),
            onvif_username: Some(String::new()),
            onvif_password: Some(String::new()),
            onvif_options: Some(String::new()),
            onvif_event_listener: Some(0),
            onvif_alarm_text: None,
            use_amcrest_api: Some(0),
            device: Some(String::new()),
            channel: Some(0),
            format: Some(0),
            v4l_multi_buffer: None,
            v4l_captures_per_frame: None,
            protocol: None,
            method: None,
            host: None,
            port: Some(String::new()),
            sub_path: Some(String::new()),
            path: None,
            second_path: None,
            options: None,
            user: None,
            pass: None,
            width: Some(1),
            height: Some(1),
            colours: Some(4),
            palette: Some(0),
            orientation: Some(Orientation::Rotate0),
            deinterlacing: Some(0),
            decoder: None,
            decoder_hw_accel_name: None,
            decoder_hw_accel_device: None,
            save_jpe_gs: Some(1),
            video_writer: Some(1),
            output_codec: None,
            encoder: None,
            output_container: Some(OutputContainer::Auto),
            encoder_parameters: None,
            record_audio: Some(1),
            recording_source: Some(RecordingSource::Primary),
            rtsp_describe: None,
            brightness: Some(0),
            contrast: Some(0),
            hue: Some(0),
            colour: Some(0),
            event_prefix: Some("Event-".to_string()),
            label_format: None,
            label_x: Some(0),
            label_y: Some(0),
            label_size: Some(1),
            image_buffer_count: Some(3),
            max_image_buffer_count: Some(3),
            warmup_count: Some(0),
            pre_event_count: Some(5),
            post_event_count: Some(5),
            stream_replay_buffer: Some(1000),
            alarm_frame_count: Some(1),
            section_length: Some(600),
            section_length_warn: Some(0),
            event_close_mode: Some(EventCloseMode::Idle),
            min_section_length: Some(10),
            motion_frame_skip: Some(0),
            device_class: None,
            audio_detection: None,
            audio_threshold: None,
            audio_alarm_score: None,
            analysis_fps_limit: None,
            analysis_update_delay: Some(0),
            max_fps: None,
            alarm_max_fps: None,
            fps_report_interval: Some(100),
            ref_blend_perc: Some(6),
            alarm_ref_blend_perc: Some(6),
            controllable: Some(0),
            control_id: None,
            control_device: None,
            control_address: None,
            auto_stop_timeout: None,
            track_motion: Some(0),
            track_delay: None,
            return_location: Some(-1),
            return_delay: None,
            modect_during_ptz: Some(0),
            default_rate: Some(100),
            default_scale: Some("0".to_string()),
            default_codec: Some(DefaultCodec::Auto),
            signal_check_points: Some(0),
            signal_check_colour: Some(String::new()),
            web_colour: Some(String::new()),
            exif: Some(0),
            sequence: None,
            zone_count: Some(0),
            refresh: None,
            latitude: None,
            longitude: None,
            rtsp_server: Some(0),
            rtsp_stream_name: Some(String::new()),
            soap_wsa_compl: Some(0),
            importance: Some(Importance::Normal),
            mqtt_enabled: Some(0),
            mqtt_subscriptions: Some(String::new()),
            startup_delay: Some(0),
            analysing: Some(Analysing::None),
            analysis_source: Some(AnalysisSource::Primary),
            analysis_image: Some(AnalysisImage::FullColour),
            recording: Some(Recording::Always),
            go2rtc_enabled: None,
            wall_clock_timestamps: None,
            default_player: None,
            encoder_hw_accel_name: None,
            encoder_hw_accel_device: None,
            stream_channel: None,
            what_display: None,
            analysis_image_opacity: None,
            object_detection: None,
            object_detection_model: None,
            object_detection_object_threshold: None,
            object_detection_nms_threshold: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema, Validate)]
pub struct UpdateMonitorRequest {
    #[garde(length(chars, min = 1, max = 64))]
    pub name: Option<String>,

    // Boolean to match CreateMonitorRequest and MonitorResponse (GH #18).
    #[garde(skip)]
    pub deleted: Option<bool>,

    /// Enable/disable the monitor (ZoneMinder's `Monitors.Enabled`).
    #[garde(range(min = 0, max = 1))]
    pub enabled: Option<u8>,

    // --- Object detection (ZoneMinder 1.39.17 AI data model) ---
    /// Alpha applied when compositing the analysis image over the frame.
    #[garde(range(min = 0, max = 255))]
    pub analysis_image_opacity: Option<u8>,
    /// Detector to use, or `none` to disable object detection.
    #[garde(length(max = 16))]
    pub object_detection: Option<String>,
    /// Name of the AI model used for object detection.
    #[garde(length(max = 255))]
    pub object_detection_model: Option<String>,
    /// Object confidence threshold (0.0-1.0).
    #[garde(range(min = 0.0, max = 1.0))]
    pub object_detection_object_threshold: Option<f32>,
    /// Non-maximum-suppression threshold (0.0-1.0).
    #[garde(range(min = 0.0, max = 1.0))]
    pub object_detection_nms_threshold: Option<f32>,

    #[garde(skip)]
    pub notes: Option<String>,

    #[garde(skip)]
    pub server_id: Option<u32>,

    // 0 = ZoneMinder default storage (GH #19).
    #[garde(range(min = 0))]
    pub storage_id: Option<u16>,

    #[garde(skip)]
    pub manufacturer_id: Option<u32>,

    #[garde(skip)]
    pub model_id: Option<u32>,

    #[serde(rename = "type")]
    #[garde(skip)]
    pub r#type: Option<MonitorType>,

    #[serde(rename = "function")]
    #[garde(skip)]
    pub function: Option<Function>,

    #[serde(rename = "capturing")]
    #[garde(skip)]
    pub capturing: Option<Capturing>,

    #[garde(range(min = 0, max = 1))]
    pub decoding_enabled: Option<u8>,

    #[serde(rename = "decoding")]
    #[garde(skip)]
    pub decoding: Option<Decoding>,

    #[garde(range(min = -1, max = 1))]
    pub rtsp2_web_enabled: Option<i8>,

    #[serde(rename = "rtsp2_web_type")]
    #[garde(skip)]
    pub rtsp2_web_type: Option<Rtsp2WebType>,

    #[garde(range(min = -1, max = 1))]
    pub janus_enabled: Option<i8>,

    #[garde(range(min = -1, max = 1))]
    pub janus_audio_enabled: Option<i8>,

    #[garde(inner(length(chars, max = 30)))]
    pub janus_profile_override: Option<String>,

    #[garde(range(min = -1, max = 1))]
    pub restream: Option<i8>,

    #[garde(skip)]
    pub rtsp_user: Option<i32>,

    #[garde(skip)]
    pub janus_rtsp_session_timeout: Option<i32>,

    #[garde(inner(length(chars, max = 255)))]
    pub linked_monitors: Option<String>,

    #[garde(length(min = 0))]
    pub triggers: Option<String>,

    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_command_string)))]
    pub event_start_command: Option<String>,

    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_command_string)))]
    pub event_end_command: Option<String>,

    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_onvif_url)))]
    pub onvif_url: Option<String>,

    #[garde(inner(length(chars, max = 20)))]
    pub onvif_events_path: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub onvif_username: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub onvif_password: Option<String>,

    #[garde(inner(length(chars, max = 255)))]
    pub onvif_options: Option<String>,

    #[garde(range(min = -1, max = 1))]
    pub onvif_event_listener: Option<i8>,

    #[garde(inner(length(chars, max = 30)))]
    pub onvif_alarm_text: Option<String>,

    #[garde(range(min = -1, max = 1))]
    pub use_amcrest_api: Option<i8>,

    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_device_path)))]
    pub device: Option<String>,

    #[garde(range(min = 0))]
    pub channel: Option<u8>,

    #[garde(skip)]
    pub format: Option<u32>,

    #[garde(skip)]
    pub v4l_multi_buffer: Option<u8>,

    #[garde(skip)]
    pub v4l_captures_per_frame: Option<u8>,

    #[garde(inner(length(chars, max = 16)))]
    pub protocol: Option<String>,

    #[garde(inner(length(chars, max = 16)))]
    pub method: Option<String>,

    #[garde(inner(length(max = 64)))]
    #[garde(inner(custom(is_safe_url_component)))]
    pub host: Option<String>,

    #[garde(inner(length(max = 8)))]
    #[garde(inner(custom(is_safe_url_component)))]
    pub port: Option<String>,

    #[garde(inner(length(max = 64)))]
    #[garde(inner(custom(is_safe_url_component)))]
    pub sub_path: Option<String>,

    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_capture_path)))]
    pub path: Option<String>,

    #[garde(inner(length(max = 255)))]
    #[garde(inner(custom(is_safe_capture_path)))]
    pub second_path: Option<String>,

    #[garde(inner(length(chars, max = 255)))]
    pub options: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub user: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub pass: Option<String>,

    #[garde(range(min = 1))]
    pub width: Option<u16>,

    #[garde(range(min = 1))]
    pub height: Option<u16>,

    #[garde(range(min = 0))]
    pub colours: Option<u8>,

    #[garde(skip)]
    pub palette: Option<u32>,

    #[serde(rename = "orientation")]
    #[garde(skip)]
    pub orientation: Option<Orientation>,

    #[garde(skip)]
    pub deinterlacing: Option<u32>,

    #[garde(inner(length(chars, max = 32)))]
    pub decoder: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub decoder_hw_accel_name: Option<String>,

    #[garde(inner(length(chars, max = 255)))]
    pub decoder_hw_accel_device: Option<String>,

    /// `Monitors.SaveJPEGs` is a two-bit mask, not a flag: bit 0 saves
    /// captured frames and bit 1 saves analysis images, so 3 means both — and
    /// 3 is the column default, which the old `max = 1` rejected (GH #39).
    #[garde(range(min = 0, max = 3))]
    pub save_jpe_gs: Option<i8>,

    /// 0 disabled, 1 encode, 2 camera passthrough.
    #[garde(range(min = 0, max = 2))]
    pub video_writer: Option<i8>,

    #[garde(skip)]
    pub output_codec: Option<u32>,

    #[garde(inner(length(chars, max = 32)))]
    pub encoder: Option<String>,

    #[serde(rename = "output_container")]
    #[garde(skip)]
    pub output_container: Option<OutputContainer>,

    #[garde(skip)]
    pub encoder_parameters: Option<String>,

    /// Boolean. The old `min = -1` accepted a value the column has no meaning for.
    #[garde(range(min = 0, max = 1))]
    pub record_audio: Option<i8>,

    #[serde(rename = "recording_source")]
    #[garde(skip)]
    pub recording_source: Option<RecordingSource>,

    /// Accepted and ignored: ZoneMinder 1.39.37 removed `RTSPDescribe`.
    #[garde(skip)]
    pub rtsp_describe: Option<u8>,

    #[garde(range(min = -1, max = i32::MAX))]
    pub brightness: Option<i32>,

    #[garde(range(min = -1, max = i32::MAX))]
    pub contrast: Option<i32>,

    #[garde(range(min = -1, max = i32::MAX))]
    pub hue: Option<i32>,

    #[garde(range(min = -1, max = i32::MAX))]
    pub colour: Option<i32>,

    #[garde(inner(length(chars, max = 32)))]
    pub event_prefix: Option<String>,

    #[garde(inner(length(chars, max = 64)))]
    pub label_format: Option<String>,

    #[garde(skip)]
    pub label_x: Option<u16>,

    #[garde(skip)]
    pub label_y: Option<u16>,

    #[garde(range(min = 1))]
    pub label_size: Option<u16>,

    #[garde(range(min = 1))]
    pub image_buffer_count: Option<u16>,

    // 0 = unlimited (GH #19).
    #[garde(range(min = 0))]
    pub max_image_buffer_count: Option<u16>,

    #[garde(skip)]
    pub warmup_count: Option<u16>,

    #[garde(skip)]
    pub pre_event_count: Option<u16>,

    #[garde(skip)]
    pub post_event_count: Option<u16>,

    #[garde(range(min = 0))]
    pub stream_replay_buffer: Option<u32>,

    #[garde(range(min = 1))]
    pub alarm_frame_count: Option<u16>,

    #[garde(range(min = 1))]
    pub section_length: Option<u32>,

    #[garde(range(min = -1, max = 1))]
    pub section_length_warn: Option<i8>,

    #[serde(rename = "event_close_mode")]
    #[garde(skip)]
    pub event_close_mode: Option<EventCloseMode>,

    #[garde(range(min = 1))]
    pub min_section_length: Option<u32>,

    #[garde(skip)]
    pub motion_frame_skip: Option<u16>,

    #[garde(skip)]
    pub device_class: Option<DeviceClass>,
    #[garde(skip)]
    pub audio_detection: Option<u8>,
    #[garde(skip)]
    pub audio_threshold: Option<u8>,
    #[garde(skip)]
    pub audio_alarm_score: Option<u16>,

    #[garde(skip)]
    pub analysis_fps_limit: Option<f64>,

    #[garde(skip)]
    pub analysis_update_delay: Option<u16>,

    #[garde(skip)]
    pub max_fps: Option<f64>,

    #[garde(skip)]
    pub alarm_max_fps: Option<f64>,

    #[garde(skip)]
    pub fps_report_interval: Option<u16>,

    #[garde(range(min = 0, max = 100))]
    pub ref_blend_perc: Option<u8>,

    #[garde(range(min = 0, max = 100))]
    pub alarm_ref_blend_perc: Option<u8>,

    #[garde(range(min = 0, max = 1))]
    pub controllable: Option<u8>,

    #[garde(skip)]
    pub control_id: Option<u32>,

    #[garde(inner(length(chars, max = 255)))]
    pub control_device: Option<String>,

    #[garde(inner(length(chars, max = 255)))]
    pub control_address: Option<String>,

    #[garde(skip)]
    pub auto_stop_timeout: Option<f64>,

    #[garde(range(min = 0, max = 1))]
    pub track_motion: Option<u8>,

    #[garde(skip)]
    pub track_delay: Option<u16>,

    #[garde(range(min = -1, max = 1))]
    pub return_location: Option<i8>,

    #[garde(skip)]
    pub return_delay: Option<u16>,

    #[garde(range(min = 0, max = 1))]
    pub modect_during_ptz: Option<u8>,

    #[garde(skip)]
    pub default_rate: Option<u16>,

    #[garde(inner(length(chars, max = 16)))]
    pub default_scale: Option<String>,

    #[serde(rename = "default_codec")]
    #[garde(skip)]
    pub default_codec: Option<DefaultCodec>,

    #[garde(skip)]
    pub signal_check_points: Option<u32>,

    #[garde(inner(length(chars, max = 32)))]
    pub signal_check_colour: Option<String>,

    #[garde(inner(length(chars, max = 32)))]
    pub web_colour: Option<String>,

    #[garde(range(min = 0, max = 1))]
    pub exif: Option<u8>,

    #[garde(skip)]
    pub sequence: Option<u16>,

    #[garde(range(min = -128, max = 127))]
    pub zone_count: Option<i8>,

    #[garde(skip)]
    pub refresh: Option<u32>,

    #[garde(skip)]
    pub latitude: Option<f64>,

    #[garde(skip)]
    pub longitude: Option<f64>,

    #[garde(range(min = -1, max = 1))]
    pub rtsp_server: Option<i8>,

    #[garde(inner(length(chars, max = 255)))]
    pub rtsp_stream_name: Option<String>,

    #[garde(range(min = -1, max = 1))]
    pub soap_wsa_compl: Option<i8>,

    #[serde(rename = "importance")]
    #[garde(skip)]
    pub importance: Option<Importance>,

    #[garde(range(min = -1, max = 1))]
    pub mqtt_enabled: Option<i8>,

    #[garde(inner(length(chars, max = 255)))]
    pub mqtt_subscriptions: Option<String>,

    #[garde(skip)]
    pub startup_delay: Option<i32>,

    #[serde(rename = "analysing")]
    #[garde(skip)]
    pub analysing: Option<Analysing>,

    #[serde(rename = "analysis_source")]
    #[garde(skip)]
    pub analysis_source: Option<AnalysisSource>,

    #[serde(rename = "analysis_image")]
    #[garde(skip)]
    pub analysis_image: Option<AnalysisImage>,

    #[serde(rename = "recording")]
    #[garde(skip)]
    pub recording: Option<Recording>,

    // --- Columns the response returns that had no request field (#159) ---
    /// Serve the monitor through go2rtc.
    #[garde(inner(range(min = 0, max = 1)))]
    pub go2rtc_enabled: Option<i8>,
    /// Stamp events with wall-clock rather than stream timestamps.
    #[garde(inner(range(min = 0, max = 1)))]
    pub wall_clock_timestamps: Option<i8>,
    /// Player the web UI uses for this monitor by default.
    #[garde(inner(length(chars, max = 64)))]
    pub default_player: Option<String>,
    /// Hardware encoder (`EncoderHWAccelName`).
    #[garde(inner(length(chars, max = 64)))]
    pub encoder_hw_accel_name: Option<String>,
    /// Hardware encoder device (`EncoderHWAccelDevice`).
    #[garde(inner(length(chars, max = 255)))]
    pub encoder_hw_accel_device: Option<String>,
    /// Stream source channel.
    #[garde(skip)]
    pub stream_channel: Option<crate::entity::sea_orm_active_enums::StreamChannel>,
    /// What the live view renders: video, audio visualization, or both.
    #[garde(skip)]
    pub what_display: Option<crate::entity::sea_orm_active_enums::WhatDisplay>,
}

/// Body of `PATCH /monitors/{id}/state`. Distinct from the run-state
/// `UpdateStateRequest` in `dto::request::states`: the two shared a name, and
/// the OpenAPI spec documented this endpoint with the other one's body (#158).
#[derive(Debug, Serialize, Deserialize, ToSchema, Validate)]
pub struct MonitorStateRequest {
    #[garde(custom(is_valid_state))]
    pub state: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema, Validate)]
pub struct AlarmControlRequest {
    /// Action to perform: "on" (force an alarm), "off"/"cancel" (cancel a
    /// forced alarm), "disable_alarms" (force alarms off until
    /// "enable_alarms", ZoneMinder's `zmu -n` / `zmu -c`), "status"
    #[garde(custom(is_valid_alarm_action))]
    pub action: String,

    /// Alarm score (1-100+). Only used when action is "on". Default: 100
    #[garde(skip)]
    #[serde(default)]
    pub score: Option<u32>,

    /// Short cause string (max 31 chars). Only used when action is "on". Default: "API"
    #[garde(length(max = 31))]
    #[serde(default)]
    pub cause: Option<String>,

    /// Description text (max 255 chars). Only used when action is "on".
    #[garde(length(max = 255))]
    #[serde(default)]
    pub text: Option<String>,
}

#[cfg(test)]
mod validator_tests {
    use super::*;

    #[test]
    fn capture_path_refuses_sources_that_run_programs() {
        for bad in [
            "exec:ffmpeg -i x -f rtsp {output}",
            "echo:/bin/sh -c id",
            "expr:let x = 1",
            "ffmpeg:rtsp://cam#raw=-y /tmp/x",
            "file:///etc/passwd",
            "pipe:0",
            "concat:/etc/passwd",
            "admin:pw@10.0.0.5/stream",
        ] {
            assert!(is_safe_capture_path(bad, &()).is_err(), "{bad}");
        }
    }

    #[test]
    fn capture_path_accepts_camera_urls_and_local_paths() {
        for good in [
            "",
            "rtsp://user:pw@10.0.0.5:554/Streaming/Channels/101",
            "RTSPS://cam.example/live",
            "http://10.0.0.5/mjpg/video.mjpg",
            "/axis-cgi/mjpg/video.cgi",
            "/var/lib/zm/sample.mp4",
        ] {
            assert!(is_safe_capture_path(good, &()).is_ok(), "{good}");
        }
    }

    #[test]
    fn device_path_must_be_a_plain_dev_path() {
        assert!(is_safe_device_path("", &()).is_ok());
        assert!(is_safe_device_path("/dev/video0", &()).is_ok());
        assert!(is_safe_device_path("/dev/v4l/by-id/usb-Cam_1.0-video-index0", &()).is_ok());
        for bad in [
            "/dev/video0'; id; '",
            "/dev/video0 $(id)",
            "/dev/../etc/passwd",
            "/tmp/x",
            "/dev/",
            "video0",
        ] {
            assert!(is_safe_device_path(bad, &()).is_err(), "{bad}");
        }
    }

    #[test]
    fn storage_path_must_be_shell_literal() {
        assert!(is_safe_capture_path("rtsp://127.0.0.1:8554/cam", &()).is_ok());
        assert!(is_safe_capture_path("http://169.254.169.254/latest/meta-data/", &()).is_err());
        assert!(is_safe_storage_path("", &()).is_ok());
        assert!(is_safe_storage_path("/var/cache/zoneminder/events", &()).is_ok());
        assert!(is_safe_storage_path("/mnt/nas-1/zm_events.2", &()).is_ok());
        for bad in [
            "/mnt/x; rm -rf /",
            "/mnt/$(id)",
            "/mnt/`id`",
            "/mnt/a b",
            "/mnt/x|id",
            "relative/path",
        ] {
            assert!(is_safe_storage_path(bad, &()).is_err(), "{bad}");
        }
    }

    #[test]
    fn control_protocol_must_be_a_bare_module_name() {
        assert!(is_perl_module_name("", &()).is_ok());
        assert!(is_perl_module_name("onvif", &()).is_ok());
        assert!(is_perl_module_name("Hikvision_ISAPI2", &()).is_ok());
        for bad in [
            "../../../../tmp/x",
            "Foo::Bar",
            "Foo'Bar",
            "x;system('id')",
            "a b",
        ] {
            assert!(is_perl_module_name(bad, &()).is_err(), "{bad}");
        }
    }

    #[test]
    fn url_component_rejects_control_chars_and_crlf() {
        assert!(is_safe_url_component("foo\nbar", &()).is_err());
        assert!(is_safe_url_component("foo\0bar", &()).is_err());
        assert!(is_safe_url_component("foo\rbar", &()).is_err());
        assert!(is_safe_url_component("foo\x1bbar", &()).is_err());
        assert!(is_safe_url_component("foo\x7fbar", &()).is_err());
    }

    #[test]
    fn url_component_accepts_typical_camera_values() {
        assert!(is_safe_url_component("192.168.1.50", &()).is_ok());
        assert!(is_safe_url_component("cam.local:8080", &()).is_ok());
        assert!(is_safe_url_component("/Streaming/Channels/101", &()).is_ok());
        assert!(is_safe_url_component("", &()).is_ok());
    }

    #[test]
    fn url_component_enforces_length_cap() {
        let too_long = "a".repeat(2049);
        assert!(is_safe_url_component(&too_long, &()).is_err());
        let at_cap = "a".repeat(2048);
        assert!(is_safe_url_component(&at_cap, &()).is_ok());
    }

    #[test]
    fn onvif_url_rejects_dangerous_schemes() {
        // The threat is SSRF from ZoneMinder's capture daemon making
        // outbound requests via the URL, so block schemes that resolve to
        // local FS, Unix sockets, or other gadgets. `javascript:` is
        // intentionally not covered here — the daemon won't render it; that
        // class belongs to whoever renders the URL in the UI.
        assert!(is_safe_onvif_url("file:///etc/passwd", &()).is_err());
        assert!(is_safe_onvif_url("unix:///var/run/foo.sock", &()).is_err());
        assert!(is_safe_onvif_url("gopher://attacker/x", &()).is_err());
        assert!(is_safe_onvif_url("ftp://example.com/", &()).is_err());
    }

    #[test]
    fn onvif_url_accepts_camera_schemes() {
        assert!(is_safe_onvif_url("http://192.168.1.50/onvif", &()).is_ok());
        assert!(is_safe_onvif_url("HTTPS://CAM.LOCAL:8080/x", &()).is_ok());
        assert!(is_safe_onvif_url("rtsp://cam:554/stream", &()).is_ok());
        assert!(is_safe_onvif_url("rtsps://cam:443/stream", &()).is_ok());
        // No scheme at all is also fine — the field has historically accepted
        // bare host:port style strings.
        assert!(is_safe_onvif_url("192.168.1.50", &()).is_ok());
        assert!(is_safe_onvif_url("", &()).is_ok());
    }

    #[test]
    fn command_string_rejects_nul_and_newlines_but_keeps_shell_metas() {
        // Shell metacharacters stay legal — the field IS designed to be
        // executed as a shell command by ZoneMinder.
        assert!(is_safe_command_string("echo hi | tee /tmp/log", &()).is_ok());
        assert!(is_safe_command_string("$(curl http://example.com)", &()).is_ok());
        assert!(is_safe_command_string("kill -9 $(pidof zmc)", &()).is_ok());
        assert!(is_safe_command_string("", &()).is_ok());

        // NUL + CRLF are rejected: they enable C-string truncation and log
        // forgery in the daemon that runs the command.
        assert!(is_safe_command_string("rm -rf /\n", &()).is_err());
        assert!(is_safe_command_string("echo \0", &()).is_err());
        assert!(is_safe_command_string("echo \rbad", &()).is_err());
    }

    #[test]
    fn command_string_enforces_length_cap() {
        let too_long = "a".repeat(4097);
        assert!(is_safe_command_string(&too_long, &()).is_err());
    }

    #[test]
    fn create_monitor_request_default_passes_validation() {
        // The Default impl must satisfy every `garde` constraint, so onboarding
        // (and any other caller) can build a valid monitor by overriding only a
        // few fields.
        super::CreateMonitorRequest::default()
            .validate()
            .expect("default CreateMonitorRequest must pass validation");
    }
}

#[cfg(test)]
mod parity_tests {
    use super::is_valid_alarm_action;

    /// GH #37: `cancel` is documented and handled by the service, but the
    /// validator used to reject it with a 422 before it ever got there.
    #[test]
    fn alarm_action_accepts_cancel() {
        for a in ["on", "off", "cancel", "status"] {
            assert!(is_valid_alarm_action(a, &()).is_ok(), "{a} should be valid");
        }
        assert!(is_valid_alarm_action("explode", &()).is_err());
    }

    /// GH #39: the three tinyint columns whose bounds were copy-pasted as
    /// `-1..=1`, none of which was right.
    ///
    /// `SaveJPEGs` matters most: it is a two-bit mask whose column default is
    /// **3**, so the old bound rejected ZoneMinder's own default — the same
    /// class of bug as #19. Values confirmed against the upstream monitor form,
    /// not inferred.
    ///
    /// Driven through JSON rather than a struct literal, so it exercises the
    /// path a request actually takes.
    #[test]
    fn monitor_bitmask_and_enum_bounds_match_the_columns() {
        use crate::dto::request::monitor::UpdateMonitorRequest;
        use garde::Validate;

        fn accepts(field: &str, value: i8) -> bool {
            let json = format!(r#"{{"{field}": {value}}}"#);
            let req: UpdateMonitorRequest = serde_json::from_str(&json).expect("deserialize");
            req.validate().is_ok()
        }

        // SaveJPEGs: 0 disabled, 1 frames, 2 analysis, 3 both — 3 is the
        // column default, which the old max = 1 rejected outright.
        for v in [0, 1, 2, 3] {
            assert!(accepts("save_jpe_gs", v), "SaveJPEGs {v} is a real value");
        }
        for v in [-1, 4] {
            assert!(!accepts("save_jpe_gs", v), "SaveJPEGs {v} is out of range");
        }

        // VideoWriter: 0 disabled, 1 encode, 2 camera passthrough.
        for v in [0, 1, 2] {
            assert!(
                accepts("video_writer", v),
                "VideoWriter {v} is a real value"
            );
        }
        for v in [-1, 3] {
            assert!(
                !accepts("video_writer", v),
                "VideoWriter {v} is out of range"
            );
        }

        // RecordAudio is a checkbox.
        for v in [0, 1] {
            assert!(accepts("record_audio", v));
        }
        for v in [-1, 2] {
            assert!(
                !accepts("record_audio", v),
                "RecordAudio {v} is out of range"
            );
        }
    }
}

/// Query parameters for listing monitors.
#[derive(Debug, Default, Deserialize, Serialize, ToSchema, Validate)]
pub struct MonitorListParams {
    /// Page number (1-indexed)
    #[schema(example = 1)]
    #[garde(range(min = 1))]
    pub page: Option<u64>,

    /// Number of items per page (max 1000)
    #[schema(example = 25)]
    #[garde(range(min = 1, max = 1000))]
    pub page_size: Option<u64>,

    /// Also list monitors ZoneMinder has soft-deleted (`Deleted = 1`), which
    /// stay in the table until their events are gone. Off by default.
    #[serde(default)]
    #[garde(skip)]
    pub include_deleted: bool,
}

impl MonitorListParams {
    pub fn pagination(&self) -> crate::dto::PaginationParams {
        crate::dto::PaginationParams {
            page: self.page,
            page_size: self.page_size,
        }
    }
}
