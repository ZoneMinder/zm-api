use crate::handlers::settings_tables as h;
use crate::server::state::AppState;
use crate::util::middleware::auth_middleware;
use axum::{
    middleware,
    routing::{get, post},
    Router,
};

/// EncoderTemplates and Menu_Items CRUD (System-gated by the caller).
pub fn add_settings_table_routes(router: Router<AppState>) -> Router<AppState> {
    let protected = Router::new()
        .route(
            "/api/v3/encoder-templates",
            get(h::list_encoder_templates).post(h::create_encoder_template),
        )
        .route(
            "/api/v3/encoder-templates/{id}",
            get(h::get_encoder_template)
                .put(h::update_encoder_template)
                .delete(h::delete_encoder_template),
        )
        .route("/api/v3/system/privacy", post(h::record_privacy))
        .route(
            "/api/v3/menu-items",
            get(h::list_menu_items).post(h::create_menu_item),
        )
        .route("/api/v3/menu-items/reset", post(h::reset_menu_items))
        .route(
            "/api/v3/menu-items/{id}",
            get(h::get_menu_item)
                .put(h::update_menu_item)
                .delete(h::delete_menu_item),
        )
        .layer(middleware::from_fn(auth_middleware));
    router.merge(protected)
}

/// `GET /api/v3/system/menu`: a login, no feature gate.
pub fn add_navbar_route(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/api/v3/system/menu",
        get(h::get_navbar).layer(middleware::from_fn(auth_middleware)),
    )
}
