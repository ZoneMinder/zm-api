use crate::handlers::server;
use crate::server::state::AppState;
use crate::util::authz::{protect, Feature};
use axum::{
    routing::{get, post},
    Router,
};
use tracing::info;

pub fn add_server_routes(router: Router<AppState>, state: AppState) -> Router<AppState> {
    info!("Registering routes for server...");
    let api_prefix = "/api/v3";

    // Public routes — intentionally no auth.
    let public_routes = Router::new()
        .route(
            &format!("{}/server/health_check", api_prefix),
            get(server::health_check),
        )
        .route(
            &format!("{}/host/getVersion", api_prefix),
            get(server::get_version),
        );

    // Whole-system power control: `systemctl restart/stop/start zoneminder`
    // (admin-tier, so gated behind the `System` feature). The canonical path is
    // `/server/control/{action}`; `/states/change/{action}` is kept as a
    // deprecated alias — it wrongly implied it applied a run state from the
    // `States` table (that is `POST /system/state`).
    let control_routes = protect(
        Router::new()
            .route(
                &format!("{}/server/control/{{action}}", api_prefix),
                post(server::change_state),
            )
            .route(
                &format!("{}/states/change/{{action}}", api_prefix),
                post(server::change_state),
            ),
        Feature::System,
        state.clone(),
    );

    // Read-only server locale (timezone + date/time formats): any authenticated
    // user, no feature gate.
    let locale_routes = Router::new()
        .route(
            &format!("{}/system/locale", api_prefix),
            get(server::get_locale),
        )
        .layer(axum::middleware::from_fn_with_state(
            state,
            crate::util::middleware::authenticated_middleware,
        ));

    router
        .merge(public_routes)
        .merge(control_routes)
        .merge(locale_routes)
}
