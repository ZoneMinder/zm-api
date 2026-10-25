use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub addr: String,
    pub port: u16,
    #[serde(default)]
    pub tls: Option<ServerTlsConfig>,
    #[serde(default)]
    pub acme: Option<ServerAcmeConfig>,
    /// Browser origins allowed to call the API (CORS). Accepts a TOML array
    /// or, so a single environment variable can carry the whole list, one
    /// comma-separated string: `APP_SERVER__ALLOWED_ORIGINS=https://zm.example.com`.
    ///
    /// Each entry is an exact origin (`https://zm.example.com`) or a host with
    /// a `:*` port wildcard (`http://localhost:*`). Empty means "unset", which
    /// falls back to the legacy `ALLOWED_ORIGINS` variable and then to a
    /// localhost-only default — see `routes::resolve_allowed_origins`.
    #[serde(default, deserialize_with = "de_string_or_seq")]
    pub allowed_origins: Vec<String>,
    /// HTTP middleware tuning (body limits, rate limiting).
    #[serde(default)]
    pub middleware: MiddlewareConfig,
    /// Path the reverse proxy serves zm-api under, e.g. `/zm` when it maps
    /// `/zm/api/v3/` to `/api/v3/` so the API sits with the rest of ZoneMinder.
    /// Routes do not change; only URLs zm-api writes into responses get it:
    /// stream and media links, the WHEP session `Location`, search results,
    /// Swagger UI's document and the OpenAPI server. Empty (the default) means
    /// the site root.
    #[serde(default)]
    pub public_prefix: String,
}

static PUBLIC_PREFIX: OnceLock<String> = OnceLock::new();

/// Record `ServerConfig::public_prefix` once at startup. Refuses a value
/// that isn't a plain path, such as a full URL.
pub fn set_public_prefix(prefix: &str) -> Result<(), String> {
    let _ = PUBLIC_PREFIX.set(normalize_prefix(prefix)?);
    Ok(())
}

/// The prefix in effect: empty, or `/`-led with no trailing `/`.
pub fn public_prefix() -> &'static str {
    PUBLIC_PREFIX.get().map(String::as_str).unwrap_or("")
}

/// `path`, as zm-api routes it, the way a client behind the proxy reaches it.
pub fn public_path(path: &str) -> String {
    format!("{}{path}", public_prefix())
}

fn normalize_prefix(prefix: &str) -> Result<String, String> {
    let p = prefix.trim().trim_end_matches('/');
    let p = if p.is_empty() || p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    };
    let plain_path = p
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/-._~%".contains(c));
    let segments_ok = p
        .split('/')
        .skip(1)
        .all(|s| !s.is_empty() && s != "." && s != "..");
    if !plain_path || !segments_ok {
        return Err(format!(
            "server.public_prefix must be a path such as /zm, not {prefix:?}"
        ));
    }
    Ok(p)
}

/// Accept either a comma-separated string or a sequence. TOML users write an
/// array; the `APP_` environment layer can only ever deliver a string, and
/// `APP_SERVER__ALLOWED_ORIGINS__0=` indexing is a poor fit for a setting most
/// people set once to a single origin.
fn de_string_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrSeq {
        String(String),
        Seq(Vec<String>),
    }

    let split = |s: &str| -> Vec<String> {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    };

    Ok(match StringOrSeq::deserialize(deserializer)? {
        StringOrSeq::String(s) => split(&s),
        // Split sequence entries too, so a one-element array holding a
        // comma-separated string behaves the same as the bare string.
        StringOrSeq::Seq(v) => v.iter().flat_map(|s| split(s)).collect(),
    })
}

/// Tuning for the cross-cutting HTTP middleware stack.
#[derive(Debug, Deserialize, Clone)]
pub struct MiddlewareConfig {
    /// Maximum accepted request body size, in bytes.
    #[serde(default = "default_body_limit_bytes")]
    pub body_limit_bytes: usize,
    /// Seconds between quota replenishments for the global limiter — one
    /// request's worth of budget is restored every this many seconds. `0`
    /// disables the global limiter entirely.
    ///
    /// The old name for this was `rate_limit_per_second`, which read as a rate
    /// and is the opposite of what it means: setting it to `4` expecting four
    /// requests a second gives one request every four seconds. It is still
    /// accepted so existing configs keep working (GH #70).
    #[serde(default, alias = "rate_limit_per_second")]
    pub rate_limit_period_secs: u64,

    /// How many requests a client may make before the replenish rate binds.
    ///
    /// This is the number that decides whether an ordinary page load works: a
    /// single-page app renders one screen from a dozen or so requests, and a
    /// burst below that means no page ever loads. Ignored when the limiter is
    /// disabled.
    #[serde(default)]
    pub rate_limit_burst: u32,
    /// Trust proxy forwarding headers (`X-Forwarded-For` / `X-Real-IP`) for
    /// rate-limit keying. Leave `false` unless the service sits behind a
    /// trusted reverse proxy: when the service is exposed directly these
    /// headers are attacker-controlled and would let a client mint a fresh
    /// rate-limit bucket per request. When `false`, the socket peer IP is used.
    #[serde(default)]
    pub trust_proxy_headers: bool,
    /// Proxies whose forwarding headers are believed when
    /// `trust_proxy_headers` is on: addresses or CIDRs. Loopback by default
    /// (a proxy on the same host). A request from anywhere else is keyed on
    /// its own address, whatever its headers say.
    #[serde(default = "default_trusted_proxies")]
    pub trusted_proxies: Vec<String>,
    /// Governor replenish interval, in seconds, for the dedicated limiter on
    /// the authentication endpoints (`/auth/login`, `/auth/refresh`, and the
    /// other `/auth` + `/me` routes). Kept tight to blunt credential
    /// brute-forcing regardless of the global limiter. `0` disables it.
    #[serde(default = "default_auth_rate_limit_period_secs")]
    pub auth_rate_limit_period_secs: u64,
    /// Burst allowance for the auth-endpoint limiter. `0` disables it.
    #[serde(default = "default_auth_rate_limit_burst")]
    pub auth_rate_limit_burst: u32,
}

fn default_body_limit_bytes() -> usize {
    2 * 1024 * 1024
}

fn default_trusted_proxies() -> Vec<String> {
    crate::util::rate_limit::DEFAULT_TRUSTED_PROXIES
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_auth_rate_limit_period_secs() -> u64 {
    2
}

fn default_auth_rate_limit_burst() -> u32 {
    10
}

impl Default for MiddlewareConfig {
    fn default() -> Self {
        Self {
            body_limit_bytes: default_body_limit_bytes(),
            rate_limit_period_secs: 0,
            rate_limit_burst: 0,
            trust_proxy_headers: false,
            trusted_proxies: default_trusted_proxies(),
            auth_rate_limit_period_secs: default_auth_rate_limit_period_secs(),
            auth_rate_limit_burst: default_auth_rate_limit_burst(),
        }
    }
}

impl MiddlewareConfig {
    /// Burst the global limiter should actually use.
    ///
    /// A burst of 0 with the limiter enabled used to be clamped silently to 1,
    /// which is the most hostile value there is: one request succeeds and
    /// everything after it is refused, so no page in any client ever loads and
    /// nothing says why (GH #70). A burst that small is never intentional, so
    /// treat it as the misconfiguration it is — the caller substitutes a
    /// usable default and logs it, rather than bricking the API silently or
    /// refusing to start over a tuning value.
    pub fn effective_rate_limit_burst(&self) -> (u32, bool) {
        const USABLE_DEFAULT: u32 = 60;
        if self.rate_limit_burst == 0 {
            (USABLE_DEFAULT, true)
        } else {
            (self.rate_limit_burst, false)
        }
    }

    /// Whether the global per-IP rate limiter should be installed.
    pub fn rate_limiting_enabled(&self) -> bool {
        self.rate_limit_period_secs > 0
    }

    /// Whether the dedicated auth-endpoint rate limiter should be installed.
    /// On by default (brute-force protection matters even when the global
    /// limiter is off); disable by setting either field to `0`.
    pub fn auth_rate_limiting_enabled(&self) -> bool {
        self.auth_rate_limit_period_secs > 0 && self.auth_rate_limit_burst > 0
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerTlsConfig {
    #[serde(default)]
    pub enabled: bool,
    pub cert_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerAcmeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub contact_emails: Vec<String>,
    pub cache_dir: Option<PathBuf>,
    #[serde(default)]
    pub production: bool,
    #[serde(default)]
    pub challenge: AcmeChallenge,
    pub http_port: Option<u16>,
}

#[derive(Debug, Default, Deserialize, Clone, Copy)]
pub enum AcmeChallenge {
    #[serde(rename = "tls-alpn-01", alias = "tls-alpn01")]
    #[default]
    TlsAlpn01,
    #[serde(rename = "http-01", alias = "http01")]
    Http01,
}

impl ServerConfig {
    pub fn get_addr(&self) -> String {
        format!("{}:{}", self.addr, self.port)
    }

    pub fn get_http_addr(&self) -> String {
        format!("http://{}:{}", self.addr, self.port)
    }
    pub fn get_socket_addr(&self) -> Result<SocketAddr, AddrParseError> {
        self.get_addr().parse()
    }
}

#[cfg(test)]
pub mod tests {

    use super::*;

    /// The deployment path that matters: `APP_SERVER__ALLOWED_ORIGINS` arriving
    /// through the real `config` env layer. Worth its own test because the
    /// string-or-seq deserializer is `untagged`, which needs the backend to
    /// support `deserialize_any` — serde_json proves nothing about the `config`
    /// crate's own deserializer. Uses `Environment::source` rather than real
    /// environment variables, which are process-global and race other tests.
    #[test]
    fn allowed_origins_round_trips_through_the_config_env_layer() {
        let env = config::Environment::with_prefix("APP")
            .prefix_separator("_")
            .separator("__")
            .source(Some(
                [(
                    "APP_SERVER__ALLOWED_ORIGINS".to_string(),
                    "https://zm.example.com,https://alt.example.com".to_string(),
                )]
                .into_iter()
                .collect(),
            ));

        let cfg = config::Config::builder()
            .set_default("server.addr", "0.0.0.0")
            .unwrap()
            .set_default("server.port", 8080)
            .unwrap()
            .add_source(env)
            .build()
            .expect("build config")
            .get::<ServerConfig>("server")
            .expect("deserialize ServerConfig");

        assert_eq!(
            cfg.allowed_origins,
            vec!["https://zm.example.com", "https://alt.example.com"]
        );
    }

    #[test]
    pub fn app_config_http_addr_test() {
        let config = ServerConfig {
            addr: "127.0.0.1".to_string(),
            port: 1024,
            tls: None,
            acme: None,
            allowed_origins: Vec::new(),
            middleware: MiddlewareConfig::default(),
            public_prefix: String::new(),
        };
        assert_eq!(config.get_http_addr(), "http://127.0.0.1:1024");
    }

    #[test]
    fn public_prefix_is_normalized() {
        assert_eq!(normalize_prefix("").unwrap(), "");
        assert_eq!(normalize_prefix("/").unwrap(), "");
        assert_eq!(normalize_prefix("/zm").unwrap(), "/zm");
        assert_eq!(normalize_prefix("/zm/").unwrap(), "/zm");
        assert_eq!(normalize_prefix(" zm ").unwrap(), "/zm");
        assert_eq!(normalize_prefix("/cams/zm").unwrap(), "/cams/zm");
    }

    #[test]
    fn public_prefix_must_be_a_plain_path() {
        for bad in [
            "https://host/zm",
            "/zm?x=1",
            "/zm#top",
            "/z m",
            "//zm",
            "/zm//api",
            "/zm/../etc",
            "/zm\\x",
        ] {
            assert!(normalize_prefix(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    pub fn middleware_config_defaults() {
        let mw = MiddlewareConfig::default();
        assert_eq!(mw.body_limit_bytes, 2 * 1024 * 1024);
        // Global limiter off by default; auth limiter on by default.
        assert!(!mw.rate_limiting_enabled());
        assert!(mw.auth_rate_limiting_enabled());
        // Forwarding headers are not trusted by default (no proxy assumed).
        assert!(!mw.trust_proxy_headers);
    }

    #[test]
    pub fn auth_rate_limiting_can_be_disabled() {
        let mw = MiddlewareConfig {
            auth_rate_limit_burst: 0,
            ..Default::default()
        };
        assert!(!mw.auth_rate_limiting_enabled());
        let mw = MiddlewareConfig {
            auth_rate_limit_period_secs: 0,
            ..Default::default()
        };
        assert!(!mw.auth_rate_limiting_enabled());
    }

    #[derive(Debug, Deserialize)]
    struct OriginsHolder {
        #[serde(default, deserialize_with = "de_string_or_seq")]
        allowed_origins: Vec<String>,
    }

    fn parse_origins(json: &str) -> Vec<String> {
        serde_json::from_str::<OriginsHolder>(json)
            .expect("parse")
            .allowed_origins
    }

    #[test]
    fn allowed_origins_accepts_a_sequence() {
        // The shape a TOML array in base.toml deserializes to.
        assert_eq!(
            parse_origins(r#"{"allowed_origins": ["https://a.example", "http://localhost:*"]}"#),
            vec!["https://a.example", "http://localhost:*"]
        );
    }

    #[test]
    fn allowed_origins_accepts_a_comma_separated_string() {
        // The only shape the APP_SERVER__ALLOWED_ORIGINS env layer can produce.
        assert_eq!(
            parse_origins(r#"{"allowed_origins": "https://a.example, http://localhost:*"}"#),
            vec!["https://a.example", "http://localhost:*"]
        );
    }

    #[test]
    fn allowed_origins_defaults_to_empty_and_drops_blanks() {
        assert!(parse_origins("{}").is_empty());
        assert!(parse_origins(r#"{"allowed_origins": "  ,, "}"#).is_empty());
        assert!(parse_origins(r#"{"allowed_origins": []}"#).is_empty());
    }

    /// GH #70: a burst of 0 with the limiter on used to be clamped silently to
    /// 1, so one request succeeded and every one after it was refused — no
    /// client could load a page, and nothing explained why.
    #[test]
    fn a_zero_burst_is_treated_as_misconfiguration_not_as_one() {
        let mw = MiddlewareConfig {
            rate_limit_period_secs: 1,
            rate_limit_burst: 0,
            ..Default::default()
        };
        let (burst, defaulted) = mw.effective_rate_limit_burst();
        assert!(
            defaulted,
            "the caller must be able to log that it substituted"
        );
        assert!(
            burst >= 30,
            "the substituted burst has to fit a page load, got {burst}"
        );
    }

    #[test]
    fn an_explicit_burst_is_left_alone() {
        let mw = MiddlewareConfig {
            rate_limit_period_secs: 1,
            rate_limit_burst: 5,
            ..Default::default()
        };
        // Deliberately small, but deliberate. Don't second-guess it.
        assert_eq!(mw.effective_rate_limit_burst(), (5, false));
    }

    /// The old name meant the opposite of what it read as, so it has to keep
    /// working rather than silently reverting an operator to "disabled".
    #[test]
    fn the_old_setting_name_is_still_accepted() {
        let by_old_name: MiddlewareConfig =
            serde_json::from_str(r#"{"rate_limit_per_second": 25}"#).expect("parse");
        assert_eq!(by_old_name.rate_limit_period_secs, 25);
        assert!(by_old_name.rate_limiting_enabled());

        let by_new_name: MiddlewareConfig =
            serde_json::from_str(r#"{"rate_limit_period_secs": 25}"#).expect("parse");
        assert_eq!(by_new_name.rate_limit_period_secs, 25);
    }

    /// The shipped production values must let an ordinary page load through.
    /// This is the property the issue is about, so assert it on the real file
    /// rather than on a hand-written config.
    #[test]
    fn the_shipped_production_burst_fits_a_page_load() {
        // zm-web's events page needs about eleven requests for one screen.
        const REQUESTS_PER_PAGE: u32 = 11;

        let prod = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("settings/prod.toml"),
        )
        .expect("read settings/prod.toml");

        let value_of = |key: &str| -> u64 {
            prod.lines()
                .find_map(|l| l.trim().strip_prefix(key)?.trim().strip_prefix('='))
                .unwrap_or_else(|| panic!("{key} not found in prod.toml"))
                .trim()
                .parse()
                .expect("numeric")
        };

        let burst = value_of("rate_limit_burst") as u32;
        let period = value_of("rate_limit_period_secs");

        assert!(
            burst >= REQUESTS_PER_PAGE * 2,
            "a burst of {burst} leaves no headroom for a {REQUESTS_PER_PAGE}-request \
             page load plus a second navigation"
        );
        assert!(
            period > 0 && period <= 2,
            "a refill of one request every {period}s throttles a browsing user to \
             a crawl once the burst is spent"
        );
    }
}
