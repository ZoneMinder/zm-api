use crate::handlers::{role_permissions, roles};
use crate::server::state::AppState;
use crate::util::middleware::auth_middleware;
use axum::{
    middleware,
    routing::{get, put},
    Router,
};

pub fn add_role_routes(router: Router<AppState>) -> Router<AppState> {
    let protected = Router::new()
        .route(
            "/api/v3/roles",
            get(roles::list_roles).post(roles::create_role),
        )
        .route(
            "/api/v3/roles/{id}",
            get(roles::get_role)
                .put(roles::update_role)
                .delete(roles::delete_role),
        )
        .route(
            "/api/v3/roles/{id}/monitors-permissions",
            get(role_permissions::list_monitors),
        )
        .route(
            "/api/v3/roles/{id}/monitors-permissions/{monitor_id}",
            put(role_permissions::set_monitor).delete(role_permissions::clear_monitor),
        )
        .route(
            "/api/v3/roles/{id}/groups-permissions",
            get(role_permissions::list_groups),
        )
        .route(
            "/api/v3/roles/{id}/groups-permissions/{group_id}",
            put(role_permissions::set_group).delete(role_permissions::clear_group),
        )
        .layer(middleware::from_fn(auth_middleware));
    router.merge(protected)
}
