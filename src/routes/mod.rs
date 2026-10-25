use crate::handlers::openapi::ApiDoc;
use crate::server::state::AppState;
use crate::util::authz;
use axum::extract::DefaultBodyLimit;
use axum::{
    http::{header, HeaderName, HeaderValue, Method},
    routing::any,
    Router,
};
use tower::ServiceBuilder;
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};

use crate::util::rate_limit::IpKeyExtractor;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

pub mod ai; // AI object-detection registry
pub mod auth;
pub mod configs; // Config management
pub mod control_presets; // Control Presets
pub mod controls; // Controls
pub mod daemon; // Daemon control
pub mod devices; // Devices
#[cfg(feature = "onvif-discovery")]
pub mod discovery; // ONVIF camera discovery
pub mod event_audit; // Audit Events Report
pub mod event_data; // Event Data
pub mod event_summaries; // Event Summaries (pre-calculated counts)
pub mod events; // Add events module
pub mod events_playback; // Event playback (video streaming)
pub mod events_tags; // Events Tags
pub mod filters; // Filters
pub mod frames; // Frames
pub mod groups; // Groups
pub mod groups_monitors; // Groups Monitors
pub mod groups_permissions; // Groups Permissions
pub mod live; // Live streaming (unified)
pub mod logs; // Logs
pub mod manufacturers; // Manufacturers
pub mod media_jobs; // generated event videos, media job queue (#187)
pub mod models; // Models
pub mod monitor_actions; // MonitorActions (#179)
pub mod monitor_presets; // Monitor Presets
pub mod monitor_status; // Monitor Status
pub mod monitors;
pub mod monitors_permissions; // Monitors Permissions
pub mod montage_layouts; // Montage Layouts
pub mod notifications; // push token registrations (#175)
pub mod object_types; // Object Types
pub mod ptz; // PTZ control
pub mod reports; // Reports
pub mod roles; // Roles (ZoneMinder 1.39)
pub mod search; // Natural-language / semantic event search
pub mod server;
pub mod server_stats; // Server Stats
pub mod servers; // Server info list
pub mod settings_tables; // EncoderTemplates, Menu_Items
pub mod snapshots; // Snapshots
pub mod snapshots_events; // Snapshots Events
pub mod states; // States
pub mod stats; // Stats
pub mod storage; // Storage
pub mod tags; // Tags
pub mod triggers_x10; // X10 Triggers
pub mod user_preferences; // User Preferences
pub mod users; // Users
pub mod zone_presets; // Zone Presets
pub mod zones; // Zones

/// Router fallback for unmatched paths. Returns a 404 with the standard error
/// envelope so clients can feature-detect optional endpoints instead of seeing
/// a 500. Previously this extracted `MatchedPath`, which *fails to extract* on
/// an unmatched route, producing a 500 "No matched path found" for every typo
/// or missing endpoint (GH #17).
async fn fallback_handler(
    method: axum::http::Method,
    uri: axum::http::Uri,
) -> (
    axum::http::StatusCode,
    axum::Json<crate::error::AppResponseError>,
) {
    tracing::debug!("No route for {} {}", method, uri.path());
    (
        axum::http::StatusCode::NOT_FOUND,
        axum::Json(crate::error::AppResponseError::new(
            "NOT_FOUND_ERROR",
            format!("No route for {} {}", method, uri.path()),
            None,
            vec![],
        )),
    )
}

/// Paths that belong to this API rather than to the browser UI.
///
/// When the UI is served from this binary its SPA fallback must not swallow
/// these: a typo'd endpoint has to keep returning the JSON 404 envelope that
/// clients feature-detect on, not an HTML page with status 200. Getting this
/// backwards is the classic SPA-behind-an-API bug — every mistyped request
/// silently succeeds and the client tries to parse `<!doctype html>`.
const API_PATH_PREFIXES: &[&str] = &["/api/", "/swagger-ui", "/api-docs", "/.well-known/"];

fn is_api_path(path: &str) -> bool {
    API_PATH_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// One allowed-origin rule parsed from `ALLOWED_ORIGINS`.
enum OriginRule {
    /// Exact origin match, e.g. `https://app.example.com`.
    Exact(HeaderValue),
    /// A host with any explicit port, written `scheme://host:*` (e.g.
    /// `http://localhost:*`). Matches `scheme://host:<digits>` only. This is
    /// the dev convenience that avoids re-listing every Vite/CRA/preview port,
    /// without resorting to a wildcard (illegal here — see below).
    AnyPort { prefix: String },
}

impl OriginRule {
    fn matches(&self, origin: &HeaderValue) -> bool {
        match self {
            OriginRule::Exact(allowed) => allowed == origin,
            OriginRule::AnyPort { prefix } => {
                let o = origin.as_bytes();
                let p = prefix.as_bytes();
                // `scheme://host:` followed by a non-empty, all-digits port.
                o.len() > p.len() && o.starts_with(p) && o[p.len()..].iter().all(u8::is_ascii_digit)
            }
        }
    }
}

/// Origins allowed when nothing is configured: any localhost / 127.0.0.1 port,
/// so new frontend dev ports don't need registering one by one. Deliberately
/// useless in production — a packaged install serving a dashboard from a real
/// hostname must set `server.allowed_origins`.
const DEFAULT_ALLOWED_ORIGINS: &[&str] = &["http://localhost:*", "http://127.0.0.1:*"];

/// Legacy, un-prefixed environment variable. Superseded by
/// `server.allowed_origins` (`APP_SERVER__ALLOWED_ORIGINS`) but still honoured
/// so existing deployments keep working across the upgrade.
const LEGACY_ORIGINS_ENV: &str = "ALLOWED_ORIGINS";

/// Where the effective origin list came from, for the startup log line.
#[derive(Debug, PartialEq, Eq)]
enum OriginSource {
    Config,
    LegacyEnv,
    Default,
}

/// Resolve the effective allowed-origin list: configured value first, then the
/// legacy environment variable, then the localhost-only default.
///
/// Split out from [`build_cors_layer`] so the precedence is testable without
/// mutating process environment.
fn resolve_allowed_origins(
    configured: &[String],
    legacy: Option<String>,
) -> (Vec<String>, OriginSource) {
    if !configured.is_empty() {
        return (configured.to_vec(), OriginSource::Config);
    }
    if let Some(raw) = legacy {
        let parsed: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if !parsed.is_empty() {
            return (parsed, OriginSource::LegacyEnv);
        }
    }
    (
        DEFAULT_ALLOWED_ORIGINS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        OriginSource::Default,
    )
}

/// Build the CORS layer from `server.allowed_origins`.
///
/// Each entry is either an exact origin (`https://app.example.com`) or a host
/// with a `:*` port wildcard (`http://localhost:*`) that matches that host on
/// any port.
///
/// Note: the layer sets `Access-Control-Allow-Credentials: true`, and the CORS
/// spec forbids pairing credentials with a literal `*` origin (browsers reject
/// it). So "all origins" is intentionally not offered; the `:*` rule reflects
/// the specific matching origin back instead, which is credentials-safe and,
/// for `localhost`, does not expose the API to other hosts. In production set
/// `server.allowed_origins` to the exact deployed front-end origin(s).
fn build_cors_layer(config: &crate::configure::AppConfig) -> CorsLayer {
    let (origins, source) = resolve_allowed_origins(
        &config.server.allowed_origins,
        std::env::var(LEGACY_ORIGINS_ENV).ok(),
    );

    match source {
        OriginSource::Config => {
            tracing::info!("CORS allowed origins (server.allowed_origins): {origins:?}");
        }
        OriginSource::LegacyEnv => {
            tracing::warn!(
                "CORS allowed origins came from the deprecated {LEGACY_ORIGINS_ENV} \
                 environment variable: {origins:?}. Set server.allowed_origins \
                 (APP_SERVER__ALLOWED_ORIGINS) instead."
            );
        }
        OriginSource::Default => {
            // Say this loudly: a dashboard on any other origin is CORS-blocked,
            // and the browser-side symptom gives no hint of the cause.
            tracing::warn!(
                "CORS allowed origins not configured, defaulting to {origins:?}. \
                 Browsers will block a dashboard served from any other origin — \
                 set server.allowed_origins (APP_SERVER__ALLOWED_ORIGINS) to it."
            );
        }
    }

    let frontend_urls = origins.join(",");
    let rules = parse_origin_rules(&frontend_urls);

    let allow_origin = AllowOrigin::predicate(move |origin: &HeaderValue, _req| {
        rules.iter().any(|r| r.matches(origin))
    });

    CorsLayer::new()
        // Allow frontend origins to access the API
        .allow_origin(allow_origin)
        // Allow common HTTP methods needed for a RESTful API
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ])
        // Allow common HTTP headers used in API requests
        .allow_headers([
            HeaderName::from_static("authorization"),
            HeaderName::from_static("content-type"),
            HeaderName::from_static("x-requested-with"),
            HeaderName::from_static("accept"),
            HeaderName::from_static("origin"),
        ])
        // WHEP answers carry the session URL in `Location`; a browser client
        // can only read it when it's exposed.
        .expose_headers([axum::http::header::LOCATION])
        .allow_credentials(true)
}

/// Parse an origin list into a set of [`OriginRule`]s. Entries ending in
/// `:*` become any-port host rules; the rest are exact origins. Entries that
/// are neither parseable as a header value nor a `:*` rule are skipped.
fn parse_origin_rules(raw: &str) -> Vec<OriginRule> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            if let Some(host) = entry.strip_suffix(":*") {
                // Store the prefix including the colon, e.g. "http://localhost:".
                Some(OriginRule::AnyPort {
                    prefix: format!("{host}:"),
                })
            } else {
                entry.parse::<HeaderValue>().ok().map(OriginRule::Exact)
            }
        })
        .collect()
}

/// Apply the cross-cutting middleware stack to the fully-merged router.
///
/// Outermost → innermost: tracing, security headers, body-size limit,
/// optional per-IP rate limiting, then CORS.
fn apply_common_middleware(
    router: Router<AppState>,
    cors: CorsLayer,
    cfg: &crate::configure::AppConfig,
) -> Router<AppState> {
    let mw = &cfg.server.middleware;
    let tls_enabled = cfg.server.tls.as_ref().is_some_and(|t| t.enabled)
        || cfg.server.acme.as_ref().is_some_and(|a| a.enabled);

    // Per-IP rate limiting — opt-in via config (`rate_limit_per_second = 0`
    // disables it). The key is the peer IP by default; forwarding headers are
    // believed only with `trust_proxy_headers` set and from a
    // `trusted_proxies` peer (see `IpKeyExtractor`).
    let rate_limit_layer = if mw.rate_limiting_enabled() {
        let (burst, was_defaulted) = mw.effective_rate_limit_burst();
        if was_defaulted {
            tracing::error!(
                "server.middleware.rate_limit_burst is 0 while the limiter is \
                 enabled. A burst of 0 would allow one request and refuse every \
                 one after it, so no client could load a page. Using {burst} \
                 instead — set it explicitly to something a page load fits in."
            );
        }
        match GovernorConfigBuilder::default()
            .per_second(mw.rate_limit_period_secs)
            .burst_size(burst)
            .key_extractor(IpKeyExtractor::new(
                mw.trust_proxy_headers,
                &mw.trusted_proxies,
            ))
            .finish()
        {
            Some(conf) => {
                tracing::info!(
                    "Global rate limiting enabled: 1 token/{}s per IP, burst {} (trust_proxy={})",
                    mw.rate_limit_period_secs,
                    burst,
                    mw.trust_proxy_headers,
                );
                Some(GovernorLayer::new(conf))
            }
            None => {
                tracing::warn!("Invalid rate-limit configuration; global rate limiting disabled");
                None
            }
        }
    } else {
        None
    };

    // HSTS only makes sense when the service itself terminates TLS.
    let hsts_layer = tls_enabled.then(|| {
        SetResponseHeaderLayer::if_not_present(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        )
    });

    let stack = ServiceBuilder::new()
        .layer(TraceLayer::new_for_http())
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("no-referrer"),
        ))
        .option_layer(hsts_layer)
        .layer(DefaultBodyLimit::max(mw.body_limit_bytes))
        .option_layer(rate_limit_layer)
        .layer(cors);

    router.layer(stack)
}

pub fn create_router_app(state: AppState) -> Router {
    // The state's config, not the process-wide CONFIG: they are the same in
    // production, and a test that adjusts the state's sees it take effect.
    let config = state.config.clone();
    let cors = build_cors_layer(&config);

    // `auth` and `server` expose a mix of public and protected endpoints
    // (login, health check, version) and manage their own auth per-route, so
    // they are deliberately not wrapped with a blanket RBAC feature gate.
    let server_routes = server::add_server_routes(Router::new(), state.clone());
    let auth_routes = {
        let routes = auth::add_routers(Router::new(), state.clone());
        // Dedicated, tighter rate limit on the authentication surface so
        // credential brute-forcing is throttled even when the global limiter
        // is disabled. Same peer-IP-by-default keying as the global limiter.
        let mw = &config.server.middleware;
        let auth_rl = if mw.auth_rate_limiting_enabled() {
            GovernorConfigBuilder::default()
                .per_second(mw.auth_rate_limit_period_secs)
                .burst_size(mw.auth_rate_limit_burst)
                .key_extractor(IpKeyExtractor::new(
                    mw.trust_proxy_headers,
                    &mw.trusted_proxies,
                ))
                .finish()
        } else {
            None
        };
        match auth_rl {
            Some(conf) => {
                tracing::info!(
                    "Auth rate limiting enabled: 1 token/{}s per IP, burst {} (trust_proxy={})",
                    mw.auth_rate_limit_period_secs,
                    mw.auth_rate_limit_burst,
                    mw.trust_proxy_headers,
                );
                routes.layer(GovernorLayer::new(conf))
            }
            None => routes,
        }
    };

    // Every other router is gated on the ZoneMinder permission feature it
    // belongs to. `authz::protect` derives the required level from the HTTP
    // method (read -> View, write -> Edit).
    use authz::Feature;
    let protect =
        |router: Router<AppState>, feature| authz::protect(router, feature, state.clone());

    let monitors_routes = protect(
        monitors::add_monitor_routes(Router::new()),
        Feature::Monitors,
    );
    let monitor_preset_routes = protect(
        monitor_presets::add_monitor_preset_routes(Router::new()),
        Feature::Monitors,
    );
    let monitor_status_routes = protect(
        monitor_status::add_monitor_status_routes(Router::new()),
        Feature::Monitors,
    );
    // Permission CRUD is administrative: a Groups:Edit / Monitors:Edit user
    // would otherwise be able to grant themselves or others elevated row-level
    // access via this endpoint. Require System (admin-tier) instead.
    let monitor_permission_routes = protect(
        monitors_permissions::add_monitor_permission_routes(Router::new()),
        Feature::System,
    );
    let zone_routes = protect(zones::add_zone_routes(Router::new()), Feature::Monitors);
    let zone_preset_routes = protect(
        zone_presets::add_zone_preset_routes(Router::new()),
        Feature::Monitors,
    );

    let events_routes = protect(events::add_event_routes(Router::new()), Feature::Events);
    let event_data_routes = protect(
        event_data::add_event_data_routes(Router::new()),
        Feature::Events,
    );
    let event_summary_routes = protect(
        event_summaries::add_event_summaries_routes(Router::new()),
        Feature::Events,
    );
    let event_tag_routes = protect(
        events_tags::add_event_tag_routes(Router::new()),
        Feature::Events,
    );
    let frame_routes = protect(frames::add_frames_routes(Router::new()), Feature::Events);
    let filter_routes = protect(filters::add_filter_routes(Router::new()), Feature::Events);
    let tag_routes = protect(tags::add_tag_routes(Router::new()), Feature::Events);
    let object_type_routes = protect(
        object_types::add_object_type_routes(Router::new()),
        Feature::Events,
    );
    let events_playback_routes = protect(
        events_playback::add_events_playback_routes(Router::new()),
        Feature::Events,
    );
    // Natural-language / semantic event search. JSON (compressible), so it lives
    // in the `api` group rather than the streaming group. Row-level ACL is
    // enforced inside the handlers via `MonitorScope`.
    let search_routes = protect(search::add_search_routes(Router::new()), Feature::Events);

    let control_routes = protect(
        controls::add_control_routes(Router::new()),
        Feature::Control,
    );
    let control_preset_routes = protect(
        control_presets::add_control_preset_routes(Router::new()),
        Feature::Control,
    );
    // PTZ acts on a monitor named in the path (`{id}`); guard it row-level.
    // Order matters: `protect` must wrap *outside* the row-level guard so the
    // feature-level RBAC check runs first and the guard's DB query is only
    // reached after the caller has at least `Control:View`.
    let ptz_routes = protect(
        ptz::add_ptz_routes(Router::new()).route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::service::monitor_acl::monitor_path_guard,
        )),
        Feature::Control,
    );
    let trigger_x10_routes = protect(
        triggers_x10::add_trigger_x10_routes(Router::new()),
        Feature::Control,
    );

    let group_routes = protect(groups::add_group_routes(Router::new()), Feature::Groups);
    let group_monitor_routes = protect(
        groups_monitors::add_group_monitor_routes(Router::new()),
        Feature::Groups,
    );
    // Permission CRUD is administrative — see `monitor_permission_routes` above.
    let group_permission_routes = protect(
        groups_permissions::add_group_permission_routes(Router::new()),
        Feature::System,
    );

    let device_routes = protect(devices::add_device_routes(Router::new()), Feature::Devices);
    let manufacturer_routes = protect(
        manufacturers::add_manufacturer_routes(Router::new()),
        Feature::Devices,
    );
    let model_routes = protect(models::add_model_routes(Router::new()), Feature::Devices);

    let snapshot_routes = protect(
        snapshots::add_snapshot_routes(Router::new()),
        Feature::Snapshots,
    );
    let snapshot_event_routes = protect(
        snapshots_events::add_snapshot_event_routes(Router::new()),
        Feature::Snapshots,
    );

    // Live streaming serves a monitor named in the path (`{monitor_id}`);
    // guard it row-level. `/live/sessions` and `/live/sources` have no path
    // monitor id, so the guard passes them through.
    //
    // Order matters: `protect` must wrap *outside* the row-level guard so the
    // feature-level RBAC check runs first and the guard's DB query is only
    // reached after the caller has at least `Stream:View`.
    let live_routes = protect(
        live::add_live_routes(Router::new()).route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::service::monitor_acl::monitor_stream_guard,
        )),
        Feature::Stream,
    );

    let config_routes = protect(configs::add_config_routes(Router::new()), Feature::System);
    let log_routes = protect(logs::add_log_routes(Router::new()), Feature::System);
    // AI object-detection registry: admin-tier, matching ZoneMinder's own
    // System-gated Options UI for these tables.
    let ai_routes = protect(ai::add_ai_routes(Router::new()), Feature::System);
    let storage_routes = protect(storage::add_storage_routes(Router::new()), Feature::System);
    let server_info_routes = protect(
        servers::add_server_info_routes(Router::new()),
        Feature::System,
    );
    let server_stat_routes = protect(
        server_stats::add_server_stat_routes(Router::new()),
        Feature::System,
    );
    let stat_routes = protect(stats::add_stat_routes(Router::new()), Feature::System);
    let state_routes = protect(states::add_state_routes(Router::new()), Feature::System);
    let report_routes = protect(reports::add_report_routes(Router::new()), Feature::System);
    // `[daemon].enable_rest_api = false` leaves daemon/system control to the
    // legacy socket only; the routes are not registered at all (#88).
    let daemon_routes = if config.daemon.enable_rest_api {
        protect(daemon::add_daemon_routes(Router::new()), Feature::System)
    } else {
        Router::new()
    };
    let user_routes = protect(users::add_user_routes(Router::new()), Feature::System);
    let role_routes = protect(roles::add_role_routes(Router::new()), Feature::System);
    let event_audit_routes = protect(
        event_audit::add_event_audit_routes(Router::new()),
        Feature::Events,
    );
    let settings_table_routes = protect(
        settings_tables::add_settings_table_routes(Router::new()),
        Feature::System,
    );
    // MonitorActions: editing is the monitor's (row-level guarded); running a
    // Manual action needs only Control view, checked on both monitors inside.
    let monitor_action_routes = protect(
        monitor_actions::add_monitor_action_routes(Router::new()).route_layer(
            axum::middleware::from_fn_with_state(
                state.clone(),
                crate::service::monitor_acl::monitor_path_guard,
            ),
        ),
        Feature::Monitors,
    );
    let monitor_action_run_route = authz::protect_at(
        monitor_actions::add_monitor_action_run_route(Router::new()),
        Feature::Control,
        authz::Level::View,
        state.clone(),
    );
    let user_preference_routes = protect(
        user_preferences::add_user_preference_routes(Router::new()),
        Feature::System,
    );
    let montage_layout_routes = protect(
        montage_layouts::add_montage_layout_routes(Router::new()),
        Feature::System,
    );

    // Streaming endpoints must bypass response compression: they serve
    // byte-range video, chunked HLS playlists, JPEG snapshots, SSE and
    // WebSocket upgrades, all of which `CompressionLayer` would buffer,
    // invalidate (Range) or corrupt.
    // `?st=` signed media URLs are checked ahead of every streaming route's
    // own auth, which then honours the grant (see service::media_grant).
    let streaming = Router::new()
        .merge(live_routes) // Live streaming (unified)
        .merge(events_playback_routes) // Event playback
        .merge(snapshot_routes)
        .merge(snapshot_event_routes)
        .merge(protect(
            media_jobs::add_event_video_routes(Router::new()),
            Feature::Events,
        ))
        .merge(crate::util::authz::protect_at(
            events_playback::add_event_export_route(Router::new()),
            Feature::Events,
            crate::util::authz::Level::View,
            state.clone(),
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::service::media_grant::media_grant_layer,
        ));

    // Build the served OpenAPI document, merging in feature-gated fragments
    // (utoipa's derive cannot `#[cfg]` individual path/schema entries, so the
    // ONVIF discovery fragment is composed here at runtime).
    #[allow(unused_mut)]
    let mut openapi = ApiDoc::openapi();
    #[cfg(feature = "onvif-discovery")]
    openapi.merge(crate::handlers::discovery::DiscoveryApiDoc::openapi());
    // Behind a proxy prefix, Swagger UI must fetch the document, and send
    // "try it out" requests, through that prefix.
    let prefix = crate::configure::server::public_prefix();
    let mut swagger = SwaggerUi::new("/swagger-ui");
    if !prefix.is_empty() {
        openapi.servers = Some(vec![utoipa::openapi::Server::new(prefix)]);
        swagger = swagger.config(utoipa_swagger_ui::Config::from(format!(
            "{prefix}/api-docs/openapi.json"
        )));
    }

    // Regular JSON API endpoints — safe to gzip/brotli compress.
    let api = Router::new()
        .merge(swagger.url("/api-docs/openapi.json", openapi))
        .merge(server_routes)
        .merge(auth_routes)
        .merge(monitors_routes)
        .merge(events_routes)
        .merge(config_routes)
        .merge(zone_routes)
        .merge(filter_routes)
        .merge(user_routes)
        .merge(role_routes)
        .merge(event_audit_routes)
        .merge(settings_table_routes)
        .merge(monitor_action_routes)
        .merge(monitor_action_run_route)
        .merge(notifications::add_notification_routes(Router::new()))
        .merge(protect(
            media_jobs::add_job_routes(Router::new()),
            Feature::Events,
        ))
        .merge(settings_tables::add_navbar_route(Router::new()))
        .merge(group_routes)
        .merge(server_info_routes)
        .merge(log_routes)
        .merge(ai_routes)
        .merge(storage_routes)
        .merge(manufacturer_routes)
        .merge(model_routes)
        .merge(zone_preset_routes)
        .merge(control_routes)
        .merge(control_preset_routes)
        .merge(device_routes)
        .merge(monitor_preset_routes)
        .merge(montage_layout_routes)
        .merge(tag_routes)
        .merge(trigger_x10_routes)
        .merge(user_preference_routes)
        .merge(state_routes)
        .merge(stat_routes)
        .merge(frame_routes)
        .merge(monitor_status_routes)
        .merge(object_type_routes)
        .merge(server_stat_routes)
        .merge(report_routes)
        .merge(group_monitor_routes)
        .merge(group_permission_routes)
        .merge(monitor_permission_routes)
        .merge(event_data_routes)
        .merge(event_summary_routes)
        .merge(event_tag_routes)
        .merge(search_routes)
        .merge(daemon_routes) // Daemon control
        .merge(ptz_routes) // PTZ control
        .merge(crate::util::authz::protect_at(
            events_playback::add_event_link_route(Router::new()),
            Feature::Events,
            crate::util::authz::Level::View,
            state.clone(),
        ));

    // ONVIF discovery (feature-gated). Feeds monitor creation, so gated like
    // monitor management; merged before the compression layer so its JSON
    // responses are compressed like the rest of the API. The probe/inspect
    // endpoints have no `{monitor_id}` path param, so no row-level
    // `monitor_path_guard` is needed — the handlers' own `scope.is_restricted()`
    // check is the row-level gate.
    #[cfg(feature = "onvif-discovery")]
    let api = api.merge(protect(
        discovery::add_discovery_routes(Router::new()),
        Feature::Monitors,
    ));

    let api = api.layer(CompressionLayer::new());

    let app =
        Router::new()
            .merge(api)
            .merge(streaming)
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::util::middleware::schema_gate,
            ));

    // The browser UI (zm-web), when this binary is serving it. Mounted as the
    // fallback so every real route above still wins; only unmatched paths reach
    // the SPA.
    let web = &crate::constant::CONFIG.web;
    let app = if web.should_serve() {
        tracing::info!(
            "Serving the zm-web UI from {} (assets cached {}s)",
            web.root.display(),
            web.asset_max_age_secs
        );
        app.fallback(any(web_fallback))
    } else {
        if web.enabled {
            // Enabled but nothing there: warn rather than refuse to boot. The
            // API is the more important half, and a missing UI package should
            // not take it down.
            tracing::warn!(
                "web.enabled is true but {} has no index.html — not serving the UI",
                web.root.display()
            );
        }
        app.fallback(any(fallback_handler))
    };

    apply_common_middleware(app, cors, &config).with_state(state)
}

/// Serve a file from the UI root, falling back to `index.html` so client-side
/// routes like `/events/123` load the app instead of 404ing.
///
/// API paths are excluded and keep the JSON 404 envelope — see
/// [`API_PATH_PREFIXES`].
async fn web_fallback(
    method: axum::http::Method,
    uri: axum::http::Uri,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let path = uri.path().to_string();
    if is_api_path(&path) {
        return fallback_handler(method, uri).await.into_response();
    }

    let web = &crate::constant::CONFIG.web;
    let index = web.index_path();

    // `ServeDir` resolves the request path against the root and rejects
    // traversal itself; `ServeFile` handles the SPA case. Both are constructed
    // per request, which is cheap — they hold a path, not an open handle.
    let service = tower_http::services::ServeDir::new(&web.root)
        .precompressed_gzip()
        .precompressed_br()
        .fallback(tower_http::services::ServeFile::new(&index));

    let mut response = match tower::ServiceExt::oneshot(service, req).await {
        Ok(resp) => resp.into_response(),
        Err(e) => {
            tracing::error!("serving the UI failed for {path}: {e}");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    apply_web_headers(&mut response, &path, web);
    response
}

/// Cache and security headers for UI responses.
///
/// Fingerprinted assets are immutable and cached hard; `index.html` names the
/// current asset hashes, so caching it would pin a browser to a stale build and
/// it is always revalidated.
fn apply_web_headers(
    response: &mut axum::response::Response,
    path: &str,
    web: &crate::configure::web::WebConfig,
) {
    let headers = response.headers_mut();

    let served_index = path == "/"
        || path.ends_with('/')
        || path.ends_with(".html")
        // A client-side route (no file extension) falls back to index.html.
        || !path.rsplit('/').next().is_some_and(|s| s.contains('.'));

    let cache = if served_index {
        "no-cache".to_string()
    } else {
        format!("public, max-age={}, immutable", web.asset_max_age_secs)
    };
    if let Ok(value) = HeaderValue::from_str(&cache) {
        headers.insert(header::CACHE_CONTROL, value);
    }

    if !web.content_security_policy.is_empty() {
        if let Ok(value) = HeaderValue::from_str(&web.content_security_policy) {
            headers.insert(HeaderName::from_static("content-security-policy"), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    fn allows(rules: &[OriginRule], origin: &str) -> bool {
        let o = hv(origin);
        rules.iter().any(|r| r.matches(&o))
    }

    #[test]
    fn any_port_rule_matches_localhost_on_any_port() {
        let rules = parse_origin_rules("http://localhost:*,http://127.0.0.1:*");
        assert!(allows(&rules, "http://localhost:3000"));
        assert!(allows(&rules, "http://localhost:5173"));
        assert!(allows(&rules, "http://localhost:49152")); // ephemeral
        assert!(allows(&rules, "http://127.0.0.1:8080"));
    }

    #[test]
    fn any_port_rule_rejects_lookalikes_and_other_hosts() {
        let rules = parse_origin_rules("http://localhost:*");
        // Non-numeric / empty port must not match (so a forged
        // "localhost:3000.evil.com"-style origin can't slip through).
        assert!(!allows(&rules, "http://localhost:3000.evil.com"));
        assert!(!allows(&rules, "http://localhost:"));
        assert!(!allows(&rules, "http://localhost")); // bare host, no port
                                                      // Different host, and the subdomain-confusion case.
        assert!(!allows(&rules, "http://evil.com:3000"));
        assert!(!allows(&rules, "http://localhost.evil.com:3000"));
        // Scheme must match too.
        assert!(!allows(&rules, "https://localhost:3000"));
    }

    #[test]
    fn exact_rule_matches_only_that_origin() {
        let rules = parse_origin_rules("https://app.example.com,http://localhost:3000");
        assert!(allows(&rules, "https://app.example.com"));
        assert!(allows(&rules, "http://localhost:3000"));
        assert!(!allows(&rules, "https://app.example.com:8443"));
        assert!(!allows(&rules, "http://localhost:3001"));
    }

    #[test]
    fn mixed_exact_and_wildcard_entries() {
        let rules = parse_origin_rules("https://prod.example.com, http://localhost:*");
        assert!(allows(&rules, "https://prod.example.com"));
        assert!(allows(&rules, "http://localhost:1234"));
        assert!(!allows(&rules, "https://staging.example.com"));
    }

    #[test]
    fn blank_and_unparseable_entries_are_skipped() {
        let rules = parse_origin_rules(" , ,http://localhost:*, ");
        assert_eq!(rules.len(), 1);
        assert!(allows(&rules, "http://localhost:9999"));
    }

    #[test]
    fn configured_origins_win_over_the_legacy_env_var() {
        let configured = vec!["https://zm.example.com".to_string()];
        let (origins, source) =
            resolve_allowed_origins(&configured, Some("https://stale.example".into()));
        assert_eq!(origins, configured);
        assert_eq!(source, OriginSource::Config);
    }

    #[test]
    fn legacy_env_var_is_still_honoured_when_nothing_is_configured() {
        // Existing deployments set only the bare ALLOWED_ORIGINS; upgrading
        // must not silently start CORS-blocking their dashboard.
        let (origins, source) =
            resolve_allowed_origins(&[], Some(" https://a.example, https://b.example ".into()));
        assert_eq!(origins, vec!["https://a.example", "https://b.example"]);
        assert_eq!(source, OriginSource::LegacyEnv);
    }

    #[test]
    fn falls_back_to_localhost_when_unset_or_blank() {
        for legacy in [None, Some(String::new()), Some(" , ".to_string())] {
            let (origins, source) = resolve_allowed_origins(&[], legacy);
            assert_eq!(origins, DEFAULT_ALLOWED_ORIGINS);
            assert_eq!(source, OriginSource::Default);
        }
    }
}
