// Integration tests for monitors, events, and frames with a real database
// Run with: cargo test --test handlers_monitors_events_frames_integration -- --include-ignored
#![allow(clippy::needless_borrows_for_generic_args)]

mod common;

use axum::body::{self, Body};
use axum::http::{header, Request, StatusCode};
use common::fixtures::{
    cleanup_monitor_permissions, grant_monitor_permission, insert_monitor, insert_user_with_id,
    RowGuard,
};
use common::test_db::{get_test_db, test_prefix};
use sea_orm::{ActiveModelTrait, DatabaseConnection, DbErr, EntityTrait, Set};
use tower::ServiceExt;
use zm_api::dto::request::{AlarmControlRequest, CreateMonitorRequest, MonitorStateRequest};
use zm_api::dto::response::events::EventCountsByMonitorResponse;
use zm_api::dto::response::{
    EventResponse, FrameResponse, MonitorResponse, PaginatedEventsResponse,
    PaginatedFramesResponse, PaginatedMonitorsResponse,
};
use zm_api::entity::sea_orm_active_enums::{
    Analysing, AnalysisImage, AnalysisSource, Capturing, Decoding, DefaultCodec, EventCloseMode,
    Function, Importance, MonitorType, Orientation, OutputContainer, Recording, RecordingSource,
    Rtsp2WebType,
};
use zm_api::entity::{events, frames, monitors};

fn auth_header() -> String {
    bearer_for(1)
}

/// A Bearer header carrying superuser *feature* permissions but a specific user
/// id, so row-level `Monitors_Permissions` rows for that id drive the scope.
fn bearer_for(uid: u32) -> String {
    let token = zm_api::service::token::generate_tokens(
        "tester".to_string(),
        uid,
        zm_api::util::authz::UserPermissions::superuser(),
    )
    .expect("token")
    .access_token;
    format!("Bearer {}", token)
}

fn build_app(db: DatabaseConnection) -> axum::Router {
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    common::schema::checked(zm_api::routes::create_router_app(state))
}

/// GH #19: ZoneMinder's own default sentinels must pass create validation:
/// `-1` = leave camera setting alone, `0` = default storage / unlimited buffer.
#[test]
fn create_monitor_accepts_zoneminder_default_sentinels() {
    use garde::Validate;
    let mut req = build_create_monitor_request("SentinelMon".to_string());
    req.brightness = Some(-1);
    req.contrast = Some(-1);
    req.hue = Some(-1);
    req.colour = Some(-1);
    req.storage_id = Some(0);
    req.max_image_buffer_count = Some(0);
    req.stream_replay_buffer = Some(0);
    assert!(
        req.validate().is_ok(),
        "ZoneMinder default sentinel values must validate: {:?}",
        req.validate().err()
    );
}

fn build_create_monitor_request(name: String) -> CreateMonitorRequest {
    CreateMonitorRequest {
        name,
        deleted: Some(false),
        enabled: None,
        notes: None,
        server_id: None,
        storage_id: Some(1),
        manufacturer_id: None,
        model_id: None,
        r#type: MonitorType::Local,
        function: Some(Function::Monitor),
        capturing: Some(Capturing::None),
        decoding_enabled: Some(0),
        decoding: Some(Decoding::None),
        rtsp2_web_enabled: Some(0),
        rtsp2_web_type: Some(Rtsp2WebType::Hls),
        janus_enabled: Some(0),
        janus_audio_enabled: Some(0),
        janus_profile_override: Some(String::new()),
        restream: Some(0),
        rtsp_user: None,
        janus_rtsp_session_timeout: Some(0),
        linked_monitors: None,
        triggers: Some(String::new()),
        event_start_command: Some(String::new()),
        event_end_command: Some(String::new()),
        onvif_url: Some(String::new()),
        onvif_events_path: Some("/Events".to_string()),
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
        width: Some(640),
        height: Some(480),
        colours: Some(3),
        palette: Some(0),
        orientation: Some(Orientation::Rotate0),
        deinterlacing: Some(0),
        decoder: None,
        decoder_hw_accel_name: None,
        decoder_hw_accel_device: None,
        save_jpe_gs: Some(0),
        video_writer: Some(0),
        output_codec: Some(0),
        encoder: None,
        output_container: Some(OutputContainer::Auto),
        encoder_parameters: None,
        record_audio: Some(0),
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
        image_buffer_count: Some(1),
        max_image_buffer_count: Some(1),
        warmup_count: Some(0),
        pre_event_count: Some(0),
        post_event_count: Some(0),
        stream_replay_buffer: Some(1),
        alarm_frame_count: Some(1),
        section_length: Some(1),
        section_length_warn: Some(0),
        event_close_mode: Some(EventCloseMode::System),
        min_section_length: Some(1),
        motion_frame_skip: Some(0),
        device_class: None,
        audio_detection: None,
        audio_threshold: None,
        audio_alarm_score: None,
        analysis_fps_limit: None,
        analysis_update_delay: Some(0),
        max_fps: None,
        alarm_max_fps: None,
        fps_report_interval: Some(0),
        ref_blend_perc: Some(0),
        alarm_ref_blend_perc: Some(0),
        controllable: Some(0),
        control_id: None,
        control_device: None,
        control_address: None,
        auto_stop_timeout: None,
        track_motion: Some(0),
        track_delay: None,
        return_location: Some(0),
        return_delay: None,
        modect_during_ptz: Some(0),
        default_rate: Some(100),
        default_scale: Some("100".to_string()),
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
        recording: Some(Recording::None),
        ..Default::default()
    }
}

async fn create_monitor(app: &axum::Router, name: String) -> MonitorResponse {
    let create_body = serde_json::to_vec(&build_create_monitor_request(name))
        .expect("serialize monitor create request");
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v3/monitors")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("parse monitor response")
}

/// REVIEW_FIXES_PLAN §1.4: a monitor-restricted user must not be able to create
/// monitors, even though their *feature* permissions satisfy `Monitors:Edit`.
/// Creating a monitor has no pre-existing row to scope against, so a restricted
/// scope is rejected with 403.
#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_create_monitor_rejects_restricted_user() {
    use zm_api::entity::sea_orm_active_enums::Permission;

    const UID: u32 = 990_010;

    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");

    // A monitor the user is granted access to — this is what makes their scope
    // Restricted (any row flips a user from default-allow to allowlist).
    let granted = insert_monitor(&db, "CreateAclMon")
        .await
        .expect("insert monitor");
    let _g_mon = RowGuard::monitor(granted.id);
    insert_user_with_id(&db, UID, "CreateAclUser")
        .await
        .expect("insert user");
    let _g_user = RowGuard::user(UID);
    grant_monitor_permission(&db, granted.id, UID, Permission::View)
        .await
        .expect("grant permission");
    let _g_perms = RowGuard::new(
        format!("Monitors_Permissions(user={UID})"),
        move |db| async move {
            let _ = cleanup_monitor_permissions(&db, UID).await;
        },
    );

    let app = build_app(db);

    let create_body = serde_json::to_vec(&build_create_monitor_request(format!(
        "{}restricted_create",
        test_prefix()
    )))
    .expect("serialize monitor create request");

    let response = app
        .oneshot(
            Request::post("/api/v3/monitors")
                .header(header::AUTHORIZATION, bearer_for(UID))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

async fn get_monitor(app: &axum::Router, id: u32) -> MonitorResponse {
    let response = app
        .clone()
        .oneshot(
            Request::get(&format!("/api/v3/monitors/{}", id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("parse monitor response")
}

async fn create_monitor_db(db: &DatabaseConnection) -> Result<monitors::Model, DbErr> {
    let name = format!("{}monitor", test_prefix());
    let model = monitors::ActiveModel {
        name: Set(name),
        ..Default::default()
    };
    model.insert(db).await
}

async fn create_event_db(db: &DatabaseConnection, monitor_id: u32) -> Result<events::Model, DbErr> {
    let name = format!("{}event", test_prefix());
    let model = events::ActiveModel {
        monitor_id: Set(monitor_id),
        state_id: Set(1),
        name: Set(name),
        ..Default::default()
    };
    model.insert(db).await
}

async fn create_event_db_now(
    db: &DatabaseConnection,
    monitor_id: u32,
) -> Result<events::Model, DbErr> {
    let name = format!("{}event_now", test_prefix());
    let model = events::ActiveModel {
        monitor_id: Set(monitor_id),
        state_id: Set(1),
        name: Set(name),
        start_date_time: Set(Some(chrono::Utc::now().naive_utc())),
        ..Default::default()
    };
    model.insert(db).await
}

async fn create_frame_db(db: &DatabaseConnection, event_id: u64) -> Result<frames::Model, DbErr> {
    let model = frames::ActiveModel {
        event_id: Set(event_id),
        frame_id: Set(1),
        ..Default::default()
    };
    model.insert(db).await
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_monitors_create_update_delete() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    let app = build_app(db);

    let name = format!("{}create_monitor", test_prefix());
    let created = create_monitor(&app, name).await;
    let _g_monitor = RowGuard::monitor(created.id);
    let fetched = get_monitor(&app, created.id).await;
    assert_eq!(fetched.name, created.name);

    let updated_name = format!("{}updated_monitor", test_prefix());
    let update_body = serde_json::to_vec(&serde_json::json!({
        "name": updated_name
    }))
    .expect("serialize update body");
    let response = app
        .clone()
        .oneshot(
            Request::patch(&format!("/api/v3/monitors/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(update_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    let updated: MonitorResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(updated.id, created.id);
    assert_eq!(updated.name, updated_name);

    let fetched = get_monitor(&app, created.id).await;
    assert_eq!(fetched.name, updated_name);

    let response = app
        .oneshot(
            Request::delete(&format!("/api/v3/monitors/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_monitors_state_alarm() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    let app = build_app(db);

    let name = format!("{}state_monitor", test_prefix());
    let created = create_monitor(&app, name).await;
    let _g_monitor = RowGuard::monitor(created.id);

    let state_body = serde_json::to_vec(&MonitorStateRequest {
        state: "start".to_string(),
    })
    .expect("serialize state request");
    let response = app
        .clone()
        .oneshot(
            Request::patch(&format!("/api/v3/monitors/{}/state", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(state_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let alarm_body = serde_json::to_vec(&AlarmControlRequest {
        action: "status".to_string(),
        score: None,
        cause: None,
        text: None,
    })
    .expect("serialize alarm request");
    let response = app
        .clone()
        .oneshot(
            Request::patch(&format!("/api/v3/monitors/{}/alarm", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(alarm_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::delete(&format!("/api/v3/monitors/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_monitors_list_get() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    let monitor = create_monitor_db(&db)
        .await
        .expect("Failed to create monitor");
    let _g_monitor = RowGuard::monitor(monitor.id);
    let app = build_app(db);

    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v3/monitors")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    let body: PaginatedMonitorsResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(body.items.iter().any(|m| m.id == monitor.id));

    let response = app
        .oneshot(
            Request::get(&format!("/api/v3/monitors/{}", monitor.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    let body: MonitorResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body.id, monitor.id);
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_events_list_get() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    let monitor = create_monitor_db(&db)
        .await
        .expect("Failed to create monitor");
    let _g_monitor = RowGuard::monitor(monitor.id);
    let event = create_event_db(&db, monitor.id)
        .await
        .expect("Failed to create event");
    let event_id = event.id;
    let _g_event = RowGuard::new(format!("Events#{event_id}"), move |db| async move {
        let _ = events::Entity::delete_by_id(event_id).exec(&db).await;
    });
    let app = build_app(db);

    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v3/events?page=1&page_size=100")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    let body: PaginatedEventsResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(body.items.iter().any(|e| e.id == event.id));

    let response = app
        .oneshot(
            Request::get(&format!("/api/v3/events/{}", event.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    let body: EventResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body.id, event.id);
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_frames_list_get() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    let monitor = create_monitor_db(&db)
        .await
        .expect("Failed to create monitor");
    let _g_monitor = RowGuard::monitor(monitor.id);
    let event = create_event_db(&db, monitor.id)
        .await
        .expect("Failed to create event");
    let event_id = event.id;
    let _g_event = RowGuard::new(format!("Events#{event_id}"), move |db| async move {
        let _ = events::Entity::delete_by_id(event_id).exec(&db).await;
    });
    let frame = create_frame_db(&db, event.id)
        .await
        .expect("Failed to create frame");
    let key = (frame.event_id, frame.frame_id);
    let _g_frame = RowGuard::new(format!("Frames#{key:?}"), move |db| async move {
        let _ = frames::Entity::delete_by_id(key).exec(&db).await;
    });
    let app = build_app(db);

    let response = app
        .clone()
        .oneshot(
            Request::get(&format!("/api/v3/frames?event_id={}", event.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: PaginatedFramesResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(body.items.iter().any(|f| (f.event_id, f.frame_id) == key));

    let response = app
        .oneshot(
            Request::get(&format!("/api/v3/frames/{}/{}", key.0, key.1))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: FrameResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!((body.event_id, body.frame_id), key);
    assert_eq!(body.audio_level, 0, "AudioLevel defaults to 0");
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_events_counts_by_monitor_groups_correctly() {
    // Regression for the "no column found for name: monitor_id" 500 — the
    // raw SQL emitted by the aggregation builder must alias `MonitorId` to
    // `monitor_id` so `FromQueryResult` can deserialise it.
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");

    let mon_a = create_monitor_db(&db)
        .await
        .expect("Failed to create monitor A");
    let _g_mon_a = RowGuard::monitor(mon_a.id);
    let mon_b = create_monitor_db(&db)
        .await
        .expect("Failed to create monitor B");
    let _g_mon_b = RowGuard::monitor(mon_b.id);
    let ev_a1 = create_event_db_now(&db, mon_a.id).await.expect("event A1");
    let ev_a2 = create_event_db_now(&db, mon_a.id).await.expect("event A2");
    let ev_b1 = create_event_db_now(&db, mon_b.id).await.expect("event B1");
    let event_ids = [ev_a1.id, ev_a2.id, ev_b1.id];
    let _g_events = RowGuard::new("Events#counts", move |db| async move {
        for id in event_ids {
            let _ = events::Entity::delete_by_id(id).exec(&db).await;
        }
    });

    let app = build_app(db);

    let response = app
        .oneshot(
            Request::get("/api/v3/events/counts-by-monitor/24")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "body: {}",
        String::from_utf8_lossy(&bytes)
    );
    let body: EventCountsByMonitorResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body.hours, 24);
    let count_a = body
        .counts
        .iter()
        .find(|c| c.monitor_id == mon_a.id)
        .expect("monitor A row missing");
    let count_b = body
        .counts
        .iter()
        .find(|c| c.monitor_id == mon_b.id)
        .expect("monitor B row missing");
    assert!(count_a.count >= 2, "expected >=2 events for monitor A");
    assert!(count_b.count >= 1, "expected >=1 event for monitor B");
}
