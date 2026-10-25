use utoipa::{
    openapi::security::{Http, HttpAuthScheme, SecurityScheme},
    Modify, OpenApi,
};

use crate::dto::request::{
    AlarmControlRequest, CreateMonitorRequest, LoginRequest, MonitorStateRequest,
    RefreshTokenRequest, TokenInfoRequest, UpdateMonitorRequest,
};
use crate::dto::response::{
    LoginResponse, MessageResponse, MonitorPipelineResponse, MonitorResponse,
    ServiceStatusResponse, TokenResponse, VersionResponse,
};
use crate::dto::wrappers::*;
use crate::error::{AppError, AppResponseError};
use crate::util::claim::UserClaims;

#[derive(OpenApi)]
#[openapi(
    info(
        version = crate::constant::API_VERSION,
        title = "Zoneminder API",
    ),
    paths(
        // auth
        crate::handlers::auth::login,
        crate::handlers::auth::logout,
        crate::handlers::auth::refresh_token,
        crate::handlers::auth::me,
        crate::handlers::auth::change_password,

        // AI object-detection registry
        crate::handlers::ai::list_datasets,
        crate::handlers::ai::get_dataset,
        crate::handlers::ai::create_dataset,
        crate::handlers::ai::update_dataset,
        crate::handlers::ai::delete_dataset,
        crate::handlers::ai::list_models,
        crate::handlers::ai::get_model,
        crate::handlers::ai::create_model,
        crate::handlers::ai::update_model,
        crate::handlers::ai::delete_model,
        crate::handlers::ai::list_classes,
        crate::handlers::ai::get_class,
        crate::handlers::ai::create_class,
        crate::handlers::ai::update_class,
        crate::handlers::ai::delete_class,

        // config
        crate::handlers::configs::get_config,
        crate::handlers::configs::list_categories,
        crate::handlers::configs::list_configs,
        crate::handlers::configs::update_config,

        // control presets
        crate::handlers::control_presets::create_control_preset,
        crate::handlers::control_presets::delete_control_preset,
        crate::handlers::control_presets::get_control_preset,
        crate::handlers::control_presets::list_control_presets,
        crate::handlers::control_presets::update_control_preset,

        // controls
        crate::handlers::controls::create_control,
        crate::handlers::controls::delete_control,
        crate::handlers::controls::get_control,
        crate::handlers::controls::list_controls,
        crate::handlers::controls::update_control,

        // daemons
        crate::handlers::daemon::list_daemons,
        crate::handlers::daemon::get_daemon,
        crate::handlers::daemon::start_daemon,
        crate::handlers::daemon::stop_daemon,
        crate::handlers::daemon::restart_daemon,
        crate::handlers::daemon::reload_daemon,
        crate::handlers::daemon::get_system_status,
        crate::handlers::daemon::system_startup,
        crate::handlers::daemon::system_shutdown,
        crate::handlers::daemon::system_restart,
        crate::handlers::daemon::system_logrot,
        crate::handlers::daemon::apply_state,

        // devices
        crate::handlers::devices::create_device,
        crate::handlers::devices::delete_device,
        crate::handlers::devices::get_device,
        crate::handlers::devices::list_devices,
        crate::handlers::devices::update_device,

        // event data
        crate::handlers::event_data::create_event_data,
        crate::handlers::event_data::delete_event_data,
        crate::handlers::event_data::get_event_data,
        crate::handlers::event_data::list_event_data,
        crate::handlers::event_data::update_event_data,

        // events
        crate::handlers::events::get_event_counts,
        crate::handlers::events::get_event_counts_by_monitor,
        crate::handlers::events::create_event,
        crate::handlers::events::delete_event,
        crate::handlers::events::get_event,
        crate::handlers::events::list_events,
        crate::handlers::events::update_event,

        // event playback
        crate::handlers::events_playback::get_event_playlist,
        crate::handlers::synopsis::get_event_synopsis_review,
        crate::handlers::synopsis::get_event_synopsis_layout,
        crate::handlers::synopsis::get_event_synopsis,
        crate::handlers::synopsis::get_event_synopsis_mp4,
        crate::handlers::synopsis::get_range_synopsis,
        crate::handlers::media_links::create_event_link,
        crate::handlers::events_playback::get_event_video,
        crate::handlers::events_playback::get_event_stream_video,
        crate::handlers::events_playback::get_event_stream_media,
        crate::handlers::events_playback::get_event_thumbnail,
        crate::handlers::frame_images::get_event_frame_image,
        crate::handlers::events_playback::get_event_info,
        crate::handlers::events_playback::get_event_init,
        crate::handlers::events_playback::get_event_segment,

        // event summaries
        crate::handlers::event_summaries::list_event_summaries,
        crate::handlers::event_summaries::get_event_summary,

        // natural-language / semantic search
        crate::handlers::search::search_events,
        crate::handlers::search::similar_events,

        // events tags
        crate::handlers::events_tags::create_event_tag,
        crate::handlers::events_tags::delete_event_tag,
        crate::handlers::events_tags::get_event_tag,
        crate::handlers::events_tags::list_events_tags,

        // filters
        crate::handlers::filters::create_filter,
        crate::handlers::filters::delete_filter,
        crate::handlers::filters::get_filter,
        crate::handlers::filters::list_filters,
        crate::handlers::filters::update_filter,
        crate::handlers::filters::preview_filter,

        // frames
        crate::handlers::frames::create_frame,
        crate::handlers::frames::delete_frame,
        crate::handlers::frames::get_frame,
        crate::handlers::frames::list_frames,
        crate::handlers::frames::update_frame,

        // groups
        crate::handlers::groups::create_group,
        crate::handlers::groups::delete_group,
        crate::handlers::groups::get_group,
        crate::handlers::groups::list_groups,
        crate::handlers::groups::update_group,

        // groups monitors
        crate::handlers::groups_monitors::create_group_monitor,
        crate::handlers::groups_monitors::delete_group_monitor,
        crate::handlers::groups_monitors::get_group_monitor,
        crate::handlers::groups_monitors::list_groups_monitors,

        // groups permissions
        crate::handlers::groups_permissions::create_group_permission,
        crate::handlers::groups_permissions::delete_group_permission,
        crate::handlers::groups_permissions::get_group_permission,
        crate::handlers::groups_permissions::list_groups_permissions,
        crate::handlers::groups_permissions::update_group_permission,

        // live streaming (unified at /api/v3/live)
        crate::handlers::live::start_live_stream,
        crate::handlers::live::stop_live_stream,
        crate::handlers::live::get_live_stats,
        crate::handlers::live::list_live_sessions,
        crate::handlers::live::get_live_master_playlist,
        crate::handlers::event_stream::stream_events,
        crate::handlers::media_links::create_live_link,
        crate::handlers::whep::whep_offer,
        crate::handlers::whep::whep_trickle,
        crate::handlers::whep::whep_delete,
        crate::handlers::media_links::create_rtsp_link,
        crate::handlers::event_export::export_events,
        crate::handlers::media_jobs::generate_video,
        crate::handlers::media_jobs::list_videos,
        crate::handlers::media_jobs::download_video,
        crate::handlers::media_jobs::delete_video,
        crate::handlers::media_jobs::list_jobs,
        crate::handlers::media_jobs::get_job,
        crate::handlers::media_jobs::delete_job,
        crate::handlers::notifications::list_notifications,
        crate::handlers::notifications::get_notification,
        crate::handlers::notifications::create_notification,
        crate::handlers::notifications::update_notification,
        crate::handlers::notifications::delete_notification,
        crate::handlers::monitor_actions::list_actions,
        crate::handlers::monitor_actions::list_targets,
        crate::handlers::monitor_actions::get_action,
        crate::handlers::monitor_actions::create_action,
        crate::handlers::monitor_actions::update_action,
        crate::handlers::monitor_actions::delete_action,
        crate::handlers::monitor_actions::run_action,
        crate::handlers::settings_tables::list_encoder_templates,
        crate::handlers::settings_tables::get_encoder_template,
        crate::handlers::settings_tables::create_encoder_template,
        crate::handlers::settings_tables::update_encoder_template,
        crate::handlers::settings_tables::delete_encoder_template,
        crate::handlers::settings_tables::list_menu_items,
        crate::handlers::settings_tables::get_menu_item,
        crate::handlers::settings_tables::create_menu_item,
        crate::handlers::settings_tables::update_menu_item,
        crate::handlers::settings_tables::delete_menu_item,
        crate::handlers::settings_tables::get_navbar,
        crate::handlers::settings_tables::record_privacy,
        crate::handlers::monitor::get_audio_level,
        crate::handlers::ptz::light_on,
        crate::handlers::ptz::light_off,
        crate::handlers::ptz::indicator_light_on,
        crate::handlers::ptz::indicator_light_off,
        crate::handlers::ptz::audio_stop,
        crate::handlers::ptz::audio_volume_up,
        crate::handlers::ptz::audio_volume_down,
        crate::handlers::ptz::audio_play,
        crate::handlers::ptz::light_status,
        crate::handlers::event_audit::event_audit,
        crate::handlers::roles::list_roles,
        crate::handlers::roles::get_role,
        crate::handlers::roles::create_role,
        crate::handlers::roles::update_role,
        crate::handlers::roles::delete_role,
        crate::handlers::live::get_live_media_playlist,
        crate::handlers::live::get_live_init_segment,
        crate::handlers::live::get_live_segment,
        crate::handlers::live::get_live_sources,
        crate::handlers::live::get_monitor_snapshot,
        crate::handlers::live::take_monitor_snapshot,
        crate::handlers::live::describe_monitor,
        crate::handlers::live::stream_monitor_events,
        crate::handlers::live::webrtc_websocket_handler,

        // logs
        crate::handlers::logs::get_log,
        crate::handlers::logs::list_logs,
        crate::handlers::logs::clear_logs,
        crate::handlers::logs::list_log_components,

        // manufacturers
        crate::handlers::manufacturers::create_manufacturer,
        crate::handlers::manufacturers::delete_manufacturer,
        crate::handlers::manufacturers::get_manufacturer,
        crate::handlers::manufacturers::list_manufacturers,
        crate::handlers::manufacturers::update_manufacturer,

        // models
        crate::handlers::models::create_model,
        crate::handlers::models::delete_model,
        crate::handlers::models::get_model,
        crate::handlers::models::list_models,
        crate::handlers::models::update_model,

        // monitor
        crate::handlers::monitor::alarm_control,
        crate::handlers::monitor::create_monitor,
        crate::handlers::monitor::delete_monitor,
        crate::handlers::monitor::update_monitor,
        crate::handlers::monitor::list_monitors,
        crate::handlers::monitor::update_state,
        crate::handlers::monitor::get_monitor,
        crate::handlers::monitor_pipeline::get_monitor_pipeline,
        crate::handlers::monitor_pipeline::put_monitor_pipeline,
        crate::handlers::monitor_pipeline::delete_monitor_pipeline,
        crate::handlers::monitor_pipeline::validate_monitor_pipeline,
        crate::handlers::monitor_pipeline::get_monitor_zmnext_status,
        crate::handlers::monitor_pipeline::enable_monitor_zmnext,
        crate::handlers::monitor_pipeline::disable_monitor_zmnext,

        // monitor presets
        crate::handlers::monitor_presets::create_monitor_preset,
        crate::handlers::monitor_presets::delete_monitor_preset,
        crate::handlers::monitor_presets::get_monitor_preset,
        crate::handlers::monitor_presets::list_monitor_presets,
        crate::handlers::monitor_presets::update_monitor_preset,

        // monitor status
        crate::handlers::monitor_status::get_monitor_status,
        crate::handlers::monitor_status::list_monitor_statuses,
        crate::handlers::monitor_status::update_monitor_status,

        // monitors permissions
        crate::handlers::monitors_permissions::create_monitor_permission,
        crate::handlers::monitors_permissions::delete_monitor_permission,
        crate::handlers::monitors_permissions::get_monitor_permission,
        crate::handlers::monitors_permissions::list_monitors_permissions,
        crate::handlers::monitors_permissions::update_monitor_permission,

        // montage layouts
        crate::handlers::montage_layouts::create_montage_layout,
        crate::handlers::montage_layouts::delete_montage_layout,
        crate::handlers::montage_layouts::get_montage_layout,
        crate::handlers::montage_layouts::list_montage_layouts,
        crate::handlers::montage_layouts::update_montage_layout,

        // object types
        crate::handlers::object_types::create_object_type,
        crate::handlers::object_types::delete_object_type,
        crate::handlers::object_types::get_object_type,
        crate::handlers::object_types::list_object_types,
        crate::handlers::object_types::update_object_type,

        // ptz
        crate::handlers::ptz::get_status,
        crate::handlers::ptz::get_capabilities,
        crate::handlers::ptz::list_protocols,
        crate::handlers::ptz::move_up,
        crate::handlers::ptz::move_down,
        crate::handlers::ptz::move_left,
        crate::handlers::ptz::move_right,
        crate::handlers::ptz::move_up_left,
        crate::handlers::ptz::move_up_right,
        crate::handlers::ptz::move_down_left,
        crate::handlers::ptz::move_down_right,
        crate::handlers::ptz::move_stop,
        crate::handlers::ptz::zoom_in,
        crate::handlers::ptz::zoom_out,
        crate::handlers::ptz::zoom_stop,
        crate::handlers::ptz::focus_near,
        crate::handlers::ptz::focus_far,
        crate::handlers::ptz::focus_auto,
        crate::handlers::ptz::focus_stop,
        crate::handlers::ptz::iris_open,
        crate::handlers::ptz::iris_close,
        crate::handlers::ptz::iris_auto,
        crate::handlers::ptz::iris_stop,
        crate::handlers::ptz::power_wake,
        crate::handlers::ptz::power_sleep,
        crate::handlers::ptz::power_reset,
        crate::handlers::ptz::power_reboot,
        crate::handlers::ptz::goto_preset,
        crate::handlers::ptz::set_preset,
        crate::handlers::ptz::clear_preset,
        crate::handlers::ptz::goto_home,
        crate::handlers::ptz::move_absolute,
        crate::handlers::ptz::move_relative,

        // reports
        crate::handlers::reports::create_report,
        crate::handlers::reports::delete_report,
        crate::handlers::reports::get_report,
        crate::handlers::reports::list_reports,
        crate::handlers::reports::update_report,

        // server
        crate::handlers::server::change_state,
        crate::handlers::server::get_locale,
        crate::handlers::server::get_version,
        crate::handlers::server::health_check,

        // server stats
        crate::handlers::server_stats::create_server_stat,
        crate::handlers::server_stats::delete_server_stat,
        crate::handlers::server_stats::get_server_stat,
        crate::handlers::server_stats::list_server_stats,

        // servers
        crate::handlers::servers::create_server,
        crate::handlers::servers::delete_server,
        crate::handlers::servers::get_server,
        crate::handlers::servers::list_servers,
        crate::handlers::servers::update_server,

        // snapshots
        crate::handlers::snapshots::create_snapshot,
        crate::handlers::snapshots::delete_snapshot,
        crate::handlers::snapshots::get_snapshot,
        crate::handlers::snapshots::list_snapshots,
        crate::handlers::snapshots::update_snapshot,

        // snapshots events
        crate::handlers::snapshots_events::create_snapshot_event,
        crate::handlers::snapshots_events::delete_snapshot_event,
        crate::handlers::snapshots_events::get_snapshot_event,
        crate::handlers::snapshots_events::list_snapshot_events,

        // states
        crate::handlers::states::create_state,
        crate::handlers::states::delete_state,
        crate::handlers::states::get_state,
        crate::handlers::states::list_states,
        crate::handlers::states::update_state,

        // stats
        crate::handlers::stats::create_stat,
        crate::handlers::stats::delete_stat,
        crate::handlers::stats::get_stat,
        crate::handlers::stats::list_stats,
        crate::handlers::stats::update_stat,

        // storage
        crate::handlers::storage::create_storage,
        crate::handlers::storage::delete_storage,
        crate::handlers::storage::get_storage,
        crate::handlers::storage::list_storage,
        crate::handlers::storage::update_storage,

        // tags
        crate::handlers::tags::create_tag,
        crate::handlers::tags::delete_tag,
        crate::handlers::tags::get_tag,
        crate::handlers::tags::list_tags,
        crate::handlers::tags::update_tag,

        // triggers x10
        crate::handlers::triggers_x10::create_trigger_x10,
        crate::handlers::triggers_x10::delete_trigger_x10,
        crate::handlers::triggers_x10::get_trigger_x10,
        crate::handlers::triggers_x10::list_triggers_x10,
        crate::handlers::triggers_x10::update_trigger_x10,

        // user preferences
        crate::handlers::user_preferences::create_user_preference,
        crate::handlers::user_preferences::delete_user_preference,
        crate::handlers::user_preferences::get_user_preference,
        crate::handlers::user_preferences::list_user_preferences,
        crate::handlers::user_preferences::update_user_preference,

        // users
        crate::handlers::users::create_user,
        crate::handlers::users::delete_user,
        crate::handlers::users::get_user,
        crate::handlers::users::list_users,
        crate::handlers::users::update_user,

        // zone presets
        crate::handlers::zone_presets::create_zone_preset,
        crate::handlers::zone_presets::delete_zone_preset,
        crate::handlers::zone_presets::get_zone_preset,
        crate::handlers::zone_presets::list_zone_presets,
        crate::handlers::zone_presets::update_zone_preset,

        // zones
        crate::handlers::zones::create,
        crate::handlers::zones::delete,
        crate::handlers::zones::get,
        crate::handlers::zones::list_by_monitor,
        crate::handlers::zones::update,
    ),
    components(
        schemas(
            crate::dto::response::monitor_pipeline::ZmNextWorkerStatusResponse,
            crate::dto::response::monitor_pipeline::PipelineValidationResponse,
            crate::streaming::source::worker_status::WorkerStatus,
            crate::streaming::source::worker_status::StreamHealth,
            crate::streaming::source::worker_status::StreamState,
            crate::streaming::source::worker_status::StatusChange,
            crate::service::zmnext::control::PathError,
            // zm-next on-demand commands
            crate::dto::request::monitor_ondemand::DescribeNowRequest,
            crate::dto::response::monitor_ondemand::DescribeNowResponse,
            // auth
            AppError,
            AppResponseError,
            LoginRequest,
            LoginResponse,
            MessageResponse,
            RefreshTokenRequest,
            crate::dto::request::ChangePasswordRequest,
            crate::dto::response::MeResponse,
            TokenInfoRequest,
            TokenResponse,
            UserClaims,

            // config
            crate::dto::request::config::ConfigQueryParams,
            crate::dto::request::config::UpdateConfigRequest,
            crate::dto::response::config::CategoryCountResponse,
            crate::dto::response::config::ConfigResponse,

            // control presets
            crate::dto::request::control_presets::CreateControlPresetRequest,
            crate::dto::request::control_presets::UpdateControlPresetRequest,
            crate::dto::response::control_presets::ControlPresetResponse,

            // controls
            crate::dto::request::controls::CreateControlRequest,
            crate::dto::request::controls::UpdateControlRequest,
            crate::dto::response::controls::ControlResponse,

            // daemons
            crate::dto::request::daemon::ApplyStateRequest,
            crate::dto::request::daemon::StartDaemonRequest,
            crate::dto::response::daemon::DaemonActionResponse,
            crate::dto::response::daemon::DaemonListResponse,
            crate::dto::response::daemon::DaemonStatusResponse,
            crate::dto::response::daemon::SystemStatusResponse,

            // devices
            crate::dto::request::devices::CreateDeviceRequest,
            crate::dto::request::devices::UpdateDeviceRequest,
            crate::dto::response::devices::DeviceResponse,
            crate::dto::response::devices::PaginatedDevicesResponse,

            // event data
            crate::dto::request::event_data::CreateEventDataRequest,
            crate::dto::request::event_data::UpdateEventDataRequest,
            crate::dto::response::event_data::EventDataResponse,

            // events
            crate::dto::request::events::EventCreateRequest,
            crate::dto::request::events::EventQueryParams,
            crate::dto::request::events::EventSortField,
            crate::dto::request::events::SortDirection,
            crate::dto::request::events::TagOp,
            crate::dto::request::events::EventUpdateRequest,
            crate::dto::response::events::EventCountResponse,
            crate::dto::response::events::EventCountsResponse,
            crate::dto::response::events::EventCountsByMonitorResponse,
            crate::dto::response::events::MonitorEventCount,
            crate::dto::response::events::EventResponse,
            crate::dto::response::events::PaginatedEventsResponse,

            // event playback
            crate::handlers::events_playback::EventVideoInfo,

            // natural-language / semantic search
            crate::dto::request::search::SearchQueryParams,
            crate::dto::request::search::SimilarQueryParams,
            crate::dto::response::search::SearchCitation,
            crate::dto::response::search::SearchResponse,
            crate::dto::response::search::SimilarResponse,

            // event summaries
            crate::dto::response::event_summaries::EventSummaryResponse,

            // events tags
            crate::dto::request::events_tags::CreateEventTagRequest,
            crate::dto::response::events_tags::EventTagResponse,
            crate::dto::response::events_tags::TagSummary,
            crate::dto::response::events_tags::EventSummary,
            crate::dto::response::events_tags::TagDetailResponse,

            // filters
            crate::dto::request::filters::CreateFilterRequest,
            crate::dto::request::filters::UpdateFilterRequest,
            crate::dto::request::filter_ast::FilterQuery,
            crate::dto::request::filter_ast::FilterExpr,
            crate::dto::request::filter_ast::FilterField,
            crate::dto::request::filter_ast::FilterOp,
            crate::dto::request::filter_ast::MatchOp,
            crate::dto::request::filter_ast::FilterSort,
            crate::dto::response::filters::FilterResponse,

            // frames
            crate::dto::request::frames::CreateFrameRequest,
            crate::dto::request::frames::UpdateFrameRequest,
            crate::dto::response::frames::FrameResponse,

            // groups
            crate::dto::request::groups::CreateGroupRequest,
            crate::dto::response::groups::GroupResponse,
            crate::handlers::groups::UpdateGroupRequest,

            // groups monitors
            crate::dto::request::groups_monitors::CreateGroupMonitorRequest,
            crate::dto::response::groups_monitors::GroupMonitorResponse,

            // groups permissions
            crate::dto::request::groups_permissions::CreateGroupPermissionRequest,
            crate::dto::request::groups_permissions::UpdateGroupPermissionRequest,
            crate::dto::response::groups_permissions::GroupPermissionResponse,

            // live streaming
            crate::handlers::live::StartLiveRequest,
            crate::handlers::live::StartLiveResponse,
            crate::handlers::live::LiveStatsResponse,
            crate::handlers::live::LiveProtocolStatus,
            // WebRTC signaling WebSocket message envelope (see webrtc_websocket_handler)
            crate::handlers::live::WebRtcSignalingMessage,

            // logs
            crate::dto::request::logs::LogQueryParams,
            crate::dto::response::logs::LogResponse,
            crate::dto::response::logs::PaginatedLogsResponse,

            // manufacturers
            crate::dto::request::manufacturers::CreateManufacturerRequest,
            crate::dto::response::manufacturers::ManufacturerResponse,
            crate::handlers::manufacturers::UpdateManufacturerRequest,

            // models
            crate::dto::request::models::CreateModelRequest,
            crate::dto::response::models::ModelResponse,
            crate::handlers::models::UpdateModelRequest,

            // monitor
            AlarmControlRequest,
            MonitorPipelineResponse,
            CreateMonitorRequest,
            MonitorResponse,
            UpdateMonitorRequest,
            MonitorStateRequest,
            crate::dto::request::CreateMediaLinkRequest,
            crate::dto::response::MediaLinkResponse,
            crate::dto::response::RtspLinkResponse,
            crate::dto::request::event_export::ExportPart,
            crate::dto::request::media_jobs::VideoTransform,
            crate::dto::request::media_jobs::GenerateVideoRequest,
            crate::dto::response::media_jobs::MediaJobResponse,
            crate::dto::response::media_jobs::EventVideoResponse,
            crate::dto::request::event_export::EventExportRequest,
            crate::dto::request::notifications::PushPlatform,
            crate::dto::request::notifications::PushState,
            crate::dto::request::notifications::CreateNotificationRequest,
            crate::dto::request::notifications::UpdateNotificationRequest,
            crate::dto::response::notifications::NotificationResponse,
            crate::dto::request::monitor_actions::TriggerOn,
            crate::dto::request::monitor_actions::ActionType,
            crate::dto::request::monitor_actions::CreateMonitorActionRequest,
            crate::dto::request::monitor_actions::UpdateMonitorActionRequest,
            crate::dto::response::monitor_actions::MonitorActionResponse,
            crate::dto::response::monitor_actions::ActionTargetResponse,
            crate::dto::request::settings_tables::CreateEncoderTemplateRequest,
            crate::dto::request::settings_tables::UpdateEncoderTemplateRequest,
            crate::dto::request::settings_tables::CreateMenuItemRequest,
            crate::dto::request::settings_tables::UpdateMenuItemRequest,
            crate::dto::response::settings_tables::EncoderTemplateResponse,
            crate::dto::response::settings_tables::MenuItemResponse,
            crate::dto::request::settings_tables::PrivacyRequest,
            crate::dto::response::settings_tables::PrivacyResponse,
            crate::dto::response::monitor_status::AudioLevelResponse,
            crate::dto::response::ptz::LightStatusResponse,
            crate::dto::response::event_audit::EventAuditResponse,
            crate::dto::response::event_audit::MonitorEventAudit,
            crate::dto::request::roles::CreateRoleRequest,
            crate::dto::request::roles::UpdateRoleRequest,
            crate::dto::response::roles::RoleResponse,
            crate::dto::response::roles::PaginatedRolesResponse,

            // monitor presets
            crate::dto::request::monitor_presets::CreateMonitorPresetRequest,
            crate::dto::request::monitor_presets::UpdateMonitorPresetRequest,
            crate::dto::response::monitor_presets::MonitorPresetResponse,

            // monitor status
            crate::dto::request::monitor_status::UpdateMonitorStatusRequest,
            crate::dto::response::monitor_status::MonitorStatusResponse,

            // monitors permissions
            crate::dto::request::monitors_permissions::CreateMonitorPermissionRequest,
            crate::dto::request::monitors_permissions::UpdateMonitorPermissionRequest,
            crate::dto::response::monitors_permissions::MonitorPermissionResponse,

            // montage layouts
            crate::dto::request::montage_layouts::CreateMontageLayoutRequest,
            crate::dto::request::montage_layouts::UpdateMontageLayoutRequest,
            crate::dto::response::montage_layouts::MontageLayoutResponse,

            // object types
            crate::dto::request::object_types::CreateObjectTypeRequest,
            crate::dto::request::object_types::UpdateObjectTypeRequest,
            crate::dto::response::object_types::ObjectTypeResponse,

            // ptz
            crate::dto::request::ptz::PtzMoveRequest,
            crate::dto::request::ptz::PtzZoomRequest,
            crate::dto::request::ptz::PtzFocusRequest,
            crate::dto::request::ptz::PtzPresetRequest,
            crate::dto::request::ptz::PtzAbsoluteRequest,
            crate::dto::request::ptz::PtzRelativeRequest,
            crate::dto::response::ptz::PtzCommandResponse,
            crate::dto::response::ptz::PtzStatusResponse,
            crate::dto::response::ptz::PtzCapabilitiesResponse,
            crate::dto::response::ptz::PtzProtocolListResponse,
            crate::dto::response::ptz::PtzProtocolInfo,
            crate::ptz::capabilities::PtzCapabilities,
            crate::ptz::capabilities::PowerCapabilities,
            crate::ptz::capabilities::PanTiltCapabilities,
            crate::ptz::capabilities::AxisCapabilities,
            crate::ptz::capabilities::AxisRange,
            crate::ptz::capabilities::AxisStep,
            crate::ptz::capabilities::AxisSpeed,
            crate::ptz::capabilities::TurboSpeed,
            crate::ptz::capabilities::PresetCapabilities,
            crate::ptz::capabilities::ScanCapabilities,

            // reports
            crate::dto::request::reports::CreateReportRequest,
            crate::dto::request::reports::UpdateReportRequest,
            crate::dto::response::reports::ReportResponse,

            // AI object-detection registry
            crate::dto::response::ai::AiDatasetResponse,
            crate::dto::response::ai::AiModelResponse,
            crate::dto::response::ai::AiObjectClassResponse,
            crate::dto::response::ai::PaginatedAiDatasetsResponse,
            crate::dto::response::ai::PaginatedAiModelsResponse,
            crate::dto::response::ai::PaginatedAiObjectClassesResponse,
            crate::dto::request::ai::CreateAiDatasetRequest,
            crate::dto::request::ai::UpdateAiDatasetRequest,
            crate::dto::request::ai::CreateAiModelRequest,
            crate::dto::request::ai::UpdateAiModelRequest,
            crate::dto::request::ai::CreateAiObjectClassRequest,
            crate::dto::request::ai::UpdateAiObjectClassRequest,
            crate::entity::sea_orm_active_enums::Framework,
            // server
            ServiceStatusResponse,
            VersionResponse,
            crate::dto::response::LocaleResponse,

            // server stats
            crate::dto::request::server_stats::CreateServerStatRequest,
            crate::dto::response::server_stats::ServerStatResponse,

            // servers
            crate::dto::request::servers::CreateServerRequest,
            crate::dto::response::servers::ServerResponse,
            crate::dto::request::servers::UpdateServerRequest,

            // snapshots
            crate::dto::request::snapshots::CreateSnapshotRequest,
            crate::dto::request::snapshots::UpdateSnapshotRequest,
            crate::dto::response::snapshots::SnapshotResponse,

            // snapshots events
            crate::dto::request::snapshots_events::CreateSnapshotEventRequest,
            crate::dto::response::snapshots_events::SnapshotEventResponse,

            // states
            crate::dto::request::states::CreateStateRequest,
            crate::dto::request::states::UpdateStateRequest,
            crate::dto::response::states::StateResponse,

            // stats
            crate::dto::request::stats::CreateStatRequest,
            crate::dto::request::stats::UpdateStatRequest,
            crate::dto::response::stats::StatResponse,

            // storage
            crate::dto::request::storage::CreateStorageRequest,
            crate::dto::response::storage::PaginatedStorageResponse,
            crate::dto::response::storage::StorageResponse,
            crate::handlers::storage::UpdateStorageRequest,

            // streaming
            crate::dto::response::StreamEndpoints,

            // tags
            crate::dto::request::tags::CreateTagRequest,
            crate::dto::request::tags::UpdateTagRequest,
            crate::dto::response::tags::TagResponse,

            // triggers x10
            crate::dto::request::triggers_x10::CreateTriggerX10Request,
            crate::dto::request::triggers_x10::UpdateTriggerX10Request,
            crate::dto::response::triggers_x10::TriggerX10Response,

            // user preferences
            crate::dto::request::user_preferences::CreateUserPreferenceRequest,
            crate::dto::request::user_preferences::UpdateUserPreferenceRequest,
            crate::dto::response::user_preferences::UserPreferenceResponse,

            // users
            crate::dto::request::users::CreateUserRequest,
            crate::dto::response::users::UserResponse,
            crate::dto::request::UpdateUserRequest,
            crate::dto::request::users::UserPermissionsInput,

            // wrappers
            DateTimeWrapper,
            DecimalWrapper,
            NaiveDateTimeWrapper,
            SchemeWrapper,

            // zone presets
            crate::dto::response::zone_presets::ZonePresetResponse,
            crate::handlers::zone_presets::UpdateZonePresetRequest,

            // zones
            crate::dto::request::zones::CreateZoneRequest,
            crate::dto::response::zones::ZoneResponse,
            crate::dto::request::zones::UpdateZoneRequest,
        )
    ),
    tags(
        (name = "Auth", description = "Authentication endpoints"),
        (name = "Config", description = "Config management endpoints"),
        (name = "Control Presets", description = "Monitor control presets"),
        (name = "Controls", description = "PTZ control configurations"),
        (name = "Daemons", description = "Daemon process control (replaces zmdc.pl)"),
        (name = "Devices", description = "X10 device controllers"),
        (name = "Event Data", description = "Event binary data storage"),
        (name = "Event Playback", description = "Event video playback and thumbnails"),
        (name = "Events", description = "Event management endpoints"),
        (name = "Events Tags", description = "Event-tag associations"),
        (name = "Filters", description = "Event filter endpoints"),
        (name = "Frames", description = "Individual frames within events"),
        (name = "Groups", description = "Group management endpoints"),
        (name = "Groups Monitors", description = "Group-monitor associations"),
        (name = "Groups Permissions", description = "Group permission management"),
        (name = "Live Streaming", description = "Live video streaming (HLS, WebRTC)"),
        (name = "Logs", description = "Log endpoints"),
        (name = "Manufacturers", description = "Camera manufacturers"),
        (name = "Models", description = "Camera models"),
        (name = "Monitor Presets", description = "Monitor configuration presets"),
        (name = "Monitor Status", description = "Real-time monitor status information"),
        (name = "Monitors", description = "Monitor management endpoints"),
        (name = "Notifications", description = "Push token registrations"),
        (name = "Monitors Permissions", description = "Monitor permission management"),
        (name = "Montage Layouts", description = "UI montage layouts"),
        (name = "Object Types", description = "Object detection type definitions"),
        (name = "PTZ", description = "Pan-Tilt-Zoom camera control"),
        (name = "Reports", description = "Report definitions and templates"),
        (name = "Search", description = "Natural-language / semantic event search"),
        (name = "Server", description = "Server information endpoints"),
        (name = "Server Stats", description = "Server performance statistics"),
        (name = "Servers", description = "Server info endpoints"),
        (name = "Snapshots", description = "System snapshots"),
        (name = "Snapshots Events", description = "Snapshot-event associations"),
        (name = "States", description = "Monitor states"),
        (name = "Stats", description = "Event statistics"),
        (name = "Storage", description = "Storage endpoints"),
        (name = "System", description = "System-level control (replaces zmpkg.pl)"),
        (name = "Tags", description = "Event tags"),
        (name = "TriggersX10", description = "X10 alarm triggers"),
        (name = "User Preferences", description = "User preference settings"),
        (name = "Users", description = "User management endpoints"),
        (name = "Zone Presets", description = "Zone preset endpoints"),
        (name = "Zones", description = "Zone management endpoints"),
    ),
    modifiers(&SecurityAddon, &ErrorResponses)
)]
pub struct ApiDoc;

/// Every operation answers any status it doesn't list with the standard error
/// body, [`AppResponseError`]: that is what `AppError` renders, from the
/// handler or from the auth and permission layers in front of it. A `default`
/// response says so once instead of on each of the ~330 operations, and gives
/// clients a schema for 401, 403, 404 and 500 wherever they can occur.
struct ErrorResponses;

impl Modify for ErrorResponses {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::{ContentBuilder, Ref, ResponseBuilder};
        let error = ResponseBuilder::new()
            .description("Error")
            .content(
                "application/json",
                ContentBuilder::new()
                    .schema(Some(Ref::from_schema_name("AppResponseError")))
                    .build(),
            )
            .build();
        for item in openapi.paths.paths.values_mut() {
            for op in [
                &mut item.get,
                &mut item.put,
                &mut item.post,
                &mut item.delete,
                &mut item.patch,
            ]
            .into_iter()
            .flatten()
            {
                op.responses
                    .responses
                    .entry("default".to_string())
                    .or_insert_with(|| error.clone().into());
            }
        }
    }
}

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.as_mut().unwrap();
        components.add_security_scheme(
            "jwt",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        )
    }
}
