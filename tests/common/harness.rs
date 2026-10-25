//! Shared integration-test harness.
//!
//! [`TestApp`] builds the full Axum application against the real test
//! database and drives it with `tower`'s `oneshot`, so requests exercise the
//! entire middleware + routing + handler stack exactly as in production.
#![allow(dead_code)]

use axum::body::{self, Body};
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use sea_orm::DatabaseConnection;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tower::ServiceExt;
use zm_api::server::state::AppState;
use zm_api::service::token::generate_tokens;
use zm_api::util::authz::UserPermissions;

use super::test_db::get_test_db;

/// A fully-wired application under test, plus a database handle for fixtures.
pub struct TestApp {
    /// Direct database handle for inserting fixtures and asserting state.
    pub db: DatabaseConnection,
    router: Router,
}

impl TestApp {
    /// Connect to the test database and build the production router.
    ///
    /// `DatabaseConnection` is not `Clone`, so two connections are opened: one
    /// drives the application, the other (`self.db`) is for fixtures.
    pub async fn spawn() -> Self {
        Self::spawn_with(|_| {}).await
    }

    /// [`spawn`](Self::spawn), adjusting the state (its config, say) before
    /// the router is built.
    #[allow(dead_code)]
    pub async fn spawn_with(adjust: impl FnOnce(&mut AppState)) -> Self {
        let expect_msg = "connect to test database (is it running on :3307?)";
        let fixture_db = get_test_db().await.expect(expect_msg);
        let app_db = get_test_db().await.expect(expect_msg);
        let mut state = AppState::for_test_with_db(app_db);
        adjust(&mut state);
        let router = super::schema::checked(zm_api::routes::create_router_app(state));
        Self {
            db: fixture_db,
            router,
        }
    }

    /// [`spawn`](Self::spawn) with live view on, as `AppState::new` sets it up,
    /// reading stream sockets from `socks_dir` (a
    /// [`FakeZmc`](super::fake_zmc::FakeZmc)'s).
    pub async fn spawn_live(socks_dir: &std::path::Path) -> Self {
        let socks = socks_dir.to_string_lossy().into_owned();
        Self::spawn_with(|state| {
            let mut config = (*state.config).clone();
            config.streaming.enabled = true;
            config.streaming.hls.enabled = true;
            config.streaming.hls.storage.path = format!("{socks}/hls");
            // Host candidates only: no public STUN server in tests.
            config.streaming.webrtc.stun_servers = Vec::new();
            config.streaming.zoneminder.socks_path = socks;
            state.config = std::sync::Arc::new(config);
            state.enable_live_streaming();
        })
        .await
    }

    /// Build the production router backed by an in-memory mock database.
    ///
    /// Suitable for tests that are rejected before any query runs — auth
    /// failures, RBAC denials, middleware behaviour — so they need no real
    /// database and can run in the standard `cargo test` pass.
    pub fn mock() -> Self {
        Self::mock_with(|_| {})
    }

    /// [`mock`](Self::mock), adjusting the state before the router is built.
    #[allow(dead_code)]
    pub fn mock_with(adjust: impl FnOnce(&mut AppState)) -> Self {
        use sea_orm::{DatabaseBackend, MockDatabase};
        let fixture_db = MockDatabase::new(DatabaseBackend::MySql).into_connection();
        let app_db = MockDatabase::new(DatabaseBackend::MySql).into_connection();
        let mut state = AppState::for_test_with_db(app_db);
        adjust(&mut state);
        let router = super::schema::checked(zm_api::routes::create_router_app(state));
        Self {
            db: fixture_db,
            router,
        }
    }

    /// Build the production router from a caller-supplied state — for tests
    /// that pre-seed state (token revocations, custom mock query/exec
    /// results) before wiring the router. The `db` fixture handle is a
    /// detached mock and must not be queried.
    pub fn from_state(state: AppState) -> Self {
        use sea_orm::{DatabaseBackend, MockDatabase};
        let fixture_db = MockDatabase::new(DatabaseBackend::MySql).into_connection();
        let router = super::schema::checked(zm_api::routes::create_router_app(state));
        Self {
            db: fixture_db,
            router,
        }
    }

    /// The router itself, for tests that read a response's head without
    /// waiting for its body (streams).
    #[allow(dead_code)]
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// Begin building a request. The router is cloned per call so a single
    /// `TestApp` can serve any number of requests.
    pub fn request(&self, method: Method, path: &str) -> TestRequest {
        TestRequest {
            router: self.router.clone(),
            method,
            path: path.to_string(),
            token: None,
            raw_auth_value: false,
            body: None,
            headers: Vec::new(),
        }
    }

    /// Authenticated `GET`.
    pub async fn get(&self, path: &str, token: &str) -> TestResponse {
        self.request(Method::GET, path).bearer(token).send().await
    }

    /// Authenticated `DELETE`.
    pub async fn delete(&self, path: &str, token: &str) -> TestResponse {
        self.request(Method::DELETE, path)
            .bearer(token)
            .send()
            .await
    }

    /// Authenticated `POST` with a JSON body.
    pub async fn post_json<T: Serialize>(&self, path: &str, token: &str, body: &T) -> TestResponse {
        self.request(Method::POST, path)
            .bearer(token)
            .json(body)
            .send()
            .await
    }

    /// Authenticated `PUT` with a JSON body.
    #[allow(dead_code)]
    pub async fn put_json<T: Serialize>(&self, path: &str, token: &str, body: &T) -> TestResponse {
        self.request(Method::PUT, path)
            .bearer(token)
            .json(body)
            .send()
            .await
    }

    /// Authenticated `PATCH` with a JSON body.
    pub async fn patch_json<T: Serialize>(
        &self,
        path: &str,
        token: &str,
        body: &T,
    ) -> TestResponse {
        self.request(Method::PATCH, path)
            .bearer(token)
            .json(body)
            .send()
            .await
    }
}

/// A request being built against a [`TestApp`].
pub struct TestRequest {
    router: Router,
    method: Method,
    path: String,
    token: Option<String>,
    raw_auth_value: bool,
    body: Option<(Vec<u8>, &'static str)>,
    headers: Vec<(String, String)>,
}

impl TestRequest {
    /// Attach a bearer token (the raw JWT, without the `Bearer ` prefix).
    pub fn bearer(mut self, token: &str) -> Self {
        self.token = Some(token.to_string());
        self.raw_auth_value = false;
        self
    }

    /// Attach a raw `Authorization` header value verbatim (for malformed-token
    /// tests, e.g. `"Bearer garbage"` or `"Basic x"`).
    pub fn raw_auth(mut self, value: &str) -> Self {
        self.token = Some(value.to_string());
        self.raw_auth_value = true;
        self
    }

    /// Add a request header (`Range`, say).
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Attach a JSON body (also sets `Content-Type: application/json`).
    pub fn json<T: Serialize>(mut self, body: &T) -> Self {
        let bytes = serde_json::to_vec(body).expect("serialize JSON test body");
        self.body = Some((bytes, "application/json"));
        self
    }

    /// Attach a raw body with its content type.
    #[allow(dead_code)]
    pub fn raw_body(mut self, body: &str, content_type: &'static str) -> Self {
        self.body = Some((body.as_bytes().to_vec(), content_type));
        self
    }

    /// Send the request and collect the response.
    pub async fn send(self) -> TestResponse {
        let mut builder = Request::builder().method(self.method).uri(&self.path);
        if let Some(token) = &self.token {
            let value = if self.raw_auth_value {
                token.clone()
            } else {
                format!("Bearer {token}")
            };
            builder = builder.header(header::AUTHORIZATION, value);
        }
        for (name, value) in &self.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        // Production serves with `into_make_service_with_connect_info`, so every
        // real request carries a `ConnectInfo<SocketAddr>`. The rate-limit layer
        // on the auth routes reads it (peer-IP keying), so inject a default here
        // to mirror production; without it the limiter fails to extract a key.
        builder = builder.extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            50000,
        ))));
        let body = match self.body {
            Some((bytes, content_type)) => {
                builder = builder.header(header::CONTENT_TYPE, content_type);
                Body::from(bytes)
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .oneshot(builder.body(body).expect("build request"))
            .await
            .expect("router response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .expect("read response body");
        TestResponse {
            status,
            headers,
            body: bytes.to_vec(),
        }
    }
}

/// A collected response: status code plus raw body bytes.
pub struct TestResponse {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl TestResponse {
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Deserialize the body as JSON into `T`.
    pub fn json<T: DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "failed to deserialize response body as JSON: {e}\nbody: {}",
                self.text()
            )
        })
    }

    /// The body as a UTF-8 string (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// Mint a raw access-token JWT for the given user id and permissions.
pub fn token_for(user_id: u32, perms: UserPermissions) -> String {
    generate_tokens("harness-tester".to_string(), user_id, perms)
        .expect("generate token")
        .access_token
}

/// Mint a raw access-token JWT carrying the given permissions.
///
/// Uses a high-numbered "ghost" user id that no fixture inserts or grants
/// permissions to. With no rows in `Monitors_Permissions` or
/// `Groups_Permissions`, `MonitorScope::resolve` returns `MonitorScope::All`
/// — unrestricted monitor access for the tests that don't care about
/// row-level ACL.
pub fn token_with(perms: UserPermissions) -> String {
    token_for(GHOST_TEST_UID, perms)
}

/// Test-only user id deliberately chosen to be unused by any fixture or
/// seeded user in the dev database. Sits well above the ranges other
/// `it_*` tests pick (990_xxx, 9_876_543) so it cannot collide.
const GHOST_TEST_UID: u32 = 999_999_001;

/// Mint a raw access-token JWT with full `Edit` permissions everywhere.
pub fn superuser_token() -> String {
    token_with(UserPermissions::superuser())
}
