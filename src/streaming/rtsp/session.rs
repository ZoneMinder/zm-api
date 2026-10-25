//! One RTSP connection: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN and
//! keep-alives, with RTP sent interleaved on the same connection.
//!
//! The first request that names a monitor must carry a signed link
//! (`?st=`). The connection is then bound to that monitor; later requests
//! may leave the link out, but may not name another monitor. While playing,
//! the link is checked again every [`Backend::reauth_interval`], so a
//! revoked user, a lost permission or an expired link ends the stream.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, info};

use super::message::{self, parse_target, track_url, Incoming, Request, Response, Target};
use super::rtp::{interleave, rtp_timestamp, Packetizer};
use super::sdp;
use crate::streaming::live::webrtc::AccessUnitAssembler;
use crate::streaming::source::media::{VideoCodec, VideoPacket};

/// A checked signed link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    pub uid: u32,
    /// Unix time the link expires.
    pub exp: i64,
}

/// A monitor's live video, ready to stream.
pub struct Media {
    pub codec: VideoCodec,
    /// Parameter sets and a keyframe (Annex B), sent first so playback
    /// starts at once.
    pub keyframe: Vec<u8>,
    pub keyframe_us: i64,
    pub packets: broadcast::Receiver<VideoPacket>,
}

/// What the server needs from the rest of zm-api. Errors are RTSP status
/// codes.
pub trait Backend: Clone + Send + Sync + 'static {
    fn authorize(
        &self,
        token: &str,
        monitor_id: u32,
    ) -> impl Future<Output = Result<Grant, u16>> + Send;

    fn open(&self, monitor_id: u32) -> impl Future<Output = Result<Media, u16>> + Send;

    /// How often a playing stream's link is checked again.
    fn reauth_interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    /// How long a new connection has to present a working link. Until then
    /// it holds a connection slot, and OPTIONS needs no link, so without a
    /// deadline anyone could hold every slot.
    fn auth_deadline(&self) -> Duration {
        Duration::from_secs(15)
    }
}

/// How long a connection may sit idle before it plays.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Failed link checks a connection may make before it is closed.
const MAX_AUTH_FAILURES: u32 = 3;
/// Interleaved frames queued for a slow client before frames are dropped
/// until the next keyframe.
const QUEUE: usize = 1024;
/// The session timeout advertised to clients, in seconds.
const SESSION_TIMEOUT: u32 = 60;

const PUBLIC: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER, SET_PARAMETER";

/// Serve one connection until the client leaves, tears down, or its link
/// stops working.
pub async fn serve<S, B>(stream: S, backend: B, peer: String)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    B: Backend,
{
    let (rd, wr) = tokio::io::split(stream);
    let mut rd = BufReader::new(rd);
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(QUEUE);
    let closed = Arc::new(Notify::new());
    let writer = tokio::spawn({
        let closed = closed.clone();
        async move {
            let mut wr = wr;
            while let Some(bytes) = rx.recv().await {
                if wr.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            let _ = wr.shutdown().await;
            closed.notify_one();
        }
    });

    let auth_by = tokio::time::Instant::now() + backend.auth_deadline();
    let mut conn = Conn::new(backend, tx.clone(), closed.clone());
    loop {
        let wait = if conn.playing.is_some() {
            Duration::from_secs(24 * 60 * 60)
        } else if conn.bound.is_none() {
            let left = auth_by.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                debug!("rtsp {peer}: no working link in time");
                break;
            }
            IDLE_TIMEOUT.min(left)
        } else {
            IDLE_TIMEOUT
        };
        let next = tokio::select! {
            r = tokio::time::timeout(wait, message::read(&mut rd)) => r,
            _ = closed.notified() => break,
        };
        let req = match next {
            Ok(Ok(Some(Incoming::Request(req)))) => req,
            Ok(Ok(Some(Incoming::Data))) => continue,
            Ok(Ok(None)) | Err(_) => break,
            Ok(Err(e)) => {
                debug!("rtsp {peer}: bad request: {e}");
                let _ = tx.send(Response::new(400).encode(None)).await;
                break;
            }
        };
        let resp = conn.handle(&req).await;
        if tx.send(resp.encode(req.header("CSeq"))).await.is_err() || conn.done {
            break;
        }
    }
    conn.stop();
    drop(conn);
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    debug!("rtsp {peer}: closed");
}

struct Bound {
    monitor_id: u32,
    token: String,
    grant: Grant,
    origin: String,
}

struct Setup {
    channel: u8,
    ssrc: u32,
    first_seq: u16,
}

struct Conn<B> {
    backend: B,
    tx: mpsc::Sender<Vec<u8>>,
    closed: Arc<Notify>,
    bound: Option<Bound>,
    failures: u32,
    media: Option<Media>,
    session: String,
    setup: Option<Setup>,
    playing: Option<JoinHandle<()>>,
    done: bool,
}

impl<B: Backend> Conn<B> {
    fn new(backend: B, tx: mpsc::Sender<Vec<u8>>, closed: Arc<Notify>) -> Self {
        Self {
            backend,
            tx,
            closed,
            bound: None,
            failures: 0,
            media: None,
            session: format!("{:016x}", rand::random::<u64>()),
            setup: None,
            playing: None,
            done: false,
        }
    }

    fn stop(&mut self) {
        if let Some(task) = self.playing.take() {
            task.abort();
        }
    }

    async fn handle(&mut self, req: &Request) -> Response {
        let result = match req.method.as_str() {
            "OPTIONS" => Ok(Response::new(200).header("Public", PUBLIC)),
            "DESCRIBE" => self.describe(req).await,
            "SETUP" => self.setup(req).await,
            "PLAY" => self.play(req).await,
            "TEARDOWN" => {
                self.stop();
                self.done = true;
                Ok(Response::new(200))
            }
            "GET_PARAMETER" | "SET_PARAMETER" => Ok(Response::new(200)),
            _ => Err(501),
        };
        match result {
            Ok(r) if self.setup.is_some() => r.header(
                "Session",
                format!("{};timeout={SESSION_TIMEOUT}", self.session),
            ),
            Ok(r) => r,
            Err(501) => Response::new(501).header("Public", PUBLIC),
            Err(code) => Response::new(code),
        }
    }

    /// Check the request's target and link, binding the connection to its
    /// monitor on the first success.
    async fn authorize(&mut self, req: &Request) -> Result<Target, u16> {
        let target = parse_target(&req.url).ok_or(404u16)?;
        if let Some(bound) = &self.bound {
            if bound.monitor_id != target.monitor_id {
                return Err(403);
            }
        }
        match &target.token {
            Some(token) => match self.backend.authorize(token, target.monitor_id).await {
                Ok(grant) => {
                    self.bound = Some(Bound {
                        monitor_id: target.monitor_id,
                        token: token.clone(),
                        grant,
                        origin: target.origin.clone(),
                    });
                    Ok(target)
                }
                Err(code) => {
                    self.failures += 1;
                    if self.failures >= MAX_AUTH_FAILURES {
                        self.done = true;
                    }
                    Err(code)
                }
            },
            None if self.bound.is_some() => Ok(target),
            None => Err(401),
        }
    }

    async fn ensure_media(&mut self, monitor_id: u32) -> Result<(), u16> {
        if self.media.is_none() {
            self.media = Some(self.backend.open(monitor_id).await?);
        }
        Ok(())
    }

    async fn describe(&mut self, req: &Request) -> Result<Response, u16> {
        let target = self.authorize(req).await?;
        self.ensure_media(target.monitor_id).await?;
        let bound = self.bound.as_ref().expect("authorized");
        let media = self.media.as_ref().expect("opened");
        let track = track_url(&bound.origin, bound.monitor_id, &bound.token);
        let body = sdp::describe(bound.monitor_id, media.codec, &media.keyframe, &track);
        Ok(Response::new(200)
            .header("Content-Base", req.url.clone())
            .body("application/sdp", body))
    }

    async fn setup(&mut self, req: &Request) -> Result<Response, u16> {
        if self.playing.is_some() {
            return Err(455);
        }
        let target = self.authorize(req).await?;
        let transport = req.header("Transport").ok_or(461u16)?;
        let channel = tcp_channel(transport).ok_or(461u16)?;
        self.ensure_media(target.monitor_id).await?;
        let setup = Setup {
            channel,
            ssrc: rand::random(),
            first_seq: rand::random(),
        };
        let reply = format!(
            "RTP/AVP/TCP;unicast;interleaved={}-{};ssrc={:08X}",
            setup.channel,
            setup.channel.wrapping_add(1),
            setup.ssrc
        );
        self.setup = Some(setup);
        Ok(Response::new(200).header("Transport", reply))
    }

    async fn play(&mut self, req: &Request) -> Result<Response, u16> {
        let session = req
            .header("Session")
            .map(|s| s.split(';').next().unwrap_or("").trim());
        if session != Some(self.session.as_str()) {
            return Err(454);
        }
        if self.setup.is_none() {
            return Err(455);
        }
        let target = self.authorize(req).await?;
        if target.track {
            return Err(459);
        }
        let bound = self.bound.as_ref().expect("authorized");
        let setup = self.setup.as_ref().expect("set up");
        let track = track_url(&bound.origin, bound.monitor_id, &bound.token);
        let base_rtp: u32 = rand::random();
        let rtp_info = format!("url={track};seq={};rtptime={base_rtp}", setup.first_seq);
        if self.playing.is_none() {
            let media = self.media.take().ok_or(455u16)?;
            info!(
                "rtsp: streaming monitor {} to user {}",
                bound.monitor_id, bound.grant.uid
            );
            self.playing = Some(tokio::spawn(stream(
                self.backend.clone(),
                bound.token.clone(),
                bound.monitor_id,
                bound.grant.exp,
                Packetizer::new(media.codec, setup.ssrc, setup.first_seq),
                media,
                setup.channel,
                base_rtp,
                self.tx.clone(),
                self.closed.clone(),
            )));
        }
        Ok(Response::new(200)
            .header("Range", "npt=0.000-")
            .header("RTP-Info", rtp_info))
    }
}

/// The interleaved channel of the first TCP transport offered, or `None`
/// when the client only offers UDP.
fn tcp_channel(transport: &str) -> Option<u8> {
    transport.split(',').find_map(|spec| {
        let mut parts = spec.split(';').map(str::trim);
        if !parts.next()?.eq_ignore_ascii_case("RTP/AVP/TCP") {
            return None;
        }
        let channel = parts
            .find_map(|p| p.strip_prefix("interleaved="))
            .and_then(|range| range.split('-').next())
            .map(|c| c.parse::<u8>().ok())
            .unwrap_or(Some(0))?;
        (channel < 254).then_some(channel)
    })
}

#[allow(clippy::too_many_arguments)]
async fn stream<B: Backend>(
    backend: B,
    token: String,
    monitor_id: u32,
    exp: i64,
    mut packetizer: Packetizer,
    mut media: Media,
    channel: u8,
    base_rtp: u32,
    tx: mpsc::Sender<Vec<u8>>,
    closed: Arc<Notify>,
) {
    let first_us = media.keyframe_us;
    // Send a whole access unit, or `false` when the client is gone or too
    // slow (the rest of the frame is dropped).
    let mut send = |au: &[u8], ts_us: i64| -> Result<(), bool> {
        let ts = rtp_timestamp(base_rtp, first_us, ts_us);
        for pkt in packetizer.packetize(au, ts) {
            match tx.try_send(interleave(channel, &pkt)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => return Err(false),
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(true),
            }
        }
        Ok(())
    };

    let mut assembler = AccessUnitAssembler::new();
    let mut need_keyframe = false;
    if !media.keyframe.is_empty() {
        if let Err(true) = send(&media.keyframe, media.keyframe_us) {
            return;
        }
        assembler.clear_needs_keyframe();
    }

    let mut reauth = tokio::time::interval(backend.reauth_interval());
    reauth.tick().await;
    loop {
        tokio::select! {
            pkt = media.packets.recv() => match pkt {
                Ok(pkt) => {
                    let Some(au) = assembler.push(&pkt) else { continue };
                    if need_keyframe && !au.is_keyframe {
                        continue;
                    }
                    need_keyframe = false;
                    match send(&au.data, au.timestamp_us) {
                        Ok(()) => {}
                        Err(true) => return,
                        Err(false) => need_keyframe = true,
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => need_keyframe = true,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = reauth.tick() => {
                let expired = chrono::Utc::now().timestamp() >= exp;
                if expired || backend.authorize(&token, monitor_id).await.is_err() {
                    info!("rtsp: monitor {monitor_id}: link no longer valid; closing");
                    break;
                }
            }
        }
    }
    closed.notify_one();
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
