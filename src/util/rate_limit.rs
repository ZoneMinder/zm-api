//! Rate-limiting key extraction.
//!
//! The key is the client's IP. Forwarding headers (`X-Forwarded-For`,
//! `X-Real-IP`) are written by whoever sends them, so they are believed only
//! when the connection comes from a trusted proxy
//! (`server.middleware.trusted_proxies`, loopback by default), and then only
//! as far as the proxies go: the client is the rightmost address in
//! `X-Forwarded-For` that isn't a trusted proxy. Proxies append the address
//! they saw to whatever the client sent, so the leftmost entries are the
//! client's own and prove nothing.
//!
//! `tower_governor`'s `SmartIpKeyExtractor` trusts the headers from anyone and
//! takes the leftmost address, so a client could mint a fresh bucket per
//! request, directly or through a proxy.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request};
use axum::http::{HeaderValue, Method, Response};
use futures::future::BoxFuture;
use ipnet::IpNet;
use tower::{Layer, Service, ServiceExt};
use tower_governor::errors::GovernorError;
use tower_governor::key_extractor::KeyExtractor;

/// Loopback: a proxy on the same host (ZoneMinder's Apache).
pub const DEFAULT_TRUSTED_PROXIES: [&str; 2] = ["127.0.0.0/8", "::1/128"];

/// Rate-limit key = the client IP.
///
/// - `trust_proxy = false`: the socket peer IP, whatever the headers say.
/// - `trust_proxy = true`: as above unless the peer is in `trusted`; then
///   the client address the trusted proxies passed on.
#[derive(Debug, Clone)]
pub struct IpKeyExtractor {
    pub trust_proxy: bool,
    pub trusted: Arc<[IpNet]>,
}

impl IpKeyExtractor {
    /// Parse `trusted_proxies` (addresses or CIDRs); bad entries are logged
    /// and skipped.
    pub fn new(trust_proxy: bool, trusted_proxies: &[String]) -> Self {
        let trusted: Vec<IpNet> = trusted_proxies
            .iter()
            .filter_map(|s| {
                let s = s.trim();
                s.parse::<IpNet>()
                    .or_else(|_| s.parse::<IpAddr>().map(IpNet::from))
                    .map_err(|_| {
                        tracing::warn!("server.middleware.trusted_proxies: {s:?} is not an address or CIDR; ignored")
                    })
                    .ok()
            })
            .collect();
        Self {
            trust_proxy,
            trusted: trusted.into(),
        }
    }

    fn is_trusted(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.trusted.iter().any(|n| n.contains(&ip))
    }

    /// The client's address for a request from `peer`.
    pub fn client_ip(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        if !self.trust_proxy || !self.is_trusted(peer) {
            return peer;
        }
        let chain: Vec<&str> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if chain.is_empty() {
            return headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_ip)
                .unwrap_or(peer);
        }
        // From the right: each trusted hop vouches for the address before it.
        let mut client = peer;
        for entry in chain.iter().rev() {
            let Some(ip) = parse_ip(entry) else {
                // Can't read the chain past here; the last address that was
                // vouched for stands.
                return client;
            };
            client = ip;
            if !self.is_trusted(ip) {
                return ip;
            }
        }
        client
    }
}

/// IPv4-mapped IPv6 as plain IPv4, so `::ffff:127.0.0.1` is loopback.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// An address as proxies write it: bare, `[v6]`, or with a port.
fn parse_ip(s: &str) -> Option<IpAddr> {
    let s = s.trim();
    s.parse::<IpAddr>()
        .ok()
        .or_else(|| s.parse::<SocketAddr>().ok().map(|a| a.ip()))
        .or_else(|| {
            s.strip_prefix('[')
                .and_then(|r| r.strip_suffix(']'))
                .and_then(|r| r.parse().ok())
        })
        .map(canonical)
}

impl KeyExtractor for IpKeyExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(a)| a.ip())
            .ok_or(GovernorError::UnableToExtractKey)?;
        Ok(self.client_ip(peer, req.headers()))
    }
}

/// HLS playlist, init and segment fetches for a live session. A player
/// makes about one a second per tile, so a four-tile montage spent the
/// general budget in under a minute (#257). The session already gates them,
/// and starting one goes through the limiter. `hls/link` mints a signed URL
/// and stays limited.
pub fn is_live_media_fetch(method: &Method, path: &str) -> bool {
    if method != Method::GET {
        return false;
    }
    let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    match parts.as_slice() {
        [.., "live", id, "hls", file] => {
            id.parse::<u32>().is_ok() && !file.is_empty() && *file != "link"
        }
        _ => false,
    }
}

/// The limiter's 429. tower_governor puts the wait in whole seconds,
/// rounded down, so with one token a second `retry-after` read 0 and a
/// client that obeyed it came straight back (#257). Round up instead.
pub fn too_many_requests(err: GovernorError) -> Response<axum::body::Body> {
    let mut response: Response<axum::body::Body> = err.clone().into();
    if let GovernorError::TooManyRequests { wait_time, .. } = err {
        let secs = HeaderValue::from(wait_time.saturating_add(1));
        response.headers_mut().insert("retry-after", secs.clone());
        response.headers_mut().insert("x-ratelimit-after", secs);
    }
    response
}

/// Applies a layer (the rate limiter) to every request except the ones
/// `exempt` picks out, which go straight to the inner service.
#[derive(Clone)]
pub struct ExemptLayer<L> {
    layer: L,
    exempt: fn(&Method, &str) -> bool,
}

impl<L> ExemptLayer<L> {
    pub fn new(layer: L, exempt: fn(&Method, &str) -> bool) -> Self {
        Self { layer, exempt }
    }
}

impl<L, S> Layer<S> for ExemptLayer<L>
where
    L: Layer<S>,
    S: Clone,
{
    type Service = Exempt<L::Service, S>;

    fn layer(&self, inner: S) -> Self::Service {
        Exempt {
            limited: self.layer.layer(inner.clone()),
            plain: inner,
            exempt: self.exempt,
        }
    }
}

#[derive(Clone)]
pub struct Exempt<A, B> {
    limited: A,
    plain: B,
    exempt: fn(&Method, &str) -> bool,
}

impl<A, B, ReqBody> Service<Request<ReqBody>> for Exempt<A, B>
where
    A: Service<Request<ReqBody>> + Clone + Send + 'static,
    A::Future: Send + 'static,
    B: Service<Request<ReqBody>, Response = A::Response, Error = A::Error> + Clone + Send + 'static,
    B::Future: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = A::Response;
    type Error = A::Error;
    type Future = BoxFuture<'static, Result<A::Response, A::Error>>;

    // Readiness is checked on the clone that takes the request (`oneshot`),
    // since which service that is depends on the request.
    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        if (self.exempt)(req.method(), req.uri().path()) {
            Box::pin(self.plain.clone().oneshot(req))
        } else {
            Box::pin(self.limited.clone().oneshot(req))
        }
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;
    use axum::{body::Body, routing::get, Router};
    use std::net::Ipv4Addr;
    use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};

    #[test]
    fn only_hls_media_gets_are_exempt() {
        let get = Method::GET;
        for path in [
            "/api/v3/live/3/hls/live.m3u8",
            "/api/v3/live/3/hls/master.m3u8",
            "/api/v3/live/3/hls/init.mp4",
            "/api/v3/live/12/hls/segment_42.m4s",
            "/zm-api/api/v3/live/3/hls/live.m3u8",
        ] {
            assert!(is_live_media_fetch(&get, path), "{path}");
        }
        for (method, path) in [
            (Method::POST, "/api/v3/live/3/hls/link"),
            (Method::GET, "/api/v3/live/3/hls/link"),
            (Method::POST, "/api/v3/live/3/start"),
            (Method::GET, "/api/v3/live/sessions"),
            (Method::GET, "/api/v3/live/x/hls/live.m3u8"),
            (Method::GET, "/api/v3/events"),
            (Method::DELETE, "/api/v3/live/3/hls/live.m3u8"),
        ] {
            assert!(!is_live_media_fetch(&method, path), "{method} {path}");
        }
    }

    async fn status(app: &Router, path: &str) -> (u16, Option<String>) {
        let mut req = Request::builder().uri(path).body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)),
            40000,
        )));
        let resp = app.clone().oneshot(req).await.unwrap();
        let retry = resp
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap().to_string());
        (resp.status().as_u16(), retry)
    }

    #[tokio::test]
    async fn hls_fetches_skip_the_limiter_and_429s_say_when_to_retry() {
        let conf = GovernorConfigBuilder::default()
            .per_second(1)
            .burst_size(1)
            .key_extractor(IpKeyExtractor::new(false, &[]))
            .finish()
            .unwrap();
        let limiter = ExemptLayer::new(
            GovernorLayer::new(conf).error_handler(too_many_requests),
            is_live_media_fetch,
        );
        let app = Router::new()
            .route("/api/v3/events", get(|| async { "ok" }))
            .route("/api/v3/live/{id}/hls/{file}", get(|| async { "seg" }))
            .layer(limiter);

        assert_eq!(status(&app, "/api/v3/events").await.0, 200);
        let (code, retry) = status(&app, "/api/v3/events").await;
        assert_eq!(code, 429);
        let retry: u64 = retry.expect("retry-after").parse().unwrap();
        assert!(retry >= 1, "retry-after {retry}");

        for n in 0..20 {
            let path = format!("/api/v3/live/3/hls/seg_{n}.m4s");
            assert_eq!(status(&app, &path).await.0, 200, "{path}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn defaults(trust_proxy: bool) -> IpKeyExtractor {
        let list: Vec<String> = DEFAULT_TRUSTED_PROXIES
            .iter()
            .map(|s| s.to_string())
            .collect();
        IpKeyExtractor::new(trust_proxy, &list)
    }

    fn key(e: &IpKeyExtractor, peer: &str, headers: &[(&str, &str)]) -> IpAddr {
        let mut builder = Request::builder().uri("/");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let mut req = builder.body(()).unwrap();
        let peer: IpAddr = peer.parse().unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(peer, 40000)));
        e.extract(&req).expect("key")
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn untrusted_uses_peer_ip_and_ignores_forwarded_header() {
        let e = defaults(false);
        assert_eq!(
            key(&e, "127.0.0.1", &[("x-forwarded-for", "198.51.100.99")]),
            ip("127.0.0.1")
        );
    }

    /// Exposed directly: a client's own header changes nothing, even with
    /// header trust on.
    #[test]
    fn a_direct_client_cannot_pick_its_key() {
        let e = defaults(true);
        for spoof in ["198.51.100.99", "10.10.10.10", "127.0.0.1"] {
            assert_eq!(
                key(
                    &e,
                    "203.0.113.7",
                    &[("x-forwarded-for", spoof), ("x-real-ip", spoof)]
                ),
                ip("203.0.113.7"),
                "{spoof}"
            );
        }
    }

    #[test]
    fn behind_a_local_proxy_the_client_is_what_the_proxy_saw() {
        let e = defaults(true);
        assert_eq!(
            key(&e, "127.0.0.1", &[("x-forwarded-for", "198.51.100.99")]),
            ip("198.51.100.99")
        );
        // The client sent its own header; the proxy appended the real address.
        assert_eq!(
            key(
                &e,
                "127.0.0.1",
                &[("x-forwarded-for", "6.6.6.6, 198.51.100.99")]
            ),
            ip("198.51.100.99")
        );
        // Split across two header lines, as some proxies send it.
        assert_eq!(
            key(
                &e,
                "::1",
                &[
                    ("x-forwarded-for", "6.6.6.6"),
                    ("x-forwarded-for", "198.51.100.99")
                ]
            ),
            ip("198.51.100.99")
        );
        assert_eq!(
            key(&e, "127.0.0.1", &[("x-real-ip", "198.51.100.5")]),
            ip("198.51.100.5")
        );
        assert_eq!(key(&e, "127.0.0.1", &[]), ip("127.0.0.1"));
    }

    #[test]
    fn a_chain_of_trusted_proxies_is_followed() {
        let e = IpKeyExtractor::new(true, &["127.0.0.1".into(), "10.0.0.0/8".into()]);
        assert_eq!(
            key(
                &e,
                "10.0.0.3",
                &[("x-forwarded-for", "6.6.6.6, 198.51.100.99, 10.0.0.2")]
            ),
            ip("198.51.100.99")
        );
    }

    #[test]
    fn unreadable_entries_and_odd_forms() {
        let e = defaults(true);
        // Junk where the proxy's entry should be: fall back to the proxy.
        assert_eq!(
            key(&e, "127.0.0.1", &[("x-forwarded-for", "1.2.3.4, junk")]),
            ip("127.0.0.1")
        );
        assert_eq!(
            key(
                &e,
                "127.0.0.1",
                &[("x-forwarded-for", "198.51.100.99:5555")]
            ),
            ip("198.51.100.99")
        );
        assert_eq!(
            key(&e, "127.0.0.1", &[("x-forwarded-for", "[2001:db8::1]")]),
            ip("2001:db8::1")
        );
        // A v4-mapped peer is loopback too.
        assert_eq!(
            key(
                &e,
                "::ffff:127.0.0.1",
                &[("x-forwarded-for", "198.51.100.99")]
            ),
            ip("198.51.100.99")
        );
        let _ = Ipv4Addr::LOCALHOST;
    }

    #[test]
    fn bad_trusted_entries_are_skipped() {
        let e = IpKeyExtractor::new(true, &["not-an-ip".into(), "127.0.0.1".into()]);
        assert_eq!(e.trusted.len(), 1);
    }
}
