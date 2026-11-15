//! WebRTC live streaming from per-monitor stream-socket sources
//!
//! This module provides WebRTC streaming using the webrtc-rs crate (0.21,
//! an async layer over the Sans-I/O `rtc` core). It uses
//! `TrackLocalStaticSample`, which handles RTP packetization, SRTP
//! encryption and interceptor integration via `write_sample()`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceCandidateType,
    RTCIceConnectionState, RTCIceGatheringState, RTCIceServer, RTCPeerConnectionIceEvent,
    RTCPeerConnectionState, RTCSessionDescription, Registry, SettingEngineBuilder,
};
use webrtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit, RtpSender};

use crate::streaming::live::audio::AacToOpusTranscoder;
use crate::streaming::source::{
    h264_nal_type, h265_nal_type, slice_starts_picture, VideoCodec, VideoPacket,
};
use crate::streaming::source::{AudioPacket, CachedKeyframe};

// Re-export for consumers that imported from here
pub use crate::streaming::source::extract_profile_level_id;

// ============================================================================
// Access Unit Assembler
// ============================================================================

/// Check if an H.264 NAL unit is a VCL (Video Coding Layer) unit — i.e. a
/// coded slice that is part of a picture.  Types 1-5 are VCL.
fn is_h264_vcl(nal_type: u8) -> bool {
    (1..=5).contains(&nal_type)
}

/// Check if an H.265 NAL unit is a VCL unit. HEVC reserves types 0–31 for
/// coded slice segments (trailing, leading, and IRAP pictures).
fn is_h265_vcl(nal_type: u8) -> bool {
    nal_type <= 31
}

/// Per-NAL classification the assembler needs, resolved per the packet's
/// codec: the NAL unit type, whether it is a VCL (slice) NAL, and whether it
/// delimits the start of a new access unit (parameter sets and AUD).
fn classify_nal(data: &[u8], codec: VideoCodec) -> Option<(u8, bool, bool)> {
    match codec {
        VideoCodec::H265 => {
            let t = h265_nal_type(data)?;
            // VPS (32), SPS (33), PPS (34) and AUD (35) delimit a new AU —
            // the HEVC analogue of H.264's SPS/PPS/AUD.
            Some((t, is_h265_vcl(t), (32..=35).contains(&t)))
        }
        // Unknown is treated as H.264, matching `slice_starts_picture`.
        VideoCodec::H264 | VideoCodec::Unknown => {
            let t = h264_nal_type(data)?;
            // SPS (7), PPS (8) and AUD (9) delimit a new AU.
            Some((t, is_h264_vcl(t), (7..=9).contains(&t)))
        }
    }
}

/// Assembles individual NAL units into complete Access Units (frames).
///
/// The socket reader emits one NAL unit per `VideoPacket`. WebRTC's H.264 and
/// H.265 payloaders work correctly with Annex B data containing multiple
/// NALs, but each `write_sample()` call gets a distinct RTP timestamp. For
/// correct decoding, all NALs belonging to the same picture must share
/// one timestamp. This assembler buffers non-VCL NALs (parameter sets, SEI)
/// and emits a complete AU when it sees the first VCL NAL of the *next*
/// picture — an H.264 slice with `first_mb_in_slice == 0`, or an H.265
/// slice segment with `first_slice_segment_in_pic_flag` set. Continuation
/// slices of a multi-slice picture are kept in the same AU so the RTP
/// packetizer assigns the whole frame one timestamp and a single trailing
/// marker bit. The codec is taken per-packet from `VideoPacket::codec`.
pub struct AccessUnitAssembler {
    /// Buffered Annex B data for the current access unit being assembled
    buf: Vec<u8>,
    /// Whether we have seen at least one VCL NAL in the current AU
    has_vcl: bool,
    /// Timestamp of the most recent VCL NAL added
    timestamp_us: i64,
    /// Whether the current AU contains a keyframe
    is_keyframe: bool,
    /// When true, drop assembled AUs until a keyframe is seen.
    /// Ensures the stream starts with SPS+PPS+IDR for decoder init.
    needs_keyframe: bool,
}

impl Default for AccessUnitAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl AccessUnitAssembler {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(128 * 1024),
            has_vcl: false,
            timestamp_us: 0,
            is_keyframe: false,
            needs_keyframe: true,
        }
    }

    /// Feed a NAL unit (VideoPacket) into the assembler.
    ///
    /// Returns `Some(assembled_au)` when a complete access unit is ready
    /// to be sent to the WebRTC track. The returned bytes are Annex B
    /// formatted (start codes included), suitable for the H264 payloader.
    pub fn push(&mut self, packet: &VideoPacket) -> Option<AssembledAccessUnit> {
        if packet.data.is_empty() {
            return None;
        }

        let (nal_type, vcl, starts_new_au) = match classify_nal(&packet.data, packet.codec) {
            Some(c) => c,
            None => {
                // Not a recognizable NAL — append to current buffer and continue
                self.buf.extend_from_slice(&packet.data);
                return None;
            }
        };

        if self.needs_keyframe {
            info!(
                "AU assembler: NAL type={}, is_keyframe={}, has_vcl={}, buf={}B",
                nal_type,
                packet.is_keyframe,
                self.has_vcl,
                self.buf.len()
            );
        }

        // Parameter sets (H.264 SPS/PPS, H.265 VPS/SPS/PPS) and AUDs all
        // delimit the start of a new access unit. Flush any buffered VCL data
        // when one arrives so it begins a fresh AU — parameter sets group
        // with the following keyframe, not with the preceding slice.

        if starts_new_au && self.has_vcl {
            let au = self.flush();
            self.buf.extend_from_slice(&packet.data);
            return self.filter_needs_keyframe(au);
        }

        if vcl && self.has_vcl {
            // A VCL NAL only completes the current access unit when it begins
            // a new primary coded picture (`first_mb_in_slice == 0`). The
            // remaining slices of a multi-slice picture — 4K cameras emit
            // several slice NALs per frame — carry `first_mb_in_slice > 0` and
            // belong to the SAME access unit. They must be appended, not split
            // off, so the H264 RTP packetizer keeps every slice of the picture
            // under one RTP timestamp with a single trailing marker bit.
            if slice_starts_picture(&packet.data, packet.codec) {
                let au = self.flush();

                // Start a new AU with this slice.
                self.buf.extend_from_slice(&packet.data);
                self.has_vcl = true;
                self.timestamp_us = packet.timestamp_us;
                self.is_keyframe = packet.is_keyframe;

                return self.filter_needs_keyframe(au);
            }
            // Continuation slice of the current picture — fall through to
            // append it to the access unit already being assembled.
        }

        // Append to current AU
        self.buf.extend_from_slice(&packet.data);
        if vcl {
            self.has_vcl = true;
            self.timestamp_us = packet.timestamp_us;
            if packet.is_keyframe {
                self.is_keyframe = true;
            }
        }
        None
    }

    /// Drop non-keyframe AUs while `needs_keyframe` is set, ensuring the
    /// stream starts with SPS+PPS+IDR for correct decoder initialisation.
    fn filter_needs_keyframe(
        &mut self,
        au: Option<AssembledAccessUnit>,
    ) -> Option<AssembledAccessUnit> {
        match au {
            Some(au) if self.needs_keyframe && !au.is_keyframe => {
                debug!(
                    "Dropping non-keyframe AU ({} bytes) while waiting for keyframe",
                    au.data.len()
                );
                None
            }
            Some(au) => {
                self.needs_keyframe = false;
                Some(au)
            }
            None => None,
        }
    }

    /// Mark the assembler as having received a keyframe externally.
    ///
    /// Call this after injecting a cached keyframe (SPS+PPS+IDR) into the
    /// WebRTC track so that subsequent P-frames are not dropped.
    pub fn clear_needs_keyframe(&mut self) {
        self.needs_keyframe = false;
    }

    /// Flush the current buffer as a complete access unit.
    fn flush(&mut self) -> Option<AssembledAccessUnit> {
        if self.buf.is_empty() {
            return None;
        }

        let data = std::mem::take(&mut self.buf);
        let au = AssembledAccessUnit {
            data,
            timestamp_us: self.timestamp_us,
            is_keyframe: self.is_keyframe,
        };
        self.has_vcl = false;
        self.is_keyframe = false;
        Some(au)
    }
}

/// A complete access unit (one video frame) ready for RTP packetization
pub struct AssembledAccessUnit {
    /// Annex B formatted data (may contain multiple NAL units with start codes)
    pub data: Vec<u8>,
    /// Timestamp in microseconds
    pub timestamp_us: i64,
    /// Whether this AU is a keyframe
    pub is_keyframe: bool,
}

/// The audio codec a session's audio track carries on the wire.
///
/// WebRTC's mandatory audio codecs are Opus and G.711 — G.711 cameras pass
/// through untouched (PCMU/PCMA), AAC cameras are transcoded to Opus
/// upstream of the track (see `streaming::live::audio`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioTrackKind {
    /// G.711 µ-law pass-through (8 kHz mono, payload type 0)
    Pcmu,
    /// G.711 A-law pass-through (8 kHz mono, payload type 8)
    Pcma,
    /// Opus (48 kHz, payload type 111) — AAC sources transcoded upstream
    Opus,
}

impl AudioTrackKind {
    fn capability(&self) -> RTCRtpCodec {
        match self {
            AudioTrackKind::Pcmu => RTCRtpCodec {
                mime_type: "audio/PCMU".to_string(),
                clock_rate: 8000,
                channels: 1,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![],
            },
            AudioTrackKind::Pcma => RTCRtpCodec {
                mime_type: "audio/PCMA".to_string(),
                clock_rate: 8000,
                channels: 1,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![],
            },
            AudioTrackKind::Opus => RTCRtpCodec {
                mime_type: "audio/opus".to_string(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
                rtcp_feedback: vec![],
            },
        }
    }

    fn payload_type(&self) -> u8 {
        match self {
            AudioTrackKind::Pcmu => 0,
            AudioTrackKind::Pcma => 8,
            AudioTrackKind::Opus => 111,
        }
    }
}

// ============================================================================
// WebRTC Session
// ============================================================================

/// WebRTC session state
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebRtcSessionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
}

/// WebRTC session statistics
#[derive(Debug, Clone, serde::Serialize)]
pub struct WebRtcSessionStats {
    pub session_id: String,
    pub monitor_id: u32,
    pub state: WebRtcSessionState,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// One outgoing media track and what `write_sample` needs to address it.
///
/// webrtc 0.21 stamps each packet with an explicit SSRC and payload type
/// instead of binding a codec by mime type. The SSRC is chosen when the track
/// is built; the payload type is the one the browser's answer settled on, read
/// from the sender once the answer is applied.
pub struct SendTrack {
    pub track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    payload_type: OnceLock<u8>,
}

impl SendTrack {
    /// Build a single-encoding sendonly track and attach it to `pc`.
    async fn attach(
        pc: &Arc<dyn PeerConnection>,
        kind: RtpCodecKind,
        codec: RTCRtpCodec,
        track_id: &str,
        stream_id: &str,
    ) -> Result<Self, WebRtcLiveError> {
        let ssrc = rand::random::<u32>();
        let track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    stream_id.to_string(),
                    track_id.to_string(),
                    track_id.to_string(),
                    kind,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(ssrc),
                            ..Default::default()
                        },
                        codec,
                        ..Default::default()
                    }],
                ),
            )
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?,
        );
        let transceiver = pc
            .add_transceiver_from_track(
                Arc::clone(&track) as Arc<dyn TrackLocal>,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Sendonly,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?;
        let sender = transceiver
            .sender()
            .await
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?
            .ok_or_else(|| {
                WebRtcLiveError::WebRtcError("sendonly transceiver has no sender".to_string())
            })?;
        Ok(Self {
            track,
            sender,
            ssrc,
            payload_type: OnceLock::new(),
        })
    }

    /// Like [`Self::attach`], for the answering side: `add_track` binds the
    /// track to the remote offer's transceiver of the same kind.
    async fn attach_to_offer(
        pc: &Arc<dyn PeerConnection>,
        kind: RtpCodecKind,
        codec: RTCRtpCodec,
        track_id: &str,
        stream_id: &str,
    ) -> Result<Self, WebRtcLiveError> {
        let ssrc = rand::random::<u32>();
        let track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    stream_id.to_string(),
                    track_id.to_string(),
                    track_id.to_string(),
                    kind,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(ssrc),
                            ..Default::default()
                        },
                        codec,
                        ..Default::default()
                    }],
                ),
            )
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?,
        );
        let sender = pc
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?;
        Ok(Self {
            track,
            sender,
            ssrc,
            payload_type: OnceLock::new(),
        })
    }

    /// Record the payload type the answer negotiated for this sender. The
    /// first codec in the sender's parameters is the one the browser chose.
    async fn resolve_payload_type(&self) -> Result<(), WebRtcLiveError> {
        let params = self
            .sender
            .get_parameters()
            .await
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?;
        let pt = params
            .rtp_parameters
            .codecs
            .first()
            .map(|c| c.payload_type)
            .ok_or_else(|| {
                WebRtcLiveError::WebRtcError("answer negotiated no codec for track".to_string())
            })?;
        let _ = self.payload_type.set(pt);
        Ok(())
    }

    async fn write(&self, data: &[u8], duration: Duration) -> Result<(), String> {
        let Some(&pt) = self.payload_type.get() else {
            return Err("no negotiated payload type (answer not applied)".to_string());
        };
        let sample = Sample {
            data: bytes::Bytes::copy_from_slice(data),
            duration,
            ..Sample::new(Instant::now())
        };
        self.track
            .write_sample(self.ssrc, pt, &sample, &[])
            .await
            .map_err(|e| e.to_string())
    }
}

/// A WebRTC streaming session
pub struct WebRtcLiveSession {
    pub id: Uuid,
    pub monitor_id: u32,
    pub state: RwLock<WebRtcSessionState>,
    pub peer_connection: Arc<dyn PeerConnection>,
    pub video_track: SendTrack,
    /// Audio track — present when the monitor delivers audio the session
    /// can carry (G.711 pass-through or transcoded Opus)
    pub audio_track: Option<SendTrack>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    packets_sent: AtomicU64,
    bytes_sent: AtomicU64,
    /// Candidates the viewer has sent so far.
    remote_candidates: AtomicU64,
}

/// The most ICE candidates a viewer may send for one session. A real client
/// sends a handful. Each is paired with every local candidate and probed, so
/// a viewer sending them without end would keep the server checking
/// connectivity to addresses of the viewer's choosing.
const MAX_REMOTE_CANDIDATES: u64 = 64;

/// The largest SDP a viewer may send. Real offers and answers are a few
/// kilobytes.
const MAX_SDP_BYTES: usize = 64 * 1024;

/// Refuse a viewer's SDP that is far larger than any real one, or that
/// carries more candidates than [`MAX_REMOTE_CANDIDATES`]: candidates in the
/// SDP are probed like trickled ones, and would otherwise get round that
/// limit.
pub fn check_remote_sdp(sdp: &str) -> Result<(), String> {
    if sdp.len() > MAX_SDP_BYTES {
        return Err(format!("the SDP is larger than {MAX_SDP_BYTES} bytes"));
    }
    let candidates = sdp
        .lines()
        .filter(|l| l.trim_start().starts_with("a=candidate:"))
        .count() as u64;
    if candidates > MAX_REMOTE_CANDIDATES {
        return Err(format!(
            "the SDP lists {candidates} ICE candidates; at most {MAX_REMOTE_CANDIDATES}"
        ));
    }
    Ok(())
}

/// Count one more candidate against `sent`; false once past the limit.
fn candidate_allowed(sent: &AtomicU64) -> bool {
    sent.fetch_add(1, Ordering::Relaxed) < MAX_REMOTE_CANDIDATES
}

impl WebRtcLiveSession {
    /// Get session statistics
    pub async fn stats(&self) -> WebRtcSessionStats {
        WebRtcSessionStats {
            session_id: self.id.to_string(),
            monitor_id: self.monitor_id,
            state: *self.state.read().await,
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            created_at: self.created_at,
        }
    }

    /// Update session state
    pub async fn set_state(&self, state: WebRtcSessionState) {
        *self.state.write().await = state;
    }

    /// Get current state
    pub async fn get_state(&self) -> WebRtcSessionState {
        *self.state.read().await
    }

    /// Write a complete access unit (one video frame) to the WebRTC track.
    ///
    /// The `data` should be Annex B formatted and may contain multiple NAL
    /// units (e.g. SPS + PPS + IDR for a keyframe). The H264 payloader
    /// splits them and assigns the same RTP timestamp to all.
    ///
    /// Uses `TrackLocalStaticSample::write_sample()` which handles RTP
    /// packetization, fragmentation, SRTP encryption, and interceptor
    /// integration (RTCP feedback, NACK, etc.) automatically.
    pub async fn write_access_unit(&self, au: &AssembledAccessUnit) -> Result<(), WebRtcLiveError> {
        if au.data.is_empty() {
            return Ok(());
        }

        // ~30fps default; the packetizer uses duration to compute RTP timestamps
        let duration = Duration::from_millis(33);

        self.video_track
            .write(&au.data, duration)
            .await
            .map_err(|e| WebRtcLiveError::WebRtcError(format!("Failed to write sample: {}", e)))?;

        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent
            .fetch_add(au.data.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Write one audio sample (a G.711 chunk or one Opus frame) to the
    /// session's audio track. `duration` drives the RTP timestamp advance.
    pub async fn write_audio_sample(
        &self,
        data: &[u8],
        duration: Duration,
    ) -> Result<(), WebRtcLiveError> {
        let Some(track) = &self.audio_track else {
            return Ok(());
        };
        if data.is_empty() {
            return Ok(());
        }

        track.write(data, duration).await.map_err(|e| {
            WebRtcLiveError::WebRtcError(format!("Failed to write audio sample: {}", e))
        })?;

        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent
            .fetch_add(data.len() as u64, Ordering::Relaxed);

        Ok(())
    }
}

/// WebRTC live streaming errors
#[derive(Debug, thiserror::Error)]
pub enum WebRtcLiveError {
    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Session already exists for monitor {0}")]
    SessionExists(u32),

    #[error("WebRTC error: {0}")]
    WebRtcError(String),

    #[error("Codec not supported: {0}")]
    UnsupportedCodec(String),

    #[error("No video track available")]
    NoVideoTrack,
    #[error("Invalid offer: {0}")]
    InvalidOffer(String),
}

/// Configuration for WebRTC live streaming
#[derive(Debug, Clone)]
pub struct WebRtcLiveConfig {
    pub stun_servers: Vec<String>,
    pub turn_server: Option<String>,
    pub turn_username: Option<String>,
    pub turn_password: Option<String>,
    pub max_sessions: usize,
    /// Public IPs to advertise in place of this host's own (1:1 NAT, Docker).
    pub nat_ips: Vec<String>,
    /// Bind each session's UDP socket to a free port in this range, so a
    /// firewall or container only has to forward the range.
    pub udp_ports: Option<(u16, u16)>,
}

impl Default for WebRtcLiveConfig {
    fn default() -> Self {
        Self {
            stun_servers: vec![
                "stun:stun.l.google.com:19302".to_string(),
                "stun:stun1.l.google.com:19302".to_string(),
            ],
            turn_server: None,
            turn_username: None,
            turn_password: None,
            max_sessions: 100,
            nat_ips: Vec::new(),
            udp_ports: None,
        }
    }
}

impl WebRtcLiveConfig {
    /// Build a live-streaming config from the application's `[streaming.webrtc]`
    /// settings, so the operator's configured STUN/TURN servers actually take
    /// effect on the live WS path (it previously always used the hardcoded
    /// Google STUN defaults). An **empty** `stun_servers` list is honored
    /// verbatim — on a LAN-only deployment that is the right setting: host
    /// candidates alone let ICE gathering complete in milliseconds instead of
    /// stalling on the ~5s STUN gather timeout when Google STUN is unreachable.
    /// See REVIEW_FIXES_PLAN §2.2.
    pub fn from_webrtc_config(cfg: &crate::configure::streaming::WebRtcConfig) -> Self {
        let (turn_server, turn_username, turn_password) = match &cfg.turn {
            Some(turn) if turn.enabled => (
                Some(turn.server.clone()),
                Some(turn.username.clone()),
                Some(turn.password.clone()),
            ),
            _ => (None, None, None),
        };
        Self {
            stun_servers: cfg.stun_servers.clone(),
            turn_server,
            turn_username,
            turn_password,
            max_sessions: cfg.max_connections as usize,
            nat_ips: cfg.nat_ips.clone(),
            udp_ports: match (cfg.udp_port_min, cfg.udp_port_max) {
                (Some(lo), Some(hi)) if lo <= hi => Some((lo, hi)),
                _ => None,
            },
        }
    }
}

/// The UDP wildcards each peer connection binds: IPv4 always, IPv6 when the
/// host can open an IPv6 socket.
///
/// A failed bind fails the whole peer connection in webrtc 0.21, and hosts
/// and containers with IPv6 switched off reject `[::]`, so IPv6 is probed
/// once rather than assumed.
fn udp_bind_addrs(ports: Option<(u16, u16)>) -> Vec<String> {
    static IPV6: OnceLock<bool> = OnceLock::new();
    let ipv6 = *IPV6.get_or_init(|| std::net::UdpSocket::bind("[::]:0").is_ok());
    let addrs_for = |port: u16| {
        let mut addrs = vec![format!("0.0.0.0:{port}")];
        if ipv6 {
            addrs.push(format!("[::]:{port}"));
        }
        addrs
    };
    if let Some((lo, hi)) = ports {
        // The first port free on every family. Another session can take it
        // between this probe and the peer connection's bind; that bind then
        // fails and the session errors, which the client retries.
        for port in lo..=hi {
            let free = std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok()
                && (!ipv6 || std::net::UdpSocket::bind(("::", port)).is_ok());
            if free {
                return addrs_for(port);
            }
        }
        warn!("WebRTC: no free UDP port in {lo}-{hi}; using an ephemeral port");
    }
    addrs_for(0)
}

/// Relays peer-connection events to the signaling handler's channels.
struct LiveEventHandler {
    monitor_id: u32,
    pc_state_tx: tokio::sync::watch::Sender<RTCPeerConnectionState>,
    ice_connected_tx: tokio::sync::watch::Sender<bool>,
    candidate_tx: tokio::sync::mpsc::UnboundedSender<RTCIceCandidateInit>,
    gathering_tx: tokio::sync::watch::Sender<bool>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for LiveEventHandler {
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        info!(
            monitor_id = self.monitor_id,
            state = ?state,
            "WebRTC peer connection state changed"
        );
        let _ = self.pc_state_tx.send(state);
    }

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState) {
        info!(
            monitor_id = self.monitor_id,
            state = ?state,
            "WebRTC ICE connection state changed"
        );
        // Mark ICE connected so the handler can time the ICE→DTLS split.
        // DTLS can only start once a candidate pair is usable, so this is
        // the lower bound of the offer→connected gap.
        if matches!(
            state,
            RTCIceConnectionState::Connected | RTCIceConnectionState::Completed
        ) {
            let _ = self.ice_connected_tx.send(true);
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathering_tx.send(true);
        }
    }

    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        match event.candidate.to_json() {
            Ok(init) => {
                let _ = self.candidate_tx.send(init);
            }
            Err(e) => {
                debug!(
                    "monitor {}: failed to serialize ICE candidate: {e}",
                    self.monitor_id
                );
            }
        }
    }
}

/// Manager for WebRTC live streaming sessions
pub struct WebRtcLiveManager {
    config: WebRtcLiveConfig,
    sessions: DashMap<String, Arc<RwLock<WebRtcLiveSession>>>,
    /// Map from monitor_id to list of session IDs
    monitor_sessions: DashMap<u32, Vec<String>>,
}

/// Everything the signaling handler needs after a session's offer is built.
///
/// With **trickle ICE** the offer is returned immediately — the server no longer
/// blocks on `gathering_complete_promise()` (which stalled `offer_ms` on STUN
/// round-trips). Locally gathered candidates arrive on `candidate_rx` and the
/// handler relays them to the browser over the signaling channel; the browser's
/// `ClientHello` can then land as soon as a candidate pair connects instead of
/// waiting out a DTLS retransmit. `ice_connected_rx` flips `true` at ICE-connected
/// so the handler can record the ICE→DTLS split separately.
pub struct SessionHandshake {
    pub session_id: String,
    pub offer: RTCSessionDescription,
    pub pc_state_rx: tokio::sync::watch::Receiver<RTCPeerConnectionState>,
    pub ice_connected_rx: tokio::sync::watch::Receiver<bool>,
    pub candidate_rx: tokio::sync::mpsc::UnboundedReceiver<RTCIceCandidateInit>,
    /// Flips `true` when local ICE gathering completes.
    pub gathering_rx: tokio::sync::watch::Receiver<bool>,
}

/// The H.264 `profile-level-id` values to advertise in the SDP offer.
///
/// These are **not** the camera's native profile. The server is a pure
/// pass-through — it forwards the camera's real H.264 NAL units unchanged —
/// and in practice every WebRTC decoder (Safari's VideoToolbox, Chromium's
/// decoder) decodes whatever NAL units actually arrive regardless of the
/// negotiated `profile-level-id`. So the offer advertises only the profiles
/// every browser is guaranteed to accept:
///
/// * `42e01f` — Constrained Baseline, Level 3.1. The universal baseline; every
///   WebRTC implementation accepts it.
/// * `640c1f` — Constrained High, Level 3.1.
///
/// Safari accepts *only* these two. It rejects Main (`4d…`) outright, and
/// plain non-constrained High (`6400…`) is not the same as Constrained High
/// (`640c…`). Advertising the camera's native Main/High profile therefore
/// made Safari answer with a rejected `m=video 0` line, after which the server
/// errored "codec is not supported by remote" and dropped the connection.
///
/// `level-asymmetry-allowed=1` (set in the fmtp line) lets the bytes the
/// server actually sends stay at the camera's real level — 5.1 for 4K — even
/// though the negotiated level is the conservative 3.1. Profile is not subject
/// to level asymmetry, which is exactly why the offered *profiles* must be
/// ones the decoder accepts; it then decodes the real Main/High bitstream.
fn h264_offer_profile_level_ids() -> Vec<String> {
    vec!["42e01f".to_string(), "640c1f".to_string()]
}

/// Add the session-level `a=ice-options:trickle` attribute to an offer if absent,
/// so the answerer is told candidates arrive incrementally (RFC 8838). Inserts
/// once, immediately before the first media (`m=`) line; the rest of the SDP —
/// including the ICE ufrag/pwd and DTLS fingerprint — is untouched, so the
/// peer connection's own local description still matches what the browser
/// answers against.
fn ensure_trickle_ice_option(
    offer: RTCSessionDescription,
) -> Result<RTCSessionDescription, WebRtcLiveError> {
    if offer.sdp.contains("a=ice-options:trickle") {
        return Ok(offer);
    }
    // SDP lines are CRLF-terminated; match the newline before the first `m=` so
    // we insert a properly-terminated session-level attribute line. Fall back to
    // returning the offer unchanged if the SDP has no media section.
    let mut sdp = offer.sdp;
    // No media section → nothing to anchor against; leave the SDP unchanged.
    if let Some(nl) = sdp.find("\nm=") {
        sdp.insert_str(nl + 1, "a=ice-options:trickle\r\n");
    }
    RTCSessionDescription::offer(sdp).map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))
}

/// A peer connection with its event channels, before tracks are attached.
struct BuiltPeer {
    peer_connection: Arc<dyn PeerConnection>,
    codec_capability: RTCRtpCodec,
    pc_state_rx: tokio::sync::watch::Receiver<RTCPeerConnectionState>,
    ice_connected_rx: tokio::sync::watch::Receiver<bool>,
    candidate_rx: tokio::sync::mpsc::UnboundedReceiver<RTCIceCandidateInit>,
    gathering_rx: tokio::sync::watch::Receiver<bool>,
}

impl WebRtcLiveManager {
    /// Create a new WebRTC live manager
    pub fn new(config: WebRtcLiveConfig) -> Self {
        Self {
            config,
            sessions: DashMap::new(),
            monitor_sessions: DashMap::new(),
        }
    }

    /// The peer connection for a session: codecs registered, ICE configured,
    /// event channels wired, no tracks yet.
    async fn build_peer(
        &self,
        monitor_id: u32,
        codec: VideoCodec,
        profile_level_id: Option<&str>,
        audio: Option<AudioTrackKind>,
    ) -> Result<BuiltPeer, WebRtcLiveError> {
        // Check session limit
        if self.sessions.len() >= self.config.max_sessions {
            return Err(WebRtcLiveError::WebRtcError(
                "Maximum sessions reached".to_string(),
            ));
        }

        // Create media engine with codec support
        let mut media_engine = MediaEngine::default();

        // Build the codec capabilities to advertise. For H.264 the offer
        // advertises browser-universal pass-through profiles, *not* the
        // camera's native one — see `h264_offer_profile_level_ids`.
        let codec_capabilities: Vec<RTCRtpCodec> = match codec {
            VideoCodec::H264 => {
                debug!(
                    "Monitor {monitor_id}: camera H.264 profile {}; advertising \
                     pass-through profiles 42e01f/640c1f for browser compatibility",
                    profile_level_id.unwrap_or("unknown")
                );
                h264_offer_profile_level_ids()
                    .into_iter()
                    .map(|variant| RTCRtpCodec {
                        mime_type: "video/H264".to_string(),
                        clock_rate: 90000,
                        channels: 0,
                        sdp_fmtp_line: format!(
                            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={variant}"
                        ),
                        rtcp_feedback: vec![],
                    })
                    .collect()
            }
            // H.265 fmtp per RFC 7798 §7.1: Main profile (profile-id=1),
            // Main tier — what surveillance cameras emit. Same pass-through
            // philosophy as H.264 above: advertise what the *decoder*
            // accepts and send the camera's real bitstream. level-id=120
            // (Level 4) is a conservative negotiation value; hardware
            // decoders handle the camera's true level regardless. Adjust
            // against Safari's actual answer when the browser test loop
            // exists (HEVC_WEBRTC_TASKS §1.3).
            VideoCodec::H265 => vec![RTCRtpCodec {
                mime_type: "video/H265".to_string(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "profile-id=1;tier-flag=0;level-id=120".to_string(),
                rtcp_feedback: vec![],
            }],
            VideoCodec::Unknown => {
                return Err(WebRtcLiveError::UnsupportedCodec("Unknown".to_string()));
            }
        };

        // The pass-through track binds by mime type, so any advertised
        // capability works; use the first. Negotiation settles on one of the
        // offered profiles, but the bytes we send are always the camera's
        // real bitstream — the decoder handles it regardless of profile.
        let codec_capability = codec_capabilities[0].clone();

        // Payload types 96+ are the dynamic range; one per advertised codec.
        for (i, capability) in codec_capabilities.into_iter().enumerate() {
            media_engine
                .register_codec(
                    RTCRtpCodecParameters {
                        rtp_codec: capability,
                        payload_type: 96 + i as u8,
                    },
                    RtpCodecKind::Video,
                )
                .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?;
        }

        // Register the audio codec when the session carries audio.
        if let Some(kind) = audio {
            media_engine
                .register_codec(
                    RTCRtpCodecParameters {
                        rtp_codec: kind.capability(),
                        payload_type: kind.payload_type(),
                    },
                    RtpCodecKind::Audio,
                )
                .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?;
        }

        // Register default interceptors (RTCP feedback, NACK, etc.)
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?;

        // Configure ICE settings
        let mut setting_engine = SettingEngineBuilder::new()
            .with_lite(false)
            .with_ice_timeouts(
                Some(Duration::from_secs(5)),
                Some(Duration::from_secs(10)),
                Some(Duration::from_millis(200)),
            );
        if !self.config.nat_ips.is_empty() {
            // Advertise the public address in place of the host's own.
            setting_engine = setting_engine
                .with_nat_1to1_ips(self.config.nat_ips.clone(), RTCIceCandidateType::Host);
        }
        let setting_engine = setting_engine.build();

        // Create ICE servers configuration
        let mut ice_servers = vec![];

        for stun_url in &self.config.stun_servers {
            ice_servers.push(RTCIceServer {
                urls: vec![stun_url.clone()],
                ..Default::default()
            });
        }

        if let Some(turn_url) = &self.config.turn_server {
            ice_servers.push(RTCIceServer {
                urls: vec![turn_url.clone()],
                username: self.config.turn_username.clone().unwrap_or_default(),
                credential: self.config.turn_password.clone().unwrap_or_default(),
            });
        }

        let rtc_config = RTCConfigurationBuilder::new()
            .with_ice_servers(ice_servers)
            .build();

        // Watch channel for peer connection state — lets the handler wait for
        // DTLS completion ("connected" state) before streaming media.
        let (pc_state_tx, pc_state_rx) = tokio::sync::watch::channel(RTCPeerConnectionState::New);

        // Separate watch for ICE-connected, so the handler can split the
        // ICE-connect cost from the DTLS-handshake cost (offer→ice_connected vs
        // ice_connected→connected). Flips `true` once and stays.
        let (ice_connected_tx, ice_connected_rx) = tokio::sync::watch::channel(false);

        // Trickle ICE: the event handler forwards each locally-gathered
        // candidate to the signaling handler, which relays it to the browser.
        // The handler is installed at build time, before
        // `set_local_description` starts gathering, so no early candidate is
        // missed. The browser already trickles its candidates back.
        let (candidate_tx, candidate_rx) =
            tokio::sync::mpsc::unbounded_channel::<RTCIceCandidateInit>();

        let (gathering_tx, gathering_rx) = tokio::sync::watch::channel(false);

        let events = Arc::new(LiveEventHandler {
            monitor_id,
            pc_state_tx,
            ice_connected_tx,
            candidate_tx,
            gathering_tx,
        });

        // Each wildcard binds one UDP socket per non-loopback interface of
        // its family, which is what yields usable host candidates.
        let peer_connection: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(rtc_config)
                .with_media_engine(media_engine)
                .with_interceptor_registry(registry)
                .with_setting_engine(setting_engine)
                .with_handler(events)
                .with_udp_addrs(udp_bind_addrs(self.config.udp_ports))
                .build()
                .await
                .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?,
        );

        Ok(BuiltPeer {
            peer_connection,
            codec_capability,
            pc_state_rx,
            ice_connected_rx,
            candidate_rx,
            gathering_rx,
        })
    }

    /// Store a session and index it by monitor.
    fn register(
        &self,
        monitor_id: u32,
        peer_connection: Arc<dyn PeerConnection>,
        video_track: SendTrack,
        audio_track: Option<SendTrack>,
    ) -> String {
        let session_id = Uuid::new_v4();
        let session = WebRtcLiveSession {
            id: session_id,
            monitor_id,
            state: RwLock::new(WebRtcSessionState::New),
            peer_connection,
            video_track,
            audio_track,
            created_at: chrono::Utc::now(),
            packets_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            remote_candidates: AtomicU64::new(0),
        };
        let session_id_str = session_id.to_string();
        self.sessions
            .insert(session_id_str.clone(), Arc::new(RwLock::new(session)));
        self.monitor_sessions
            .entry(monitor_id)
            .or_default()
            .push(session_id_str.clone());
        info!(
            "Created WebRTC session {} for monitor {}",
            session_id_str, monitor_id
        );
        session_id_str
    }

    /// Create a new WebRTC session for a monitor, as the offerer.
    ///
    /// `profile_level_id` is the camera's H.264 profile, for logging; the
    /// offer advertises browser-universal profiles (see
    /// `h264_offer_profile_level_ids`).
    pub async fn create_session(
        &self,
        monitor_id: u32,
        codec: VideoCodec,
        profile_level_id: Option<&str>,
        audio: Option<AudioTrackKind>,
    ) -> Result<SessionHandshake, WebRtcLiveError> {
        let BuiltPeer {
            peer_connection,
            codec_capability,
            pc_state_rx,
            ice_connected_rx,
            candidate_rx,
            gathering_rx,
        } = self
            .build_peer(monitor_id, codec, profile_level_id, audio)
            .await?;

        let stream_id = format!("zm-live-{}", monitor_id);

        // Video track (sendonly; one-way surveillance stream). The
        // pass-through track packetizes by mime type, so any advertised
        // capability works; the first is used.
        let video_track = SendTrack::attach(
            &peer_connection,
            RtpCodecKind::Video,
            codec_capability,
            "video",
            &stream_id,
        )
        .await?;

        // Audio track (sendonly), when the monitor has audio
        let audio_track = match audio {
            Some(kind) => Some(
                SendTrack::attach(
                    &peer_connection,
                    RtpCodecKind::Audio,
                    kind.capability(),
                    "audio",
                    &stream_id,
                )
                .await?,
            ),
            None => None,
        };

        // Create offer
        let offer = peer_connection
            .create_offer(None)
            .await
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?;

        // Set local description (starts ICE gathering).
        peer_connection
            .set_local_description(offer)
            .await
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?;

        // Trickle ICE: return the offer NOW, with ICE ufrag/pwd, the DTLS
        // fingerprint and the m-lines but possibly no candidates yet, instead
        // of blocking on gathering (which stalled on STUN round-trips).
        // Candidates stream to the browser via `on_ice_candidate`.
        let complete_offer = peer_connection.local_description().await.ok_or_else(|| {
            WebRtcLiveError::WebRtcError("No local description available after set".to_string())
        })?;

        // Advertise trickle support (RFC 8838 §10); webrtc-rs doesn't emit
        // `a=ice-options:trickle`.
        let complete_offer = ensure_trickle_ice_option(complete_offer)?;

        debug!(
            "Trickle offer ready for monitor {} (candidates stream via signaling)",
            monitor_id
        );

        let session_id = self.register(monitor_id, peer_connection, video_track, audio_track);
        Ok(SessionHandshake {
            session_id,
            offer: complete_offer,
            pc_state_rx,
            ice_connected_rx,
            candidate_rx,
            gathering_rx,
        })
    }

    /// Create a session that answers a client's offer (WHEP, RFC 9725).
    ///
    /// The offer is applied first so the send-only tracks bind to the
    /// client's receive slots. Audio is attached only when the offer has an
    /// audio section. The answer is returned once ICE gathering completes
    /// (or `gather_wait` passes), so it carries this side's candidates and a
    /// client that doesn't trickle can still connect. The returned
    /// handshake's `offer` field holds the answer.
    pub async fn answer_offer(
        &self,
        monitor_id: u32,
        codec: VideoCodec,
        profile_level_id: Option<&str>,
        audio: Option<AudioTrackKind>,
        offer_sdp: String,
        gather_wait: Duration,
    ) -> Result<SessionHandshake, WebRtcLiveError> {
        if !offer_sdp.contains("m=video") {
            return Err(WebRtcLiveError::InvalidOffer(
                "the offer has no video section".to_string(),
            ));
        }
        let audio = audio.filter(|_| offer_sdp.contains("m=audio"));
        let offer = RTCSessionDescription::offer(offer_sdp)
            .map_err(|e| WebRtcLiveError::InvalidOffer(e.to_string()))?;
        let BuiltPeer {
            peer_connection,
            codec_capability,
            pc_state_rx,
            ice_connected_rx,
            candidate_rx,
            mut gathering_rx,
        } = self
            .build_peer(monitor_id, codec, profile_level_id, audio)
            .await?;
        let fail = |e: WebRtcLiveError| {
            let pc = Arc::clone(&peer_connection);
            async move {
                let _ = pc.close().await;
                e
            }
        };

        if let Err(e) = peer_connection.set_remote_description(offer).await {
            return Err(fail(WebRtcLiveError::InvalidOffer(e.to_string())).await);
        }
        let stream_id = format!("zm-live-{}", monitor_id);
        let video_track = match SendTrack::attach_to_offer(
            &peer_connection,
            RtpCodecKind::Video,
            codec_capability,
            "video",
            &stream_id,
        )
        .await
        {
            Ok(t) => t,
            // No receive slot in the offer takes the camera's codec.
            Err(e) => {
                return Err(fail(WebRtcLiveError::UnsupportedCodec(format!(
                    "the offer can't receive {}: {e}",
                    codec.as_str()
                )))
                .await)
            }
        };
        let audio_track = match audio {
            Some(kind) => SendTrack::attach_to_offer(
                &peer_connection,
                RtpCodecKind::Audio,
                kind.capability(),
                "audio",
                &stream_id,
            )
            .await
            .ok(),
            None => None,
        };

        let answer = match peer_connection.create_answer(None).await {
            Ok(a) => a,
            Err(e) => return Err(fail(WebRtcLiveError::WebRtcError(e.to_string())).await),
        };
        if let Err(e) = peer_connection.set_local_description(answer).await {
            return Err(fail(WebRtcLiveError::WebRtcError(e.to_string())).await);
        }

        // The answer fixes each sender's payload type. No video codec in
        // common (an H.265 camera, a browser without H.265) ends here.
        if video_track.resolve_payload_type().await.is_err() {
            return Err(fail(WebRtcLiveError::UnsupportedCodec(format!(
                "the offer has no {} the camera's stream can be sent as",
                codec.as_str()
            )))
            .await);
        }
        let audio_track = match audio_track {
            Some(t) if t.resolve_payload_type().await.is_ok() => Some(t),
            _ => None,
        };

        let _ = tokio::time::timeout(gather_wait, gathering_rx.wait_for(|done| *done)).await;
        let answer = match peer_connection.local_description().await {
            Some(a) => a,
            None => {
                return Err(fail(WebRtcLiveError::WebRtcError(
                    "No local description after answering".to_string(),
                ))
                .await)
            }
        };

        let session_id = self.register(monitor_id, peer_connection, video_track, audio_track);
        if let Some(session) = self.get_session(&session_id) {
            session
                .read()
                .await
                .set_state(WebRtcSessionState::Connecting)
                .await;
        }
        Ok(SessionHandshake {
            session_id,
            offer: answer,
            pc_state_rx,
            ice_connected_rx,
            candidate_rx,
            gathering_rx,
        })
    }

    /// Handle answer from client
    pub async fn set_answer(
        &self,
        session_id: &str,
        answer: RTCSessionDescription,
    ) -> Result<(), WebRtcLiveError> {
        let session_lock = self
            .sessions
            .get(session_id)
            .ok_or_else(|| WebRtcLiveError::SessionNotFound(session_id.to_string()))?;

        let session = session_lock.read().await;
        session
            .peer_connection
            .set_remote_description(answer)
            .await
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?;

        // The answer fixes each sender's payload type; samples need it.
        session.video_track.resolve_payload_type().await?;
        if let Some(audio) = &session.audio_track {
            audio.resolve_payload_type().await?;
        }

        // Update state to connecting
        session.set_state(WebRtcSessionState::Connecting).await;

        info!("Set answer for WebRTC session {}", session_id);
        Ok(())
    }

    /// Add an ICE candidate to a session
    pub async fn add_ice_candidate(
        &self,
        session_id: &str,
        candidate: &str,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> Result<(), WebRtcLiveError> {
        let session_lock = self
            .sessions
            .get(session_id)
            .ok_or_else(|| WebRtcLiveError::SessionNotFound(session_id.to_string()))?;

        let session = session_lock.read().await;
        if !candidate_allowed(&session.remote_candidates) {
            return Err(WebRtcLiveError::WebRtcError(format!(
                "more than {MAX_REMOTE_CANDIDATES} ICE candidates for one session"
            )));
        }

        let ice_candidate = RTCIceCandidateInit {
            candidate: candidate.to_string(),
            sdp_mid,
            sdp_mline_index,
            ..Default::default()
        };

        session
            .peer_connection
            .add_ice_candidate(ice_candidate)
            .await
            .map_err(|e| WebRtcLiveError::WebRtcError(e.to_string()))?;

        debug!("Added ICE candidate for WebRTC session {}", session_id);
        Ok(())
    }

    /// Write a complete access unit to a session's video track
    pub async fn write_access_unit(
        &self,
        session_id: &str,
        au: &AssembledAccessUnit,
    ) -> Result<(), WebRtcLiveError> {
        let session_lock = self
            .sessions
            .get(session_id)
            .ok_or_else(|| WebRtcLiveError::SessionNotFound(session_id.to_string()))?;

        let session = session_lock.read().await;
        session.write_access_unit(au).await
    }

    /// Write an audio sample to a session's audio track (no-op when the
    /// session is video-only).
    pub async fn write_audio_sample(
        &self,
        session_id: &str,
        data: &[u8],
        duration: Duration,
    ) -> Result<(), WebRtcLiveError> {
        let session_lock = self
            .sessions
            .get(session_id)
            .ok_or_else(|| WebRtcLiveError::SessionNotFound(session_id.to_string()))?;

        let session = session_lock.read().await;
        session.write_audio_sample(data, duration).await
    }

    /// Get stats for a session
    pub async fn get_session_stats(
        &self,
        session_id: &str,
    ) -> Result<WebRtcSessionStats, WebRtcLiveError> {
        let session_lock = self
            .sessions
            .get(session_id)
            .ok_or_else(|| WebRtcLiveError::SessionNotFound(session_id.to_string()))?;

        let session = session_lock.read().await;
        Ok(session.stats().await)
    }

    /// Get session by ID
    pub fn get_session(&self, session_id: &str) -> Option<Arc<RwLock<WebRtcLiveSession>>> {
        self.sessions.get(session_id).map(|s| s.clone())
    }

    /// Remove a session
    pub async fn remove_session(&self, session_id: &str) -> Result<(), WebRtcLiveError> {
        let session_lock = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| WebRtcLiveError::SessionNotFound(session_id.to_string()))?;

        let session = session_lock.1.read().await;

        // Close peer connection
        let _ = session.peer_connection.close().await;

        // Remove from monitor sessions
        if let Some(mut sessions) = self.monitor_sessions.get_mut(&session.monitor_id) {
            sessions.retain(|id| id != session_id);
        }

        info!("Removed WebRTC session {}", session_id);
        Ok(())
    }

    /// Get all sessions for a monitor
    pub fn get_monitor_sessions(&self, monitor_id: u32) -> Vec<String> {
        self.monitor_sessions
            .get(&monitor_id)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Get session count
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// List all sessions
    pub fn list_sessions(&self) -> Vec<String> {
        self.sessions.iter().map(|r| r.key().clone()).collect()
    }
}

/// The audio track a session carries for a monitor's audio codec, and the
/// transcoder it needs. G.711 passes through (a mandatory WebRTC codec); AAC
/// becomes Opus, since browsers don't take AAC over RTP. A missing libopus
/// leaves the session video-only rather than advertising audio it can't send.
pub fn audio_plan(
    monitor_id: u32,
    codec: Option<crate::streaming::source::AudioCodec>,
) -> (Option<AudioTrackKind>, Option<AacToOpusTranscoder>) {
    use crate::streaming::source::AudioCodec;
    match codec {
        Some(AudioCodec::G711Ulaw) => (Some(AudioTrackKind::Pcmu), None),
        Some(AudioCodec::G711Alaw) => (Some(AudioTrackKind::Pcma), None),
        Some(AudioCodec::Aac) => match AacToOpusTranscoder::new() {
            Ok(t) => (Some(AudioTrackKind::Opus), Some(t)),
            Err(e) => {
                warn!(
                    "Monitor {monitor_id} has AAC audio but transcoding is unavailable ({e}); \
                     video only"
                );
                (None, None)
            }
        },
        Some(other) => {
            debug!(
                "Monitor {monitor_id} audio codec {other:?} not supported over WebRTC; video only"
            );
            (None, None)
        }
        None => (None, None),
    }
}

/// Feeds a monitor's packets into one session: assembles NAL units into
/// frames, sends the startup keyframe, passes G.711 through and transcodes
/// AAC to Opus. The WebSocket and WHEP paths share it.
pub struct MediaPump {
    session_id: String,
    assembler: AccessUnitAssembler,
    transcoder: Option<AacToOpusTranscoder>,
}

impl MediaPump {
    pub fn new(session_id: String, transcoder: Option<AacToOpusTranscoder>) -> Self {
        Self {
            session_id,
            assembler: AccessUnitAssembler::new(),
            transcoder,
        }
    }

    /// Send a keyframe at connect time so the viewer sees a picture at once
    /// instead of waiting a GOP. `false` when there's none or it failed.
    pub async fn inject_keyframe(
        &mut self,
        manager: &WebRtcLiveManager,
        keyframe: Option<&CachedKeyframe>,
    ) -> bool {
        let Some(ck) = keyframe else {
            return false;
        };
        let au = AssembledAccessUnit {
            data: ck.keyframe_au.clone(),
            timestamp_us: ck.timestamp_us,
            is_keyframe: true,
        };
        if let Err(e) = manager.write_access_unit(&self.session_id, &au).await {
            warn!("Failed to inject startup keyframe: {}", e);
            return false;
        }
        self.assembler.clear_needs_keyframe();
        true
    }

    /// Feed one video packet; `true` when it completed a frame that was sent.
    pub async fn video(&mut self, manager: &WebRtcLiveManager, packet: &VideoPacket) -> bool {
        let Some(au) = self.assembler.push(packet) else {
            return false;
        };
        match manager.write_access_unit(&self.session_id, &au).await {
            Ok(()) => true,
            Err(e) => {
                debug!("Failed to write access unit: {}", e);
                false
            }
        }
    }

    /// Feed one audio packet.
    pub async fn audio(&mut self, manager: &WebRtcLiveManager, packet: &AudioPacket) {
        use crate::streaming::source::AudioCodec;
        match packet.codec {
            AudioCodec::G711Alaw | AudioCodec::G711Ulaw => {
                // 8 kHz, one byte per sample.
                let duration = Duration::from_micros(packet.data.len() as u64 * 1_000_000 / 8_000);
                if let Err(e) = manager
                    .write_audio_sample(&self.session_id, &packet.data, duration)
                    .await
                {
                    debug!("Failed to write audio sample: {}", e);
                }
            }
            AudioCodec::Aac => {
                let Some(transcoder) = self.transcoder.as_mut() else {
                    return;
                };
                match transcoder.transcode(&packet.data) {
                    Ok(frames) => {
                        for frame in frames {
                            if let Err(e) = manager
                                .write_audio_sample(&self.session_id, &frame.data, frame.duration)
                                .await
                            {
                                debug!("Failed to write Opus frame: {}", e);
                            }
                        }
                    }
                    Err(e) => debug!(
                        "AAC→Opus transcode error for session {}: {}",
                        self.session_id, e
                    ),
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_or_candidate_stuffed_sdp_is_refused() {
        let line = "a=candidate:1 1 UDP 2122252543 192.0.2.1 50000 typ host\r\n";
        let ok = format!("v=0\r\n{}", line.repeat(8));
        assert!(check_remote_sdp(&ok).is_ok());
        let stuffed = format!("v=0\r\n{}", line.repeat(MAX_REMOTE_CANDIDATES as usize + 1));
        assert!(check_remote_sdp(&stuffed).is_err());
        let huge = format!("v=0\r\na=x:{}\r\n", "y".repeat(MAX_SDP_BYTES));
        assert!(check_remote_sdp(&huge).is_err());
    }

    #[test]
    fn a_session_takes_only_so_many_ice_candidates() {
        let sent = AtomicU64::new(0);
        for _ in 0..MAX_REMOTE_CANDIDATES {
            assert!(candidate_allowed(&sent));
        }
        assert!(!candidate_allowed(&sent));
        assert!(!candidate_allowed(&sent));
    }

    #[test]
    fn test_webrtc_live_config_default() {
        let config = WebRtcLiveConfig::default();
        assert_eq!(config.stun_servers.len(), 2);
        assert!(config.turn_server.is_none());
        assert_eq!(config.max_sessions, 100);
    }

    #[test]
    fn from_webrtc_config_honors_empty_stun_list() {
        // REVIEW_FIXES_PLAN §2.2: an empty STUN list must pass through verbatim
        // (LAN-only deployments rely on this to skip the STUN gather stall).
        let cfg = crate::configure::streaming::WebRtcConfig {
            stun_servers: vec![],
            max_connections: 7,
            turn: None,
            ..Default::default()
        };
        let live = WebRtcLiveConfig::from_webrtc_config(&cfg);
        assert!(live.stun_servers.is_empty());
        assert!(live.turn_server.is_none());
        assert_eq!(live.max_sessions, 7);
    }

    #[test]
    fn from_webrtc_config_maps_stun_and_enabled_turn() {
        let cfg = crate::configure::streaming::WebRtcConfig {
            stun_servers: vec!["stun:lan.local:3478".to_string()],
            turn: Some(crate::configure::streaming::TurnConfig {
                enabled: true,
                server: "turn:lan.local:3478".to_string(),
                username: "u".to_string(),
                password: "p".to_string(),
            }),
            ..Default::default()
        };
        let live = WebRtcLiveConfig::from_webrtc_config(&cfg);
        assert_eq!(live.stun_servers, vec!["stun:lan.local:3478".to_string()]);
        assert_eq!(live.turn_server.as_deref(), Some("turn:lan.local:3478"));
        assert_eq!(live.turn_username.as_deref(), Some("u"));
        assert_eq!(live.turn_password.as_deref(), Some("p"));
    }

    #[test]
    fn from_webrtc_config_ignores_disabled_turn() {
        // A present-but-disabled TURN block must not be advertised.
        let cfg = crate::configure::streaming::WebRtcConfig {
            turn: Some(crate::configure::streaming::TurnConfig {
                enabled: false,
                server: "turn:nope:3478".to_string(),
                username: "u".to_string(),
                password: "p".to_string(),
            }),
            ..Default::default()
        };
        let live = WebRtcLiveConfig::from_webrtc_config(&cfg);
        assert!(live.turn_server.is_none());
        assert!(live.turn_username.is_none());
        assert!(live.turn_password.is_none());
    }

    #[test]
    fn test_session_state_serialize() {
        let state = WebRtcSessionState::Connected;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"connected\"");
    }

    #[test]
    fn test_manager_creation() {
        let config = WebRtcLiveConfig::default();
        let manager = WebRtcLiveManager::new(config);
        assert_eq!(manager.session_count(), 0);
    }

    /// Manager with no STUN servers: ICE gathering completes instantly with
    /// host candidates only, so session-creation tests run offline.
    fn lan_only_manager() -> WebRtcLiveManager {
        WebRtcLiveManager::new(WebRtcLiveConfig {
            stun_servers: vec![],
            ..WebRtcLiveConfig::default()
        })
    }

    #[tokio::test]
    async fn test_session_with_audio_offers_audio_m_line() {
        let manager = lan_only_manager();
        let h = manager
            .create_session(1, VideoCodec::H264, None, Some(AudioTrackKind::Opus))
            .await
            .expect("session with audio");
        assert!(h.offer.sdp.contains("m=video"), "video m-line expected");
        assert!(h.offer.sdp.contains("m=audio"), "audio m-line expected");
        assert!(h.offer.sdp.contains("opus"), "opus codec expected in SDP");
    }

    #[tokio::test]
    async fn test_session_with_g711_offers_pcma() {
        let manager = lan_only_manager();
        let h = manager
            .create_session(1, VideoCodec::H264, None, Some(AudioTrackKind::Pcma))
            .await
            .expect("session with G.711 audio");
        assert!(h.offer.sdp.contains("m=audio"));
        assert!(h.offer.sdp.contains("PCMA"), "PCMA codec expected in SDP");
    }

    #[tokio::test]
    async fn test_video_only_session_has_no_audio_m_line() {
        let manager = lan_only_manager();
        let h = manager
            .create_session(1, VideoCodec::H264, None, None)
            .await
            .expect("video-only session");
        assert!(h.offer.sdp.contains("m=video"));
        assert!(
            !h.offer.sdp.contains("m=audio"),
            "video-only offer must not advertise audio"
        );
    }

    #[tokio::test]
    async fn test_h265_session_offers_h265_with_fmtp() {
        let manager = lan_only_manager();
        let h = manager
            .create_session(1, VideoCodec::H265, None, None)
            .await
            .expect("H.265 session");
        assert!(h.offer.sdp.contains("H265"), "H265 codec expected in SDP");
        assert!(
            h.offer.sdp.contains("profile-id=1"),
            "RFC 7798 fmtp expected in SDP"
        );
    }

    /// Trickle ICE: the offer is returned immediately with ICE ufrag + DTLS
    /// fingerprint (so the browser can answer at once) and host candidates are
    /// streamed on the candidate channel rather than embedded after a gather
    /// wait. This is what collapses `offer_ms` to its floor.
    #[tokio::test]
    async fn test_trickle_offer_is_immediate_and_streams_candidates() {
        let manager = lan_only_manager();
        let mut h = manager
            .create_session(1, VideoCodec::H264, None, None)
            .await
            .expect("session");
        // The offer carries everything the browser needs to answer without
        // waiting for candidates.
        assert!(h.offer.sdp.contains("a=ice-ufrag"), "offer has ICE ufrag");
        assert!(
            h.offer.sdp.contains("a=fingerprint"),
            "offer has DTLS fingerprint"
        );
        assert!(h.offer.sdp.contains("m=video"), "offer has video m-line");
        assert!(
            h.offer.sdp.contains("a=ice-options:trickle"),
            "offer signals trickle ICE (RFC 8838)"
        );

        // At least one host candidate trickles out within a short window.
        let got = tokio::time::timeout(Duration::from_secs(3), h.candidate_rx.recv())
            .await
            .expect("a candidate should trickle out within 3s");
        let cand = got.expect("candidate channel open");
        assert!(
            cand.candidate.contains("candidate:"),
            "trickled value is an ICE candidate line; got: {}",
            cand.candidate
        );
    }

    /// A receive-only client, as a browser behind WHEP: offers to receive
    /// video (H.264 and H.265 when `h265`), counts RTP packets that arrive.
    struct Viewer {
        pc: Arc<dyn PeerConnection>,
        connected: tokio::sync::watch::Receiver<bool>,
        packets: Arc<AtomicU64>,
    }

    struct ViewerEvents {
        connected: tokio::sync::watch::Sender<bool>,
        gathered: tokio::sync::watch::Sender<bool>,
        packets: Arc<AtomicU64>,
    }

    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for ViewerEvents {
        async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
            if state == RTCIceGatheringState::Complete {
                let _ = self.gathered.send(true);
            }
        }
        async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
            if state == RTCPeerConnectionState::Connected {
                let _ = self.connected.send(true);
            }
        }
        async fn on_track(&self, track: Arc<dyn webrtc::media_stream::track_remote::TrackRemote>) {
            let packets = self.packets.clone();
            tokio::spawn(async move {
                use webrtc::media_stream::track_remote::TrackRemoteEvent;
                while let Some(event) = track.poll().await {
                    match event {
                        TrackRemoteEvent::OnRtpPacket(_) => {
                            packets.fetch_add(1, Ordering::SeqCst);
                        }
                        TrackRemoteEvent::OnEnded => break,
                        _ => {}
                    }
                }
            });
        }
    }

    /// A viewer and its offer (gathered, so it needs no trickle).
    async fn viewer(codecs: &[(&str, &str)]) -> (Viewer, String) {
        let mut media = MediaEngine::default();
        for (i, (mime, fmtp)) in codecs.iter().enumerate() {
            media
                .register_codec(
                    RTCRtpCodecParameters {
                        rtp_codec: RTCRtpCodec {
                            mime_type: mime.to_string(),
                            clock_rate: 90000,
                            channels: 0,
                            sdp_fmtp_line: fmtp.to_string(),
                            rtcp_feedback: vec![],
                        },
                        payload_type: 102 + i as u8,
                    },
                    RtpCodecKind::Video,
                )
                .unwrap();
        }
        let (connected_tx, connected) = tokio::sync::watch::channel(false);
        let (gathered_tx, mut gathered) = tokio::sync::watch::channel(false);
        let packets = Arc::new(AtomicU64::new(0));
        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_media_engine(media)
                .with_handler(Arc::new(ViewerEvents {
                    connected: connected_tx,
                    gathered: gathered_tx,
                    packets: packets.clone(),
                }))
                .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
                .build()
                .await
                .unwrap(),
        );
        pc.add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let offer = pc.create_offer(None).await.unwrap();
        pc.set_local_description(offer).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), gathered.wait_for(|g| *g)).await;
        let sdp = pc.local_description().await.unwrap().sdp;
        (
            Viewer {
                pc,
                connected,
                packets,
            },
            sdp,
        )
    }

    const H264: (&str, &str) = (
        "video/H264",
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
    );

    /// WHEP: a receive-only offer is answered with a send-only video
    /// section and this side's candidates; the viewer connects and gets RTP.
    #[tokio::test]
    async fn an_offer_is_answered_and_the_viewer_receives_video() {
        let manager = lan_only_manager();
        let (mut viewer, offer) = viewer(&[H264]).await;
        let h = manager
            .answer_offer(
                1,
                VideoCodec::H264,
                None,
                None,
                offer,
                Duration::from_secs(3),
            )
            .await
            .expect("answer");
        let answer = h.offer.sdp.clone();
        assert!(answer.contains("a=sendonly"), "{answer}");
        assert!(answer.contains("H264/90000"), "{answer}");
        assert!(
            answer.contains("a=candidate"),
            "answer carries candidates: {answer}"
        );
        viewer
            .pc
            .set_remote_description(RTCSessionDescription::answer(answer).unwrap())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), viewer.connected.wait_for(|c| *c))
            .await
            .expect("viewer connects within 10 s")
            .unwrap();
        let au = AssembledAccessUnit {
            data: vec![
                0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
                0x84,
            ],
            timestamp_us: 0,
            is_keyframe: true,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while viewer.packets.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "no RTP reached the viewer");
            let _ = manager.write_access_unit(&h.session_id, &au).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        manager.remove_session(&h.session_id).await.unwrap();
        let _ = viewer.pc.close().await;
    }

    #[tokio::test]
    async fn an_offer_without_the_cameras_codec_is_refused() {
        let manager = lan_only_manager();
        let (viewer, offer) = viewer(&[H264]).await;
        let err = manager
            .answer_offer(
                1,
                VideoCodec::H265,
                None,
                None,
                offer,
                Duration::from_secs(1),
            )
            .await
            .err()
            .expect("an H.265 camera can't answer an H.264-only offer");
        assert!(matches!(err, WebRtcLiveError::UnsupportedCodec(_)), "{err}");
        assert_eq!(manager.session_count(), 0);
        let _ = viewer.pc.close().await;
    }

    #[tokio::test]
    async fn an_offer_without_video_is_refused() {
        let manager = lan_only_manager();
        let err = manager
            .answer_offer(
                1,
                VideoCodec::H264,
                None,
                None,
                "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n".to_string(),
                Duration::from_secs(1),
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(err, WebRtcLiveError::InvalidOffer(_)), "{err}");
    }

    #[test]
    fn udp_bind_addrs_always_binds_ipv4_and_ipv6_only_when_available() {
        let addrs = udp_bind_addrs(None);
        assert_eq!(addrs[0], "0.0.0.0:0");
        let ipv6 = std::net::UdpSocket::bind("[::]:0").is_ok();
        assert_eq!(addrs.contains(&"[::]:0".to_string()), ipv6);
    }

    // --- AccessUnitAssembler tests ---

    /// Build a single-NAL `VideoPacket`. For VCL slice NAL types a slice-header
    /// byte with `first_mb_in_slice == 0` (MSB set) is inserted right after the
    /// NAL header, so the packet reads as the *start* of a coded picture. Use
    /// `make_continuation_slice` for slices that continue a multi-slice picture.
    fn make_nal(nal_type: u8, extra: &[u8]) -> VideoPacket {
        let raw = nal_type & 0x1F;
        let is_keyframe = raw == 5; // IDR
        let mut data = vec![0x00, 0x00, 0x00, 0x01, nal_type];
        if (1..=5).contains(&raw) {
            // first_mb_in_slice = 0 → Exp-Golomb single `1` bit (MSB set).
            data.push(0x80);
        }
        data.extend_from_slice(extra);
        VideoPacket {
            monitor_id: 1,
            timestamp_us: 0,
            data,
            is_keyframe,
            codec: VideoCodec::H264,
        }
    }

    /// Build a continuation slice of a multi-slice picture: a VCL NAL whose
    /// `first_mb_in_slice` is greater than 0 (MSB of the slice header clear).
    fn make_continuation_slice(nal_type: u8) -> VideoPacket {
        let raw = nal_type & 0x1F;
        let is_keyframe = raw == 5;
        // 0x00 slice-header byte → leading Exp-Golomb bit clear → MB index > 0.
        let data = vec![0x00, 0x00, 0x00, 0x01, nal_type, 0x00, 0xAA];
        VideoPacket {
            monitor_id: 1,
            timestamp_us: 0,
            data,
            is_keyframe,
            codec: VideoCodec::H264,
        }
    }

    /// Build a slice NAL whose slice header opens with `first_mb_in_slice`,
    /// Exp-Golomb (`ue(v)`) encoded as a real encoder would — so continuation
    /// slices carry the large macroblock indices a 4K picture produces.
    fn make_slice(nal_type: u8, first_mb_in_slice: u32) -> VideoPacket {
        let code = first_mb_in_slice as u64 + 1;
        let significant = 64 - code.leading_zeros();
        let leading_zeros = significant - 1;
        let mut bits: Vec<bool> = Vec::new();
        bits.extend(std::iter::repeat_n(false, leading_zeros as usize));
        for i in (0..significant).rev() {
            bits.push((code >> i) & 1 == 1);
        }
        while !bits.len().is_multiple_of(8) {
            bits.push(false);
        }
        let mut data = vec![0x00, 0x00, 0x00, 0x01, nal_type];
        for chunk in bits.chunks(8) {
            let mut byte = 0u8;
            for (i, &bit) in chunk.iter().enumerate() {
                if bit {
                    byte |= 1 << (7 - i);
                }
            }
            data.push(byte);
        }
        VideoPacket {
            monitor_id: 1,
            timestamp_us: 0,
            data,
            is_keyframe: (nal_type & 0x1F) == 5,
            codec: VideoCodec::H264,
        }
    }

    #[test]
    fn test_au_assembler_4k_many_slices_single_au() {
        // A 3840×2160 picture is 240×135 = 32400 macroblocks. Real 4K cameras
        // split it into many slices; only the first has first_mb_in_slice == 0.
        // Every continuation slice must stay in the SAME access unit so the
        // RTP packetizer gives the whole picture one timestamp + marker bit.
        const SLICES: u32 = 24;
        let mut asm = AccessUnitAssembler::new();

        asm.push(&make_nal(0x67, &[0x64, 0x00, 0x33])); // SPS
        asm.push(&make_nal(0x68, &[0xCE, 0x3C, 0x80])); // PPS

        // First IDR slice starts the picture; the rest continue it.
        assert!(asm.push(&make_slice(0x65, 0)).is_none());
        for s in 1..SLICES {
            assert!(
                asm.push(&make_slice(0x65, s * 1350)).is_none(),
                "continuation slice {s} must not flush the access unit"
            );
        }

        // The next picture's first slice flushes the 24-slice keyframe AU.
        let au = asm
            .push(&make_slice(0x41, 0))
            .expect("multi-slice keyframe AU expected");
        assert!(au.is_keyframe);
        // SPS + PPS + 24 IDR slices = 26 NAL start codes, all in one AU.
        let starts = au.data.windows(4).filter(|w| w == &[0, 0, 0, 1]).count();
        assert_eq!(
            starts, 26,
            "the whole 4K picture must emit as a single access unit"
        );
    }

    #[test]
    fn test_au_assembler_sps_pps_idr_grouped() {
        let mut asm = AccessUnitAssembler::new();

        // SPS (type 7) — non-VCL, buffered
        assert!(asm.push(&make_nal(0x67, &[0x4d, 0x00, 0x33])).is_none());
        // PPS (type 8) — non-VCL, buffered
        assert!(asm.push(&make_nal(0x68, &[0xCE, 0x3C, 0x80])).is_none());
        // IDR (type 5) — first VCL, buffered (no previous AU to flush)
        assert!(asm.push(&make_nal(0x65, &[0x88, 0x84])).is_none());

        // Next P-frame (type 1) triggers flush of the IDR AU
        let au = asm.push(&make_nal(0x41, &[0x9A, 0x21]));
        assert!(au.is_some());
        let au = au.unwrap();
        assert!(au.is_keyframe);
        // AU should contain SPS + PPS + IDR (3 start codes)
        let starts: Vec<_> = au
            .data
            .windows(4)
            .enumerate()
            .filter(|(_, w)| w == &[0, 0, 0, 1])
            .map(|(i, _)| i)
            .collect();
        assert_eq!(starts.len(), 3, "Expected 3 NAL units in keyframe AU");
    }

    #[test]
    fn test_au_assembler_p_frames_after_keyframe() {
        let mut asm = AccessUnitAssembler::new();

        // Prime with keyframe to clear needs_keyframe
        assert!(asm.push(&make_nal(0x65, &[0x88])).is_none()); // IDR buffered
        let au = asm.push(&make_nal(0x41, &[0x01])); // P flushes IDR
        assert!(au.is_some());
        assert!(au.unwrap().is_keyframe);

        // Now P-frames should emit normally
        let au = asm.push(&make_nal(0x41, &[0x02])); // flushes P1
        assert!(au.is_some());
        assert!(!au.unwrap().is_keyframe);

        let au = asm.push(&make_nal(0x41, &[0x03])); // flushes P2
        assert!(au.is_some());
        assert!(!au.unwrap().is_keyframe);
    }

    #[test]
    fn test_au_assembler_drops_until_keyframe() {
        let mut asm = AccessUnitAssembler::new();

        // P-frames before any keyframe should be dropped
        assert!(asm.push(&make_nal(0x41, &[0x01])).is_none()); // buffered
        assert!(asm.push(&make_nal(0x41, &[0x02])).is_none()); // flush P1 → dropped
        assert!(asm.push(&make_nal(0x41, &[0x03])).is_none()); // flush P2 → dropped

        // SPS arrives → flushes P3 (dropped), starts fresh AU
        assert!(asm.push(&make_nal(0x67, &[0x4d, 0x00, 0x33])).is_none());
        // PPS appended
        assert!(asm.push(&make_nal(0x68, &[0xCE, 0x3C, 0x80])).is_none());
        // IDR appended (first VCL in this AU)
        assert!(asm.push(&make_nal(0x65, &[0x88, 0x84])).is_none());

        // Next P triggers flush of keyframe AU [SPS+PPS+IDR]
        let au = asm.push(&make_nal(0x41, &[0x04]));
        assert!(au.is_some());
        let au = au.unwrap();
        assert!(au.is_keyframe);

        // Now P-frames flow normally
        let au = asm.push(&make_nal(0x41, &[0x05]));
        assert!(au.is_some());
        assert!(!au.unwrap().is_keyframe);
    }

    #[test]
    fn test_au_assembler_sps_boundary_separates_from_p_frame() {
        let mut asm = AccessUnitAssembler::new();

        // Prime with keyframe to clear needs_keyframe
        assert!(asm.push(&make_nal(0x65, &[0x88])).is_none());
        let au = asm.push(&make_nal(0x41, &[0x01]));
        assert!(au.unwrap().is_keyframe);

        // P2 flushes P1 (needs_keyframe is now cleared, so P-frames emit)
        let au = asm.push(&make_nal(0x41, &[0x02]));
        assert!(au.is_some());
        assert!(!au.unwrap().is_keyframe);

        // P3 buffered (P2 was just started in the buffer)
        // SPS arrives while P3 is buffered → flushes P3, starts fresh AU
        let au = asm.push(&make_nal(0x67, &[0x4d, 0x00, 0x33]));
        assert!(au.is_some());
        assert!(!au.unwrap().is_keyframe); // flushed AU was P2

        // PPS appended to SPS
        assert!(asm.push(&make_nal(0x68, &[0xCE, 0x3C, 0x80])).is_none());
        // IDR appended
        assert!(asm.push(&make_nal(0x65, &[0x88, 0x84])).is_none());

        // Next P flushes [SPS+PPS+IDR]
        let au = asm.push(&make_nal(0x41, &[0x03]));
        assert!(au.is_some());
        let au = au.unwrap();
        assert!(au.is_keyframe);
        // Verify 3 NALs in the keyframe AU
        let starts: Vec<_> = au
            .data
            .windows(4)
            .enumerate()
            .filter(|(_, w)| w == &[0, 0, 0, 1])
            .map(|(i, _)| i)
            .collect();
        assert_eq!(starts.len(), 3, "Expected SPS+PPS+IDR in keyframe AU");
    }

    #[test]
    fn test_au_assembler_multi_slice_keyframe_single_au() {
        // A 4K keyframe split across three slice NALs must emit as ONE AU so
        // the RTP packetizer gives the whole picture one timestamp + marker.
        let mut asm = AccessUnitAssembler::new();

        asm.push(&make_nal(0x67, &[0x4d, 0x00, 0x33])); // SPS
        asm.push(&make_nal(0x68, &[0xCE, 0x3C, 0x80])); // PPS
                                                        // First IDR slice (first_mb_in_slice == 0) starts the picture.
        assert!(asm.push(&make_nal(0x65, &[0x88])).is_none());
        // Two continuation IDR slices of the SAME picture — must NOT flush.
        assert!(asm.push(&make_continuation_slice(0x65)).is_none());
        assert!(asm.push(&make_continuation_slice(0x65)).is_none());

        // The next picture's first slice flushes the multi-slice keyframe AU.
        let au = asm.push(&make_nal(0x41, &[0x9A])).expect("AU expected");
        assert!(au.is_keyframe);
        // AU must contain SPS + PPS + 3 IDR slices = 5 NAL start codes.
        let starts = au.data.windows(4).filter(|w| w == &[0, 0, 0, 1]).count();
        assert_eq!(starts, 5, "multi-slice keyframe AU must hold all 5 NALs");
    }

    #[test]
    fn test_au_assembler_multi_slice_p_frame_single_au() {
        let mut asm = AccessUnitAssembler::new();

        // Prime with a keyframe to clear needs_keyframe.
        asm.push(&make_nal(0x65, &[0x88]));
        let au = asm.push(&make_nal(0x41, &[0x01])).expect("keyframe AU");
        assert!(au.is_keyframe);

        // The P slice just pushed is the current AU; add two continuation
        // slices of the same multi-slice P-frame.
        assert!(asm.push(&make_continuation_slice(0x41)).is_none());
        assert!(asm.push(&make_continuation_slice(0x41)).is_none());

        // The next picture's first slice flushes the 3-slice P-frame as one AU.
        let au = asm.push(&make_nal(0x41, &[0x02])).expect("P-frame AU");
        assert!(!au.is_keyframe);
        let starts = au.data.windows(4).filter(|w| w == &[0, 0, 0, 1]).count();
        assert_eq!(starts, 3, "multi-slice P-frame AU must hold all 3 slices");
    }

    #[test]
    fn test_au_assembler_multi_slice_keyframe_flag_from_first_slice() {
        // An AU is a keyframe when its first slice is an IDR slice, even when
        // later continuation slices are present.
        let mut asm = AccessUnitAssembler::new();
        asm.push(&make_nal(0x67, &[0x4d, 0x00, 0x33]));
        asm.push(&make_nal(0x68, &[0xCE]));
        asm.push(&make_nal(0x65, &[0x88])); // first IDR slice
        asm.push(&make_continuation_slice(0x65)); // continuation IDR slice
        let au = asm.push(&make_nal(0x41, &[0x9A])).expect("AU");
        assert!(au.is_keyframe);
    }

    #[test]
    fn test_au_assembler_aud_delimits_access_unit() {
        // An Access Unit Delimiter (NAL type 9) flushes the buffered picture.
        let mut asm = AccessUnitAssembler::new();
        asm.push(&make_nal(0x65, &[0x88])); // IDR slice buffered
        let au = asm
            .push(&make_nal(0x09, &[0xF0]))
            .expect("AUD flushes keyframe AU");
        assert!(au.is_keyframe);
    }

    #[test]
    fn test_au_assembler_empty_packet_ignored() {
        let mut asm = AccessUnitAssembler::new();
        let empty = VideoPacket {
            monitor_id: 1,
            timestamp_us: 0,
            data: vec![],
            is_keyframe: false,
            codec: VideoCodec::H264,
        };
        assert!(asm.push(&empty).is_none());
    }

    #[test]
    fn test_h264_nal_type_parsing() {
        // SPS
        assert_eq!(h264_nal_type(&[0, 0, 0, 1, 0x67]), Some(7));
        // PPS
        assert_eq!(h264_nal_type(&[0, 0, 0, 1, 0x68]), Some(8));
        // IDR
        assert_eq!(h264_nal_type(&[0, 0, 0, 1, 0x65]), Some(5));
        // Non-IDR slice
        assert_eq!(h264_nal_type(&[0, 0, 0, 1, 0x41]), Some(1));
        // 3-byte start code
        assert_eq!(h264_nal_type(&[0, 0, 1, 0x67]), Some(7));
        // No start code
        assert_eq!(h264_nal_type(&[0xFF, 0x67]), None);
    }

    #[test]
    fn test_extract_profile_level_id() {
        // Main Profile, Level 5.1: profile_idc=0x4D, constraints=0x00, level_idc=0x33
        let sps = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x4D, 0x00, 0x33, 0xFF];
        assert_eq!(extract_profile_level_id(&sps), Some("4d0033".to_string()));

        // Constrained Baseline, Level 3.1
        let sps = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xE0, 0x1F];
        assert_eq!(extract_profile_level_id(&sps), Some("42e01f".to_string()));

        // High Profile, Level 4.0
        let sps = vec![0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x28, 0xAA];
        assert_eq!(extract_profile_level_id(&sps), Some("640028".to_string()));

        // Not an SPS (PPS type 8)
        let pps = vec![0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80];
        assert_eq!(extract_profile_level_id(&pps), None);

        // Too short
        let short = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x4D];
        assert_eq!(extract_profile_level_id(&short), None);

        // No start code
        let no_start = vec![0x67, 0x4D, 0x00, 0x33];
        assert_eq!(extract_profile_level_id(&no_start), None);
    }

    #[test]
    fn test_h264_offer_profile_level_ids() {
        // The offer advertises browser-universal pass-through profiles,
        // independent of the camera's native profile. Safari accepts only
        // Constrained Baseline (42e01f) and Constrained High (640c1f); both
        // are advertised so Safari can answer instead of rejecting the
        // m-line. Chrome accepts them too.
        assert_eq!(
            h264_offer_profile_level_ids(),
            vec!["42e01f".to_string(), "640c1f".to_string()]
        );

        // Constrained Baseline must always be offered — it is the one profile
        // every WebRTC implementation is guaranteed to support.
        assert!(h264_offer_profile_level_ids().contains(&"42e01f".to_string()));

        // The advertised levels are the conservative 3.1 (`…1f`); the real
        // 4K bitstream stays at its native level via level-asymmetry-allowed.
        for plid in h264_offer_profile_level_ids() {
            assert!(
                plid.ends_with("1f"),
                "offered profile must advertise level 3.1: {plid}"
            );
        }
    }

    #[test]
    fn test_clear_needs_keyframe() {
        let mut asm = AccessUnitAssembler::new();

        // P-frames should be dropped before any keyframe
        assert!(asm.push(&make_nal(0x41, &[0x01])).is_none()); // buffered
        assert!(asm.push(&make_nal(0x41, &[0x02])).is_none()); // flush P1 → dropped

        // Simulate external keyframe injection — clear needs_keyframe
        asm.clear_needs_keyframe();

        // Now P-frames should flow through (flushing the buffered P2)
        let au = asm.push(&make_nal(0x41, &[0x03])); // flush P2
        assert!(au.is_some());
        assert!(!au.unwrap().is_keyframe);
    }

    // --- AccessUnitAssembler H.265 tests ---

    /// Build a single-NAL H.265 `VideoPacket`. The two-byte HEVC NAL header is
    /// `(type << 1)` followed by `0x01` (layer 0, temporal_id_plus1 = 1). For
    /// VCL NAL types (0–31) a slice-segment-header byte with
    /// `first_slice_segment_in_pic_flag` set (MSB) is inserted, so the packet
    /// reads as the *start* of a coded picture.
    fn make_h265_nal(nal_type: u8, extra: &[u8]) -> VideoPacket {
        let mut data = vec![0x00, 0x00, 0x00, 0x01, nal_type << 1, 0x01];
        if nal_type <= 31 {
            data.push(0x80);
        }
        data.extend_from_slice(extra);
        VideoPacket {
            monitor_id: 1,
            timestamp_us: 0,
            data,
            is_keyframe: (16..=21).contains(&nal_type), // IRAP range
            codec: VideoCodec::H265,
        }
    }

    /// Build a continuation slice segment of a multi-slice H.265 picture:
    /// `first_slice_segment_in_pic_flag` clear (MSB of the slice header 0).
    fn make_h265_continuation_slice(nal_type: u8) -> VideoPacket {
        let data = vec![0x00, 0x00, 0x00, 0x01, nal_type << 1, 0x01, 0x00, 0xAA];
        VideoPacket {
            monitor_id: 1,
            timestamp_us: 0,
            data,
            is_keyframe: (16..=21).contains(&nal_type),
            codec: VideoCodec::H265,
        }
    }

    #[test]
    fn test_h265_au_assembler_vps_sps_pps_idr_grouped() {
        let mut asm = AccessUnitAssembler::new();

        // VPS (32), SPS (33), PPS (34) — non-VCL, buffered
        assert!(asm.push(&make_h265_nal(32, &[0x0C, 0x01])).is_none());
        assert!(asm.push(&make_h265_nal(33, &[0x01, 0x01])).is_none());
        assert!(asm.push(&make_h265_nal(34, &[0xC1, 0x62])).is_none());
        // IDR_W_RADL (19) — first VCL, buffered
        assert!(asm.push(&make_h265_nal(19, &[0x88])).is_none());

        // Next picture (TRAIL_R, type 1) flushes the keyframe AU
        let au = asm
            .push(&make_h265_nal(1, &[0x9A]))
            .expect("keyframe AU expected");
        assert!(au.is_keyframe);
        // VPS + SPS + PPS + IDR = 4 NAL start codes in one AU
        let starts = au.data.windows(4).filter(|w| w == &[0, 0, 0, 1]).count();
        assert_eq!(starts, 4, "expected VPS+SPS+PPS+IDR in keyframe AU");
    }

    #[test]
    fn test_h265_au_assembler_4k_multi_slice_single_au() {
        // Multi-slice H.265 keyframe: only the first slice segment carries
        // first_slice_segment_in_pic_flag == 1; continuation segments must
        // stay in the SAME access unit.
        let mut asm = AccessUnitAssembler::new();

        asm.push(&make_h265_nal(32, &[0x0C])); // VPS
        asm.push(&make_h265_nal(33, &[0x01])); // SPS
        asm.push(&make_h265_nal(34, &[0xC1])); // PPS

        assert!(asm.push(&make_h265_nal(19, &[0x88])).is_none());
        for s in 1..24 {
            assert!(
                asm.push(&make_h265_continuation_slice(19)).is_none(),
                "continuation slice {s} must not flush the access unit"
            );
        }

        // The next picture's first slice flushes the 24-slice keyframe AU.
        let au = asm
            .push(&make_h265_nal(1, &[0x9A]))
            .expect("multi-slice keyframe AU expected");
        assert!(au.is_keyframe);
        // VPS + SPS + PPS + 24 IDR slices = 27 NAL start codes, one AU.
        let starts = au.data.windows(4).filter(|w| w == &[0, 0, 0, 1]).count();
        assert_eq!(
            starts, 27,
            "the whole 4K H.265 picture must emit as a single access unit"
        );
    }

    #[test]
    fn test_h265_au_assembler_drops_until_keyframe() {
        let mut asm = AccessUnitAssembler::new();

        // TRAIL_R pictures before any keyframe are dropped
        assert!(asm.push(&make_h265_nal(1, &[0x01])).is_none()); // buffered
        assert!(asm.push(&make_h265_nal(1, &[0x02])).is_none()); // flush → dropped

        // Parameter sets flush the buffered P (dropped) and start a fresh AU
        assert!(asm.push(&make_h265_nal(32, &[0x0C])).is_none()); // VPS
        assert!(asm.push(&make_h265_nal(33, &[0x01])).is_none()); // SPS
        assert!(asm.push(&make_h265_nal(34, &[0xC1])).is_none()); // PPS
        assert!(asm.push(&make_h265_nal(19, &[0x88])).is_none()); // IDR

        // Next picture flushes [VPS+SPS+PPS+IDR]; keyframe passes the gate
        let au = asm.push(&make_h265_nal(1, &[0x03])).expect("keyframe AU");
        assert!(au.is_keyframe);

        // Subsequent P-frames flow normally
        let au = asm.push(&make_h265_nal(1, &[0x04])).expect("P-frame AU");
        assert!(!au.is_keyframe);
    }

    #[test]
    fn test_h265_au_assembler_cra_is_keyframe() {
        // CRA (type 21) is an IRAP picture — must pass the keyframe gate.
        let mut asm = AccessUnitAssembler::new();
        assert!(asm.push(&make_h265_nal(21, &[0x88])).is_none());
        let au = asm.push(&make_h265_nal(1, &[0x01])).expect("CRA AU");
        assert!(au.is_keyframe);
    }

    #[test]
    fn test_h265_aud_delimits_access_unit() {
        // An Access Unit Delimiter (type 35) flushes the buffered picture.
        let mut asm = AccessUnitAssembler::new();
        asm.push(&make_h265_nal(19, &[0x88])); // IDR slice buffered
        let au = asm
            .push(&make_h265_nal(35, &[0x50]))
            .expect("AUD flushes keyframe AU");
        assert!(au.is_keyframe);
    }
}
