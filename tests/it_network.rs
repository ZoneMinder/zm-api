//! The server over a real network connection (#238).
//!
//! The rest of the suite drives the router in-process with `oneshot`, so it
//! never parses HTTP off a socket, negotiates TLS, or sees a client's address.
//! These start the app on 127.0.0.1 the way `server::run` serves it (with
//! `ConnectInfo`, and through the same TLS configuration) and call it with an
//! HTTP client.
//!
//!   APP_PROFILE=test-db cargo test --test it_network -- --include-ignored

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use common::harness::TestApp;
use serde_json::json;

/// Serve `router` over plain HTTP, as `server::run` does without TLS.
async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

/// Serve `router` over TLS from certificate files, as `server.tls` does.
async fn serve_tls(router: Router) -> SocketAddr {
    // As main() does before any TLS.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls");
    let tls = zm_api::server::tls_from_pem_files(
        format!("{dir}/localhost.crt"),
        format!("{dir}/localhost.key"),
    )
    .await
    .expect("test certificate");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let server = axum_server::from_tcp_rustls(listener, tls).unwrap();
    tokio::spawn(async move {
        server
            .serve(router.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
    });
    addr
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn tls_from_certificate_files_negotiates_http2() {
    let app = TestApp::spawn().await;
    let addr = serve_tls(app.router()).await;
    let url = format!(
        "https://localhost:{}/api/v3/server/health_check",
        addr.port()
    );

    // reqwest's default client lands on HTTP/1.1 here though curl gets h2
    // from the same server. Offering h2 alone makes this a check of the
    // server: the request fails unless the server's ALPN accepts h2.
    let h2 = reqwest::Client::builder()
        .no_proxy()
        .resolve("localhost", addr)
        .danger_accept_invalid_certs(true)
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let r = h2.get(&url).send().await.expect("TLS request");
    assert_eq!(r.status(), 200);
    assert_eq!(r.version(), reqwest::Version::HTTP_2, "ALPN chose h2");

    // A client that only speaks HTTP/1.1 is served too.
    let h1 = reqwest::Client::builder()
        .no_proxy()
        .resolve("localhost", addr)
        .danger_accept_invalid_certs(true)
        .http1_only()
        .build()
        .unwrap();
    let r = h1.get(&url).send().await.expect("TLS request");
    assert_eq!(r.status(), 200);
    assert_eq!(r.version(), reqwest::Version::HTTP_11);
}

#[test]
fn acme_mode_advertises_http2() {
    let state = rustls_acme::AcmeConfig::new(["cams.example.com"]).state();
    let config = zm_api::server::acme_server_config(&state);
    assert_eq!(
        config.alpn_protocols,
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    );
}

/// Bad logins until the limiter answers 429, each tagged with `forwarded`
/// as `X-Forwarded-For`. Whether it ever did.
async fn exhaust(addr: SocketAddr, forwarded: &str) -> bool {
    let url = format!("http://{addr}/api/v3/auth/login");
    for _ in 0..40 {
        let r = client()
            .post(&url)
            .header("x-forwarded-for", forwarded)
            .json(&json!({ "username": "nobody-here", "password": "wrong" }))
            .send()
            .await
            .unwrap();
        if r.status() == 429 {
            return true;
        }
    }
    false
}

async fn status_from(addr: SocketAddr, forwarded: &str) -> u16 {
    client()
        .post(format!("http://{addr}/api/v3/auth/login"))
        .header("x-forwarded-for", forwarded)
        .json(&json!({ "username": "nobody-here", "password": "wrong" }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn behind_a_trusted_proxy_each_client_has_its_own_bucket() {
    let app = TestApp::spawn_with(|state| {
        let mut config = (*state.config).clone();
        config.server.middleware.trust_proxy_headers = true;
        state.config = Arc::new(config);
    })
    .await;
    let addr = serve(app.router()).await;

    assert!(
        exhaust(addr, "203.0.113.1").await,
        "the limiter never answered 429"
    );
    assert_eq!(status_from(addr, "203.0.113.1").await, 429);
    // Another client behind the same proxy isn't limited.
    assert_ne!(status_from(addr, "203.0.113.2").await, 429);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn without_trusted_proxies_the_header_changes_nothing() {
    let app = TestApp::spawn_with(|state| {
        let mut config = (*state.config).clone();
        config.server.middleware.trust_proxy_headers = false;
        state.config = Arc::new(config);
    })
    .await;
    let addr = serve(app.router()).await;

    assert!(
        exhaust(addr, "203.0.113.1").await,
        "the limiter never answered 429"
    );
    // A made-up X-Forwarded-For doesn't buy a fresh bucket.
    assert_eq!(status_from(addr, "203.0.113.2").await, 429);
}

/// Two clients on different loopback addresses are told apart by the
/// connection alone. Linux routes all of 127.0.0.0/8 to loopback; macOS only
/// 127.0.0.1 unless aliased, so this runs on Linux.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn the_limiter_keys_on_the_peer_address() {
    let app = TestApp::spawn().await;
    let addr = serve(app.router()).await;
    let from = |ip: [u8; 4]| {
        reqwest::Client::builder()
            .no_proxy()
            .local_address(std::net::IpAddr::from(ip))
            .build()
            .unwrap()
    };
    let url = format!("http://{addr}/api/v3/auth/login");
    let bad = json!({ "username": "nobody-here", "password": "wrong" });

    let first = from([127, 0, 0, 1]);
    let mut limited = false;
    for _ in 0..40 {
        if first.post(&url).json(&bad).send().await.unwrap().status() == 429 {
            limited = true;
            break;
        }
    }
    assert!(limited, "the limiter never answered 429");
    let other = from([127, 0, 0, 2]);
    assert_ne!(
        other.post(&url).json(&bad).send().await.unwrap().status(),
        429
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn cors_preflight_answers_allowed_origins_only() {
    let app = TestApp::spawn_with(|state| {
        let mut config = (*state.config).clone();
        config.server.allowed_origins = vec!["https://cams.example.com".into()];
        state.config = Arc::new(config);
    })
    .await;
    let addr = serve(app.router()).await;
    let preflight = |origin: &'static str| {
        client()
            .request(
                reqwest::Method::OPTIONS,
                format!("http://{addr}/api/v3/monitors"),
            )
            .header("origin", origin)
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "authorization")
            .send()
    };

    let r = preflight("https://cams.example.com").await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    assert_eq!(
        r.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://cams.example.com")
    );
    let r = preflight("https://evil.example.net").await.unwrap();
    assert!(
        r.headers().get("access-control-allow-origin").is_none(),
        "a foreign origin is not allowed"
    );
}
