use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StreamingConfig {
    pub enabled: bool,
    pub default_protocol: StreamingProtocol,
    pub source: SourceConfig,
    pub zoneminder: ZoneMinderConfig,
    pub webrtc: WebRtcConfig,
    pub hls: HlsConfig,
    pub rtsp: RtspConfig,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_protocol: StreamingProtocol::default(),
            source: SourceConfig::default(),
            zoneminder: ZoneMinderConfig::default(),
            webrtc: WebRtcConfig::default(),
            hls: HlsConfig::default(),
            rtsp: RtspConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StreamingProtocol {
    #[default]
    Auto,
    Webrtc,
    Hls,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SourceConfig {
    pub priority: Vec<String>, // ["socket", "rtsp", "go2rtc"]
    pub prefer_direct_rtsp: bool,
    pub fallback_to_go2rtc: bool,
    pub cache_sdp_seconds: u32,
    /// Monitors whose stream-socket reader is kept hot ("always-warm pool"), so
    /// the first viewer skips cold reader spin-up and the keyframe cache is
    /// pre-populated (instant codec + an injectable keyframe). Empty = none.
    pub prewarm_monitors: Vec<u32>,
    /// How often the warm-keeper re-ensures each `prewarm_monitors` reader is
    /// alive (restarting any the HLS reaper or a crash stopped). `0` disables
    /// pre-warming entirely.
    pub prewarm_interval_seconds: u64,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            priority: vec![
                "socket".to_string(),
                "rtsp".to_string(),
                "go2rtc".to_string(),
            ],
            prefer_direct_rtsp: false,
            fallback_to_go2rtc: true,
            cache_sdp_seconds: 300,
            prewarm_monitors: Vec::new(),
            prewarm_interval_seconds: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ZoneMinderConfig {
    pub enabled: bool,
    /// Directory holding zmc's per-monitor stream sockets
    /// (`stream_{monitor_id}.sock`) — ZoneMinder's `ZM_PATH_SOCKS`.
    pub socks_path: String,
    /// Stream-socket read timeout. zmc sends STATS every ~5s even when no
    /// media flows, so this mostly guards against a wedged peer.
    pub read_timeout_ms: u64,
    pub reconnect_delay_ms: u64,
    /// Path to ZoneMinder events directory
    pub events_dir: String,
}

impl Default for ZoneMinderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            socks_path: "/run/zm".to_string(),
            read_timeout_ms: 10_000,
            reconnect_delay_ms: 1000,
            events_dir: "/var/lib/zoneminder/events".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebRtcConfig {
    pub enabled: bool,
    pub mode: String, // "native" | "plugin" | "go2rtc"
    pub max_connections: u32,
    pub connection_timeout_seconds: u64,
    pub stun_servers: Vec<String>,
    pub turn: Option<TurnConfig>,
    /// Public IPs to advertise instead of this host's own, for 1:1 NAT or a
    /// container whose address browsers can't reach. Empty: the host's own.
    pub nat_ips: Vec<String>,
    /// UDP port range for media (one port per session). Unset: ephemeral
    /// ports chosen by the OS.
    pub udp_port_min: Option<u16>,
    pub udp_port_max: Option<u16>,
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: "go2rtc".to_string(),
            max_connections: 100,
            connection_timeout_seconds: 30,
            stun_servers: vec![
                "stun:stun.l.google.com:19302".to_string(),
                "stun:stun1.l.google.com:19302".to_string(),
            ],
            turn: None,
            nat_ips: Vec::new(),
            udp_port_min: None,
            udp_port_max: None,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct TurnConfig {
    pub enabled: bool,
    pub server: String,
    pub username: String,
    pub password: String,
}

// Manual Debug: the TURN password must not come out with the config.
impl std::fmt::Debug for TurnConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnConfig")
            .field("enabled", &self.enabled)
            .field("server", &self.server)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server: "turn:turn.example.com:3478".to_string(),
            username: String::new(),
            password: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HlsConfig {
    pub enabled: bool,
    pub segment_duration_seconds: u32,
    pub playlist_size: u32,
    pub ll_hls_enabled: bool,
    pub partial_segment_ms: u32,
    /// Tear down an HLS session whose playlist/segments have not been requested
    /// for this many seconds (a viewer that navigated away). `0` disables idle
    /// reaping.
    ///
    /// MUST comfortably exceed the LL-HLS blocking-request hold time (5s, see
    /// `handlers::live`) plus one `segment_duration_seconds`, or an actively
    /// blocking LL-HLS player could be reaped between requests. The 90s default
    /// leaves a wide margin.
    pub idle_timeout_seconds: u64,
    pub storage: HlsStorageConfig,
}

impl Default for HlsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            segment_duration_seconds: 2,
            playlist_size: 10,
            ll_hls_enabled: false,
            partial_segment_ms: 200,
            idle_timeout_seconds: 90,
            storage: HlsStorageConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HlsStorageConfig {
    pub path: String,
    pub retention_minutes: u32,
}

impl Default for HlsStorageConfig {
    fn default() -> Self {
        Self {
            path: "/tmp/hls".to_string(),
            retention_minutes: 10,
        }
    }
}

/// zm-api's own RTSP server (`streaming::rtsp`): live video for players that
/// only take a URL (Home Assistant, VLC, NVRs), opened by a signed link
/// from `POST /api/v3/live/{id}/rtsp/link`. Off by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RtspConfig {
    pub enabled: bool,
    /// Address to listen on.
    pub bind: String,
    /// Plain RTSP port. ZoneMinder's own `zm_rtsp_server` defaults to 8554
    /// too: run one or the other, or move this one.
    pub port: u16,
    /// RTSPS (RTSP over TLS) port, using `server.tls`'s certificate and key.
    /// Unset: no RTSPS listener.
    pub tls_port: Option<u16>,
    /// Host name to put in the links. Unset: the host the link request was
    /// sent to.
    pub public_host: Option<String>,
    /// Most connections at once, across both listeners.
    pub max_connections: usize,
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "0.0.0.0".to_string(),
            port: 8554,
            tls_port: None,
            public_host: None,
            max_connections: 64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_turn_password_is_not_printed_with_the_config() {
        let turn = TurnConfig {
            password: "hunter2".to_string(),
            ..TurnConfig::default()
        };
        assert!(!format!("{turn:?}").contains("hunter2"));
    }

    #[test]
    fn test_streaming_config_default() {
        let config = StreamingConfig::default();
        assert!(config.enabled);
        assert_eq!(config.source.priority.len(), 3);
        assert_eq!(config.source.priority[0], "socket");
    }

    #[test]
    fn test_zoneminder_config_default() {
        let config = ZoneMinderConfig::default();
        assert!(config.enabled);
        assert_eq!(config.socks_path, "/run/zm");
        assert_eq!(config.read_timeout_ms, 10_000);
        assert_eq!(config.reconnect_delay_ms, 1000);
        assert_eq!(config.events_dir, "/var/lib/zoneminder/events");
    }

    #[test]
    fn test_streaming_protocol_default() {
        let protocol = StreamingProtocol::default();
        assert!(matches!(protocol, StreamingProtocol::Auto));
    }

    #[test]
    fn test_webrtc_config_default() {
        let config = WebRtcConfig::default();
        assert!(config.enabled);
        assert_eq!(config.mode, "go2rtc");
        assert_eq!(config.stun_servers.len(), 2);
        assert!(config.turn.is_none());
    }

    #[test]
    fn test_hls_config_default() {
        let config = HlsConfig::default();
        assert!(config.enabled);
        assert_eq!(config.segment_duration_seconds, 2);
        assert!(!config.ll_hls_enabled);
    }
}

#[cfg(test)]
mod removed_block_tests {
    use super::StreamingConfig;

    /// GH #53: `[streaming.go2rtc]` and `[streaming.rtsp_proxy]` were removed
    /// because nothing read them. An operator who copied the old `base.toml`
    /// still has those blocks, so deserialization must ignore them rather than
    /// refuse to start — no struct here sets `deny_unknown_fields`, and this
    /// pins that.
    #[test]
    fn a_config_still_carrying_the_removed_blocks_loads() {
        let legacy = r#"
            {
              "go2rtc":     { "enabled": true, "base_url": "http://localhost:1984" },
              "rtsp_proxy": { "enabled": true, "port": 8554 },
              "zoneminder": { "socks_path": "/run/zm" }
            }
        "#;
        let parsed: Result<StreamingConfig, _> = serde_json::from_str(legacy);
        assert!(
            parsed.is_ok(),
            "an upgrade must not fail on config blocks that no longer exist: {:?}",
            parsed.err()
        );
    }
}
