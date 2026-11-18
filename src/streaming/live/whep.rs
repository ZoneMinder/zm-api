//! WHEP (WebRTC-HTTP Egress Protocol, RFC 9725): a viewer POSTs an SDP
//! offer, gets an SDP answer and a session URL, and plays. Home Assistant's
//! camera WebRTC works this way (the browser makes the offer), so it can show
//! zm-api's WebRTC without go2rtc in between.
//!
//! Each session answers with this side's candidates included, so a viewer
//! that doesn't trickle still connects; one that does can `PATCH` its
//! candidates. A session ends on `DELETE`, when the connection fails or
//! closes, when the monitor's stream stops, or when its user loses access
//! (checked again every [`RECHECK`]).

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::{broadcast, Notify};
use tracing::{debug, info, warn};
use webrtc::peer_connection::RTCPeerConnectionState;

use super::webrtc::{
    audio_plan, MediaPump, SessionHandshake, WebRtcLiveConfig, WebRtcLiveError, WebRtcLiveManager,
};
use crate::server::state::AppState;
use crate::streaming::source::{CachedKeyframe, MonitorSource, VideoCodec};
use crate::util::authz::Feature;

/// How often a playing session's user is checked again.
pub const RECHECK: Duration = Duration::from_secs(60);
/// How long the answer waits for this side's candidates.
const GATHER_WAIT: Duration = Duration::from_secs(2);
/// How long a viewer has to connect after the answer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a session couldn't start, as WHEP status codes.
#[derive(Debug, thiserror::Error)]
pub enum WhepError {
    /// 404: no such session, or not this caller's, or no stream for the monitor.
    #[error("{0}")]
    NotFound(String),
    /// 400: the offer isn't usable SDP.
    #[error("{0}")]
    BadOffer(String),
    /// 406: the offer can't receive the camera's codec.
    #[error("{0}")]
    Codec(String),
    /// 503: streaming off, or the session limit reached.
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}

struct Entry {
    monitor_id: u32,
    uid: u32,
    manager: Arc<WebRtcLiveManager>,
    stop: Arc<Notify>,
    /// Freed when the session's entry is removed.
    _slot: crate::streaming::live::slots::WebRtcSlot,
}

/// The WHEP sessions of this zm-api process.
#[derive(Default)]
pub struct WhepSessions {
    sessions: DashMap<String, Entry>,
}

impl WhepSessions {
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Answer `offer_sdp` for `monitor_id` on behalf of `uid`: returns the
    /// session id and the SDP answer, and starts streaming once the viewer
    /// connects.
    pub async fn start(
        state: &AppState,
        monitor_id: u32,
        uid: u32,
        offer_sdp: String,
    ) -> Result<(String, String), WhepError> {
        crate::streaming::live::webrtc::check_remote_sdp(&offer_sdp)
            .map_err(WhepError::BadOffer)?;
        let router = state
            .source_router
            .as_ref()
            .ok_or_else(|| WhepError::Unavailable("Live streaming not configured".into()))?;
        let limit = state.config.streaming.webrtc.max_connections as usize;
        let slot = state.webrtc_slots.try_acquire(limit).ok_or_else(|| {
            WhepError::Unavailable(format!("WebRTC session limit ({limit}) reached"))
        })?;
        if !router.is_available(monitor_id) {
            return Err(WhepError::NotFound(format!(
                "No stream for monitor {monitor_id}"
            )));
        }
        let source = router
            .get_source(monitor_id)
            .await
            .map_err(|e| WhepError::Unavailable(e.to_string()))?;
        let cached = source.cached_keyframe();
        let (codec, profile) = match &cached {
            Some(ck) => (ck.codec, Some(ck.profile_level_id.clone())),
            None => {
                // The reader learns the codec from the socket's first packet.
                tokio::time::sleep(Duration::from_millis(100)).await;
                match source.codec().await {
                    VideoCodec::Unknown => (VideoCodec::H264, None),
                    known => (known, None),
                }
            }
        };
        let (audio_kind, transcoder) = audio_plan(monitor_id, source.audio_codec());

        let manager = Arc::new(WebRtcLiveManager::new(
            WebRtcLiveConfig::from_webrtc_config(&state.config.streaming.webrtc),
        ));
        let handshake = manager
            .answer_offer(
                monitor_id,
                codec,
                profile.as_deref(),
                audio_kind,
                offer_sdp,
                GATHER_WAIT,
            )
            .await
            .map_err(|e| match e {
                WebRtcLiveError::InvalidOffer(m) => WhepError::BadOffer(m),
                WebRtcLiveError::UnsupportedCodec(m) => WhepError::Codec(m),
                other => WhepError::Internal(other.to_string()),
            })?;
        let session_id = handshake.session_id.clone();
        let answer = handshake.offer.sdp.clone();
        let has_audio = audio_kind.is_some() && answer.contains("m=audio");
        let stop = Arc::new(Notify::new());
        state.whep.sessions.insert(
            session_id.clone(),
            Entry {
                monitor_id,
                uid,
                manager: manager.clone(),
                stop: stop.clone(),
                _slot: slot,
            },
        );
        info!("WHEP session {session_id} for monitor {monitor_id}, user {uid}");
        tokio::spawn(run(
            state.clone(),
            manager,
            handshake,
            source,
            cached,
            MediaPump::new(session_id.clone(), transcoder),
            has_audio,
            uid,
            stop,
        ));
        Ok((session_id, answer))
    }

    fn owned(
        &self,
        id: &str,
        monitor_id: u32,
        uid: u32,
    ) -> Result<(Arc<WebRtcLiveManager>, Arc<Notify>), WhepError> {
        self.sessions
            .get(id)
            .filter(|e| e.monitor_id == monitor_id && e.uid == uid)
            .map(|e| (e.manager.clone(), e.stop.clone()))
            .ok_or_else(|| WhepError::NotFound("No such WHEP session".into()))
    }

    /// Add the viewer's trickled candidates (`application/trickle-ice-sdpfrag`).
    pub async fn add_candidates(
        &self,
        id: &str,
        monitor_id: u32,
        uid: u32,
        fragment: &str,
    ) -> Result<(), WhepError> {
        let (manager, _) = self.owned(id, monitor_id, uid)?;
        for (mid, candidate) in parse_sdpfrag(fragment) {
            if let Err(e) = manager.add_ice_candidate(id, &candidate, mid, None).await {
                debug!("WHEP {id}: candidate refused: {e}");
            }
        }
        Ok(())
    }

    /// End a session (`DELETE`).
    pub fn stop(&self, id: &str, monitor_id: u32, uid: u32) -> Result<(), WhepError> {
        let (_, stop) = self.owned(id, monitor_id, uid)?;
        stop.notify_one();
        Ok(())
    }
}

/// The candidates of a trickle-ice-sdpfrag (RFC 8840), each with the
/// `a=mid` it follows. `a=end-of-candidates` and ICE credentials are
/// ignored (no ICE restart).
pub fn parse_sdpfrag(fragment: &str) -> Vec<(Option<String>, String)> {
    let mut mid = None;
    let mut out = Vec::new();
    for line in fragment.lines().map(str::trim) {
        if let Some(m) = line.strip_prefix("a=mid:") {
            mid = Some(m.to_string());
        } else if let Some(c) = line.strip_prefix("a=") {
            if c.starts_with("candidate:") {
                out.push((mid.clone(), c.to_string()));
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn run(
    state: AppState,
    manager: Arc<WebRtcLiveManager>,
    handshake: SessionHandshake,
    source: Arc<MonitorSource>,
    cached: Option<CachedKeyframe>,
    mut pump: MediaPump,
    has_audio: bool,
    uid: u32,
    stop: Arc<Notify>,
) {
    let SessionHandshake {
        session_id,
        mut pc_state_rx,
        ..
    } = handshake;
    let monitor_id = source.monitor_id();
    // The credential check below counts from the session's start.
    let started = crate::util::time::unix_now();

    let connected = tokio::select! {
        r = tokio::time::timeout(
            CONNECT_TIMEOUT,
            pc_state_rx.wait_for(|s| matches!(
                s,
                RTCPeerConnectionState::Connected
                    | RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Closed
            )),
        ) => matches!(r, Ok(Ok(ref s)) if **s == RTCPeerConnectionState::Connected),
        _ = stop.notified() => false,
    };

    if connected {
        info!("WHEP session {session_id} connected");
        let keyframe = source.cached_keyframe().or(cached);
        pump.inject_keyframe(&manager, keyframe.as_ref()).await;
        let mut video_rx = source.subscribe_video();
        let mut audio_rx = has_audio.then(|| source.subscribe_audio());
        let mut health_rx = source.subscribe_reader_health();
        let mut recheck = tokio::time::interval(RECHECK);
        recheck.tick().await;
        loop {
            tokio::select! {
                r = video_rx.recv() => match r {
                    Ok(p) => { pump.video(&manager, &p).await; }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!("WHEP session {session_id} lagged {n} packets");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                r = async {
                    match audio_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => match r {
                    Ok(p) => pump.audio(&manager, &p).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => audio_rx = None,
                },
                r = pc_state_rx.changed() => {
                    let state = r.ok().map(|_| *pc_state_rx.borrow_and_update());
                    if !matches!(state, Some(RTCPeerConnectionState::Connected)) {
                        info!("WHEP session {session_id}: connection {state:?}");
                        break;
                    }
                }
                r = health_rx.changed() => if r.is_err() {
                    warn!("WHEP session {session_id}: monitor {monitor_id}'s reader stopped");
                    break;
                },
                _ = recheck.tick() => {
                    if let Err(e) = crate::service::media_grant::check_viewer(
                        &state, uid, started, Feature::Stream, monitor_id,
                    ).await {
                        info!("WHEP session {session_id}: user {uid} lost access ({e}); closing");
                        break;
                    }
                }
                _ = stop.notified() => {
                    info!("WHEP session {session_id} deleted");
                    break;
                }
            }
        }
    } else {
        info!("WHEP session {session_id}: viewer didn't connect");
    }
    let _ = manager.remove_session(&session_id).await;
    state.whep.sessions.remove(&session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdpfrag_candidates_keep_their_mid() {
        let frag = "a=ice-ufrag:abcd\r\na=ice-pwd:x\r\na=group:BUNDLE 0\r\n\
            m=video 9 RTP/AVP 0\r\na=mid:0\r\n\
            a=candidate:1 1 udp 2130706431 192.168.1.5 50000 typ host\r\n\
            a=candidate:2 1 udp 1694498815 203.0.113.9 50000 typ srflx raddr 0.0.0.0 rport 0\r\n\
            a=end-of-candidates\r\n";
        let got = parse_sdpfrag(frag);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0.as_deref(), Some("0"));
        assert!(got[0].1.starts_with("candidate:1 1 udp"));
        assert!(got[1].1.contains("typ srflx"));
        assert!(parse_sdpfrag("a=end-of-candidates\r\n").is_empty());
    }
}
