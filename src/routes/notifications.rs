use crate::handlers::notifications as h;
use crate::server::state::AppState;
use crate::util::middleware::authenticated_middleware;
use axum::{middleware, routing::get, Router};

/// Push registrations: a login, no feature gate; rows are scoped to the
/// caller inside.
pub fn add_notification_routes(router: Router<AppState>, state: AppState) -> Router<AppState> {
    let protected = Router::new()
        .route(
            "/api/v3/notifications",
            get(h::list_notifications).post(h::create_notification),
        )
        .route(
            "/api/v3/notifications/{id}",
            get(h::get_notification)
                .patch(h::update_notification)
                .delete(h::delete_notification),
        )
        .layer(middleware::from_fn_with_state(
            state,
            authenticated_middleware,
        ));
    router.merge(protected)
}
