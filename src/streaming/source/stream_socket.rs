//! Reader for ZoneMinder's per-monitor stream socket.
//!
//! Each `zmc` process serves the monitor's compressed media — video access
//! units and audio packets, as received from the camera — on a unix domain
//! socket at `{PATH_SOCKS}/stream_{monitor_id}.sock`. The socket replaces
//! the media FIFOs of earlier ZoneMinder versions: it is always on, carries
//! both streams on one connection, delivers codec parameters in a HELLO
//! handshake, sends the most recent cached keyframe to new consumers, and
//! makes packet loss observable per consumer (sequence gaps + STATS).
//!
//! On connect zmc enqueues, in order: video HELLO, audio HELLO (when the
//! monitor has audio), cached KEYFRAME. So the stream topology is fully
//! known before the first media message arrives.
//!
//! This reader translates wire messages into the pipeline's packet types:
//!
//! * one [`VideoPacket`] per NAL unit, every NAL of an access unit sharing
//!   that AU's timestamp (the grouping the segmenter and RTP packetizer
//!   need for multi-slice frames);
//! * one [`AudioPacket`] per audio frame, AAC re-framed as ADTS using the
//!   AudioSpecificConfig from the HELLO extradata (zmc sends raw frames);
//! * keyframe AUs that arrive without in-band parameter sets get the HELLO
//!   extradata NALs prepended, so decoders (and the keyframe cache) can
//!   always initialise from any keyframe.
//!
//! Timestamps are normalized so a session starts near zero: both streams
//! share one pts clock (`AV_TIME_BASE_Q`), so a single base — seeded by the
//! first media message — keeps A/V in sync.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use super::media::{
    codec_from_extradata, extradata_to_annexb_nals, h264_nal_type, h265_nal_type, nal_is_keyframe,
    split_annexb_nals, AdtsWrapper, AudioCodec, AudioPacket, VideoCodec, VideoPacket,
};
use super::protocol::{
    self, Header, MessageType, MonitorEvent, ProtocolError, StreamId, FLAG_KEYFRAME, HEADER_SIZE,
};
use crate::configure::streaming::ZoneMinderConfig;

/// Errors from the stream-socket source layer.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("stream socket not found: {path}")]
    NotFound { path: PathBuf },

    /// The socket exists but this process may not open it. zmc creates it mode
    /// 0660 owned by `ZM_STREAM_SOCKET_GROUP`, so the bare OS message ("permission
    /// denied") names nothing the operator can act on.
    #[error(
        "permission denied opening stream socket {path}: the zm-api service user must be a \
         member of ZoneMinder's ZM_STREAM_SOCKET_GROUP (the socket is mode 0660)"
    )]
    PermissionDenied { path: PathBuf },

    #[error("stream socket I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("read timeout after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },

    #[error("stream socket closed by zmc")]
    Closed,

    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("not connected")]
    NotConnected,
}

/// One decoded item from the socket, in wire order. Params events always
/// precede the media they describe.
#[derive(Debug)]
pub enum SocketEvent {
    /// Video stream parameters arrived (HELLO).
    VideoParams { codec: VideoCodec },
    /// Audio stream parameters arrived (HELLO).
    AudioParams { codec: AudioCodec },
    /// One video NAL unit.
    Video(VideoPacket),
    /// One audio frame.
    Audio(AudioPacket),
    /// A monitor lifecycle / analysis EVENT (zm-next or stock zmc). Carries no
    /// media; routed to DB ingest rather than the media sinks. EVENTs use their
    /// own per-monitor sequence counter, independent of the media streams.
    MonitorEvent(MonitorEvent),
    /// A zm-next worker's `0x12 Response` to a command this connection sent.
    CommandResponse(protocol::CommandResponse),
    /// A zm-next worker's hello (`0x14`), sent first on a new connection.
    WorkerHello(protocol::WorkerHello),
}

/// Turn a connect failure into the most actionable variant we can. EACCES is
/// the one worth naming: it means group membership, not a missing camera, and
/// it is otherwise indistinguishable from any other I/O error in the log.
fn classify_connect_error(e: std::io::Error, path: &Path) -> SourceError {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => SourceError::PermissionDenied {
            path: path.to_path_buf(),
        },
        _ => SourceError::Io(e),
    }
}

/// The documented-convention socket path for a monitor:
/// `{socks_path}/stream_{monitor_id}.sock`.
pub fn stream_socket_path(config: &ZoneMinderConfig, monitor_id: u32) -> PathBuf {
    Path::new(&config.socks_path).join(format!("stream_{monitor_id}.sock"))
}

/// Per-stream parameter state, reset by a HELLO for that stream.
#[derive(Default)]
struct VideoState {
    codec: VideoCodec,
    /// Parameter-set NALs from the HELLO extradata (Annex B), prepended to
    /// keyframe AUs that lack in-band sets.
    extradata_nals: Vec<Vec<u8>>,
    generation: Option<u32>,
    next_sequence: Option<u32>,
}

#[derive(Default)]
struct AudioState {
    codec: Option<AudioCodec>,
    /// Re-frames zmc's raw AAC into ADTS; `None` for non-AAC codecs or when
    /// the HELLO carried no usable AudioSpecificConfig.
    adts: Option<AdtsWrapper>,
    next_sequence: Option<u32>,
}

/// Reader for one monitor's stream socket.
pub struct StreamSocketReader {
    monitor_id: u32,
    path: PathBuf,
    config: ZoneMinderConfig,
    /// Read half of the connection. The write half ([`Self::take_writer`]) is
    /// handed to the router task so it can send control replies (the
    /// id-assignment handshake) on the same connection without contending with
    /// the read loop for `&mut self`.
    read: Option<OwnedReadHalf>,
    write: Option<OwnedWriteHalf>,
    /// Accumulation buffer of raw socket bytes awaiting a complete message.
    buf: Vec<u8>,
    /// Events decoded from messages, awaiting emission (one message can
    /// yield several events — an AU splits into NALs).
    pending: VecDeque<SocketEvent>,
    video: VideoState,
    audio: AudioState,
    /// First media pts observed on this connection, subtracted from every
    /// later pts so the session starts near zero. Both streams share the
    /// clock, so one base keeps A/V sync. Reset on (re)connect and on a
    /// video generation bump (camera reconfigure = possible new clock).
    base_pts_us: Option<i64>,
    /// Cumulative drop count reported by the last STATS message.
    last_reported_drops: u64,
}

impl StreamSocketReader {
    pub fn new(monitor_id: u32, config: ZoneMinderConfig) -> Self {
        let path = stream_socket_path(&config, monitor_id);
        Self {
            monitor_id,
            path,
            config,
            read: None,
            write: None,
            buf: Vec::with_capacity(64 * 1024),
            pending: VecDeque::new(),
            video: VideoState::default(),
            audio: AudioState::default(),
            base_pts_us: None,
            last_reported_drops: 0,
        }
    }

    pub fn monitor_id(&self) -> u32 {
        self.monitor_id
    }

    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Take the connection's write half for sending control replies. Available
    /// after [`Self::connect`]; returns `None` if already taken or not
    /// connected. The router task owns it for the connection's lifetime and
    /// drops it on reconnect (a fresh `connect` produces a new write half).
    pub fn take_writer(&mut self) -> Option<OwnedWriteHalf> {
        self.write.take()
    }

    /// Whether zmc has created the monitor's socket (it appears when zmc
    /// starts and survives camera reconnects).
    pub fn socket_exists(&self) -> bool {
        self.path.exists()
    }

    /// Connect to the monitor's stream socket. Resets all per-connection
    /// state — zmc replays the HELLO handshake on every connection.
    pub async fn connect(&mut self) -> Result<(), SourceError> {
        if !self.socket_exists() {
            return Err(SourceError::NotFound {
                path: self.path.clone(),
            });
        }

        info!(
            "Connecting to stream socket for monitor {}: {}",
            self.monitor_id,
            self.path.display()
        );

        let stream = UnixStream::connect(&self.path)
            .await
            .map_err(|e| classify_connect_error(e, &self.path))?;
        let (read, write) = stream.into_split();
        self.read = Some(read);
        self.write = Some(write);
        self.buf.clear();
        self.pending.clear();
        self.video = VideoState::default();
        self.audio = AudioState::default();
        self.base_pts_us = None;
        self.last_reported_drops = 0;

        info!("Connected to stream socket for monitor {}", self.monitor_id);
        Ok(())
    }

    /// Read the next event, bounded by the configured read timeout.
    ///
    /// `Err(Timeout)` is expected when no media is flowing (idle camera);
    /// callers should keep polling. `Err(Closed)` means zmc shut the stream
    /// down (BYE or EOF) — reconnect. Protocol errors mean the byte stream
    /// can no longer be trusted — reconnect.
    pub async fn next_event(&mut self) -> Result<SocketEvent, SourceError> {
        let timeout_ms = self.config.read_timeout_ms;
        tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            self.next_event_internal(),
        )
        .await
        .map_err(|_| SourceError::Timeout { timeout_ms })?
    }

    async fn next_event_internal(&mut self) -> Result<SocketEvent, SourceError> {
        loop {
            // 1. Emit an event already decoded from a previous message.
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }

            // 2. Decode the next complete message from the buffer.
            if self.buf.len() >= HEADER_SIZE {
                let header =
                    protocol::parse_header(self.buf[..HEADER_SIZE].try_into().expect("sized"))?;
                let total = HEADER_SIZE + header.payload_len;
                if self.buf.len() >= total {
                    let payload = self.buf[HEADER_SIZE..total].to_vec();
                    self.buf.drain(..total);
                    self.handle_message(header, payload)?;
                    continue;
                }
            }

            // 3. Need more bytes. `read_buf` appends to the buffer and is
            //    cancel-safe: if the caller's timeout fires, no data is lost.
            let read = self.read.as_mut().ok_or(SourceError::NotConnected)?;
            let n = read.read_buf(&mut self.buf).await?;
            if n == 0 {
                return Err(SourceError::Closed);
            }
        }
    }

    /// Decode one wire message into zero or more pending events.
    fn handle_message(&mut self, header: Header, payload: Vec<u8>) -> Result<(), SourceError> {
        // Command replies are a zm-next control extension, not a media type, so
        // they are matched on the raw byte. Malformed ones are skipped like a
        // malformed EVENT.
        if header.msg_type == protocol::MSG_TYPE_WORKER_HELLO {
            match protocol::parse_worker_hello(&payload) {
                Some(hello) => self.pending.push_back(SocketEvent::WorkerHello(hello)),
                None => debug!(
                    "Monitor {}: skipping malformed worker hello",
                    self.monitor_id
                ),
            }
            return Ok(());
        }
        if header.msg_type == protocol::MSG_TYPE_RESPONSE {
            match protocol::parse_command_response(&payload) {
                Some(resp) => self.pending.push_back(SocketEvent::CommandResponse(resp)),
                None => debug!(
                    "Monitor {}: skipping malformed command response",
                    self.monitor_id
                ),
            }
            return Ok(());
        }

        let Some(msg_type) = MessageType::from_u8(header.msg_type) else {
            debug!(
                "Monitor {}: skipping unknown message type {:#x}",
                self.monitor_id, header.msg_type
            );
            return Ok(());
        };

        // EVENT frames ride the Monitor stream and carry no media; handle them
        // before resolving a media stream id. A malformed EVENT is skipped (not
        // fatal): the header already framed the payload, so one bad event does
        // not desync the byte stream, and media must keep flowing.
        if msg_type == MessageType::Event {
            match protocol::parse_event(&payload) {
                Ok(ev) => self.pending.push_back(SocketEvent::MonitorEvent(ev)),
                Err(e) => debug!(
                    "Monitor {}: skipping malformed EVENT payload: {e}",
                    self.monitor_id
                ),
            }
            return Ok(());
        }

        let Some(stream_id) = StreamId::from_u8(header.stream) else {
            debug!(
                "Monitor {}: skipping unknown stream id {}",
                self.monitor_id, header.stream
            );
            return Ok(());
        };
        // Media / HELLO on the Monitor stream is meaningless — only EVENT is
        // valid there. Skip rather than misinterpret it as video/audio.
        if stream_id == StreamId::Monitor {
            debug!(
                "Monitor {}: skipping {:?} on the monitor stream",
                self.monitor_id, msg_type
            );
            return Ok(());
        }

        match msg_type {
            MessageType::Hello => self.handle_hello(stream_id, header, &payload)?,
            MessageType::Media | MessageType::Keyframe => {
                self.handle_media(stream_id, header, payload)
            }
            MessageType::Event => unreachable!("EVENT handled above"),
            MessageType::Stats => {
                if let Ok((sent, dropped)) = protocol::parse_stats(&payload) {
                    if dropped > self.last_reported_drops {
                        warn!(
                            "Monitor {}: zmc dropped {} stream messages for this consumer \
                             ({} total sent) — consumer too slow",
                            self.monitor_id,
                            dropped - self.last_reported_drops,
                            sent
                        );
                        self.last_reported_drops = dropped;
                    } else {
                        debug!(
                            "Monitor {} stream socket stats: sent={} dropped={}",
                            self.monitor_id, sent, dropped
                        );
                    }
                }
            }
            MessageType::Bye => {
                info!(
                    "Monitor {}: zmc is shutting the stream down (BYE)",
                    self.monitor_id
                );
                return Err(SourceError::Closed);
            }
        }
        Ok(())
    }

    fn handle_hello(
        &mut self,
        stream_id: StreamId,
        header: Header,
        payload: &[u8],
    ) -> Result<(), SourceError> {
        let hello = protocol::parse_hello(payload)?;
        match stream_id {
            StreamId::Video => {
                let mut codec = protocol::video_codec_from_id(hello.codec_id);
                if codec == VideoCodec::Unknown {
                    // The id is in zmc's FFmpeg numbering, which need not be
                    // ours (#212); the parameter sets say what it is.
                    codec = codec_from_extradata(&hello.extradata);
                    if codec == VideoCodec::Unknown {
                        warn!(
                            "Monitor {}: unsupported video codec id {} on stream socket",
                            self.monitor_id, hello.codec_id
                        );
                    } else {
                        info!(
                            "Monitor {}: codec id {} is not one this build's FFmpeg \
                             knows; identified {} from its parameter sets (zmc links \
                             a different FFmpeg version)",
                            self.monitor_id,
                            hello.codec_id,
                            codec.as_str()
                        );
                    }
                }
                // A generation bump means the camera reconfigured — its pts
                // clock may have restarted, so re-base both streams.
                if self
                    .video
                    .generation
                    .is_some_and(|g| g != header.generation)
                {
                    info!(
                        "Monitor {}: video stream generation {} → {}, re-initialising",
                        self.monitor_id,
                        self.video.generation.unwrap_or(0),
                        header.generation
                    );
                    self.base_pts_us = None;
                }
                self.video = VideoState {
                    codec,
                    extradata_nals: extradata_to_annexb_nals(&hello.extradata, codec),
                    generation: Some(header.generation),
                    next_sequence: None,
                };
                info!(
                    "Monitor {}: video stream is {} ({}x{}, {} extradata NALs)",
                    self.monitor_id,
                    codec.as_str(),
                    hello.width,
                    hello.height,
                    self.video.extradata_nals.len()
                );
                self.pending.push_back(SocketEvent::VideoParams { codec });
            }
            StreamId::Audio => {
                let codec = protocol::audio_codec_from_id(hello.codec_id);
                let adts = if codec == AudioCodec::Aac {
                    let wrapper = AdtsWrapper::from_asc(&hello.extradata);
                    if wrapper.is_none() {
                        warn!(
                            "Monitor {}: AAC stream HELLO carried no usable \
                             AudioSpecificConfig; forwarding frames unframed",
                            self.monitor_id
                        );
                    }
                    wrapper
                } else {
                    None
                };
                self.audio = AudioState {
                    codec: Some(codec),
                    adts,
                    next_sequence: None,
                };
                info!(
                    "Monitor {}: audio stream is {} ({} Hz, {} ch)",
                    self.monitor_id,
                    codec.as_str(),
                    hello.sample_rate,
                    hello.channels
                );
                self.pending.push_back(SocketEvent::AudioParams { codec });
            }
            // Guarded out in handle_message; only video/audio HELLOs reach here.
            StreamId::Monitor => unreachable!("monitor stream has no HELLO"),
        }
        Ok(())
    }

    fn handle_media(&mut self, stream_id: StreamId, header: Header, payload: Vec<u8>) {
        // Sequence gaps mean zmc dropped messages from our queue (slow
        // consumer) — observable here per the protocol design.
        let next_seq = match stream_id {
            StreamId::Video => &mut self.video.next_sequence,
            StreamId::Audio => &mut self.audio.next_sequence,
            StreamId::Monitor => unreachable!("monitor stream carries no media"),
        };
        if let Some(expected) = *next_seq {
            if header.sequence != expected {
                debug!(
                    "Monitor {}: {:?} stream sequence gap (expected {}, got {})",
                    self.monitor_id, stream_id, expected, header.sequence
                );
            }
        }
        *next_seq = Some(header.sequence.wrapping_add(1));

        // Normalize so the session starts near zero. Both streams share the
        // pts clock, so the first media message on either seeds the base.
        let base = *self.base_pts_us.get_or_insert(header.pts_us);
        let timestamp_us = header.pts_us.saturating_sub(base).max(0);

        match stream_id {
            StreamId::Video => {
                let codec = self.video.codec;
                if codec == VideoCodec::Unknown {
                    // Unsupported codec (warned at HELLO) or media before any
                    // HELLO (cannot happen with a spec-conforming zmc).
                    return;
                }

                let mut nals = split_annexb_nals(payload);
                if nals.is_empty() {
                    return;
                }

                // Keyframe AUs must be decodable in isolation: when the AU
                // carries no in-band parameter sets, prepend the HELLO
                // extradata so the keyframe cache and late-joining decoders
                // can initialise from it.
                if header.flags & FLAG_KEYFRAME != 0
                    && !au_has_parameter_sets(&nals, codec)
                    && !self.video.extradata_nals.is_empty()
                {
                    let mut with_params = self.video.extradata_nals.clone();
                    with_params.append(&mut nals);
                    nals = with_params;
                }

                for nal in nals {
                    let is_keyframe = nal_is_keyframe(&nal, codec);
                    self.pending.push_back(SocketEvent::Video(VideoPacket {
                        monitor_id: self.monitor_id,
                        timestamp_us,
                        data: nal,
                        is_keyframe,
                        codec,
                    }));
                }
            }
            StreamId::Audio => {
                let Some(codec) = self.audio.codec else {
                    return; // media before HELLO — cannot interpret
                };
                let data = match &self.audio.adts {
                    Some(wrapper) => match wrapper.wrap(&payload) {
                        Some(framed) => framed,
                        None => return, // oversized frame — corrupt, drop
                    },
                    None => payload,
                };
                self.pending.push_back(SocketEvent::Audio(AudioPacket {
                    monitor_id: self.monitor_id,
                    timestamp_us,
                    data,
                    codec,
                }));
            }
            StreamId::Monitor => unreachable!("monitor stream carries no media"),
        }
    }
}

/// Whether an AU's NAL list already includes an SPS (the indicator that the
/// camera repeats parameter sets in-band).
fn au_has_parameter_sets(nals: &[Vec<u8>], codec: VideoCodec) -> bool {
    nals.iter().any(|nal| match codec {
        VideoCodec::H264 => h264_nal_type(nal) == Some(7),
        VideoCodec::H265 => h265_nal_type(nal) == Some(33),
        VideoCodec::Unknown => false,
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A scripted fake zmc: listens on a unix socket and serves each
    //! connection the same canned byte stream, then keeps the connection
    //! open until the script says to close it.

    use std::path::PathBuf;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    /// Spawn a fake zmc serving `script` to every connection. When
    /// `close_after` is true the server closes the connection after writing;
    /// otherwise it holds it open (reads and discards nothing — zmc ignores
    /// inbound bytes, and the tests never write any).
    pub fn spawn_fake_zmc(
        path: PathBuf,
        script: Vec<u8>,
        close_after: bool,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(&path).expect("bind fake zmc socket");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let script = script.clone();
                tokio::spawn(async move {
                    let _ = stream.write_all(&script).await;
                    if !close_after {
                        // Hold the connection open until the peer goes away.
                        let mut sink = [0u8; 64];
                        use tokio::io::AsyncReadExt;
                        while let Ok(n) = stream.read(&mut sink).await {
                            if n == 0 {
                                break;
                            }
                        }
                    }
                });
            }
        })
    }

    /// Like [`spawn_fake_zmc`] but captures bytes the client writes back (the
    /// control-channel replies), forwarding each read chunk on the returned
    /// channel. Holds the connection open. Used to assert the id-assignment
    /// handshake reaches the worker.
    pub fn spawn_fake_zmc_capturing(
        path: PathBuf,
        script: Vec<u8>,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = UnixListener::bind(&path).expect("bind fake zmc socket");
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let script = script.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = stream.write_all(&script).await;
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let _ = tx.send(buf[..n].to_vec());
                    }
                });
            }
        });
        (handle, rx)
    }

    /// A temp dir for a test's socket, cleaned by the caller.
    pub fn test_sock_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zm_sock_{}_{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::super::protocol::test_encode::*;
    use super::super::protocol::{EVENT_DETECTION, EVENT_SNAPSHOT};
    use super::test_support::*;
    use super::*;

    fn test_config(dir: &Path) -> ZoneMinderConfig {
        ZoneMinderConfig {
            socks_path: dir.to_string_lossy().into_owned(),
            ..ZoneMinderConfig::default()
        }
    }

    const SPS: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67, 0x4D, 0x00, 0x33];
    const PPS: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80];

    fn idr_au() -> Vec<u8> {
        vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00]
    }

    fn p_slice_au() -> Vec<u8> {
        vec![0x00, 0x00, 0x00, 0x01, 0x41, 0x9A, 0x21]
    }

    fn extradata() -> Vec<u8> {
        let mut e = SPS.to_vec();
        e.extend_from_slice(PPS);
        e
    }

    async fn collect_events(reader: &mut StreamSocketReader, count: usize) -> Vec<SocketEvent> {
        let mut events = Vec::new();
        while events.len() < count {
            match reader.next_event().await {
                Ok(ev) => events.push(ev),
                Err(e) => panic!("expected event #{}, got error: {e}", events.len() + 1),
            }
        }
        events
    }

    #[test]
    fn socket_path_follows_convention() {
        let config = ZoneMinderConfig {
            socks_path: "/run/zm".to_string(),
            ..ZoneMinderConfig::default()
        };
        assert_eq!(
            stream_socket_path(&config, 7),
            PathBuf::from("/run/zm/stream_7.sock")
        );
    }

    #[tokio::test]
    async fn connect_fails_when_socket_missing() {
        let dir = test_sock_dir("missing");
        let mut reader = StreamSocketReader::new(99, test_config(&dir));
        assert!(!reader.socket_exists());
        assert!(matches!(
            reader.connect().await,
            Err(SourceError::NotFound { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handshake_then_keyframe_then_media() {
        let dir = test_sock_dir("handshake");
        let sock = dir.join("stream_1.sock");

        // zmc's connect-time order: video HELLO, cached KEYFRAME (no audio),
        // then live MEDIA. The keyframe AU has no in-band SPS/PPS, so the
        // reader must prepend the extradata NALs.
        let mut script = Vec::new();
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            0,
            0,
            &hello_payload(h264_codec_id(), &extradata()),
        ));
        script.extend_from_slice(&encode_message(
            0x03,
            0,
            FLAG_KEYFRAME,
            5,
            0,
            9_000_000,
            &idr_au(),
        ));
        script.extend_from_slice(&encode_message(0x02, 0, 0, 6, 0, 9_040_000, &p_slice_au()));

        let server = spawn_fake_zmc(sock, script, false);

        let mut reader = StreamSocketReader::new(1, test_config(&dir));
        reader.connect().await.expect("connect");

        // VideoParams + 3 keyframe NALs (SPS, PPS, IDR) + 1 P-slice NAL.
        let events = collect_events(&mut reader, 5).await;

        let SocketEvent::VideoParams { codec } = &events[0] else {
            panic!("expected VideoParams first, got {:?}", events[0]);
        };
        assert_eq!(*codec, VideoCodec::H264);

        let nal_types: Vec<Option<u8>> = events[1..]
            .iter()
            .map(|e| match e {
                SocketEvent::Video(p) => h264_nal_type(&p.data),
                other => panic!("expected video packet, got {other:?}"),
            })
            .collect();
        assert_eq!(nal_types, vec![Some(7), Some(8), Some(5), Some(1)]);

        // Keyframe flag is per-NAL: only the IDR slice carries it.
        let keyframes: Vec<bool> = events[1..]
            .iter()
            .map(|e| match e {
                SocketEvent::Video(p) => p.is_keyframe,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(keyframes, vec![false, false, true, false]);

        // Timestamps: keyframe AU seeds the base (0), P-slice is +40ms; the
        // prepended parameter sets share the keyframe's timestamp.
        let timestamps: Vec<i64> = events[1..]
            .iter()
            .map(|e| match e {
                SocketEvent::Video(p) => p.timestamp_us,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(timestamps, vec![0, 0, 0, 40_000]);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #212: zmc linked against FFmpeg 7 sends HEVC as 173, which an FFmpeg 9
    /// build numbers 172. An id this build does not know must not drop an
    /// H.265 stream whose parameter sets say what it is.
    #[tokio::test]
    async fn unrecognised_codec_id_is_identified_from_parameter_sets() {
        let dir = test_sock_dir("codec-sniff");
        let sock = dir.join("stream_7.sock");

        let vps: &[u8] = &[0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01];
        let sps: &[u8] = &[0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01];
        let pps: &[u8] = &[0, 0, 0, 1, 0x44, 0x01, 0xC1, 0x72];
        // IDR_W_RADL (type 19).
        let idr: &[u8] = &[0, 0, 0, 1, 0x26, 0x01, 0xAF, 0x08];
        let foreign_id = u32::MAX - 1;
        assert_eq!(
            protocol::video_codec_from_id(foreign_id),
            VideoCodec::Unknown
        );

        let mut script = Vec::new();
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            0,
            0,
            &hello_payload(foreign_id, &[vps, sps, pps].concat()),
        ));
        script.extend_from_slice(&encode_message(0x03, 0, FLAG_KEYFRAME, 1, 0, 1000, idr));

        let server = spawn_fake_zmc(sock, script, false);
        let mut reader = StreamSocketReader::new(7, test_config(&dir));
        reader.connect().await.expect("connect");

        // VideoParams, then VPS + SPS + PPS + IDR.
        let events = collect_events(&mut reader, 5).await;
        let SocketEvent::VideoParams { codec } = &events[0] else {
            panic!("expected VideoParams first, got {:?}", events[0]);
        };
        assert_eq!(*codec, VideoCodec::H265);
        let SocketEvent::Video(last) = &events[4] else {
            panic!("expected the IDR, got {:?}", events[4]);
        };
        assert_eq!(h265_nal_type(&last.data), Some(19));
        assert!(last.is_keyframe);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn keyframe_with_inband_sets_is_not_double_prefixed() {
        let dir = test_sock_dir("inband");
        let sock = dir.join("stream_2.sock");

        // Camera repeats SPS/PPS in-band before the IDR.
        let mut au = extradata();
        au.extend_from_slice(&idr_au());

        let mut script = Vec::new();
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            0,
            0,
            &hello_payload(h264_codec_id(), &extradata()),
        ));
        script.extend_from_slice(&encode_message(0x02, 0, FLAG_KEYFRAME, 0, 0, 1000, &au));

        let server = spawn_fake_zmc(sock, script, false);
        let mut reader = StreamSocketReader::new(2, test_config(&dir));
        reader.connect().await.unwrap();

        // VideoParams + exactly 3 NALs — no duplicated parameter sets.
        let events = collect_events(&mut reader, 4).await;
        let nal_types: Vec<Option<u8>> = events[1..]
            .iter()
            .map(|e| match e {
                SocketEvent::Video(p) => h264_nal_type(&p.data),
                other => panic!("expected video packet, got {other:?}"),
            })
            .collect();
        assert_eq!(nal_types, vec![Some(7), Some(8), Some(5)]);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn raw_aac_is_adts_framed_with_shared_clock() {
        use super::super::media::AdtsHeader;

        let dir = test_sock_dir("aac");
        let sock = dir.join("stream_3.sock");

        let raw_aac = vec![0x21, 0x1B, 0x80, 0x00, 0x55];
        let asc = [0x14, 0x08]; // AAC-LC 16 kHz mono

        let mut script = Vec::new();
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            0,
            0,
            &hello_payload(h264_codec_id(), &extradata()),
        ));
        script.extend_from_slice(&encode_message(
            0x01,
            1,
            0,
            0,
            0,
            0,
            &hello_payload(aac_codec_id(), &asc),
        ));
        // Video seeds the shared base at pts 7s; audio at 7.064s lands 64ms in.
        script.extend_from_slice(&encode_message(
            0x02,
            0,
            FLAG_KEYFRAME,
            0,
            0,
            7_000_000,
            &idr_au(),
        ));
        script.extend_from_slice(&encode_message(0x02, 1, 0, 0, 0, 7_064_000, &raw_aac));

        let server = spawn_fake_zmc(sock, script, false);
        let mut reader = StreamSocketReader::new(3, test_config(&dir));
        reader.connect().await.unwrap();

        // VideoParams, AudioParams, 3 video NALs (extradata prepended), audio.
        let events = collect_events(&mut reader, 6).await;

        let SocketEvent::AudioParams { codec } = &events[1] else {
            panic!("expected AudioParams second, got {:?}", events[1]);
        };
        assert_eq!(*codec, AudioCodec::Aac);

        let SocketEvent::Audio(packet) = &events[5] else {
            panic!("expected audio packet last, got {:?}", events[5]);
        };
        assert_eq!(packet.codec, AudioCodec::Aac);
        assert_eq!(packet.timestamp_us, 64_000);
        // The raw frame was ADTS-framed from the HELLO's ASC.
        let h = AdtsHeader::parse(&packet.data).expect("ADTS-framed AAC");
        assert_eq!(h.sample_rate, 16000);
        assert_eq!(h.channel_configuration, 1);
        assert_eq!(&packet.data[h.header_len..], &raw_aac[..]);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn g711_passes_through_unframed() {
        let dir = test_sock_dir("g711");
        let sock = dir.join("stream_4.sock");

        let g711 = vec![0xD5; 160]; // 20ms of A-law
        let mut script = Vec::new();
        script.extend_from_slice(&encode_message(
            0x01,
            1,
            0,
            0,
            0,
            0,
            &hello_payload(alaw_codec_id(), &[]),
        ));
        script.extend_from_slice(&encode_message(0x02, 1, 0, 0, 0, 1000, &g711));

        let server = spawn_fake_zmc(sock, script, false);
        let mut reader = StreamSocketReader::new(4, test_config(&dir));
        reader.connect().await.unwrap();

        let events = collect_events(&mut reader, 2).await;
        let SocketEvent::AudioParams { codec } = &events[0] else {
            panic!("expected AudioParams, got {:?}", events[0]);
        };
        assert_eq!(*codec, AudioCodec::G711Alaw);
        let SocketEvent::Audio(packet) = &events[1] else {
            panic!("expected audio packet, got {:?}", events[1]);
        };
        assert_eq!(packet.data, g711);
        assert_eq!(packet.codec, AudioCodec::G711Alaw);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bye_and_eof_surface_as_closed() {
        let dir = test_sock_dir("bye");
        let sock = dir.join("stream_5.sock");

        let mut script = Vec::new();
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            0,
            0,
            &hello_payload(h264_codec_id(), &[]),
        ));
        script.extend_from_slice(&encode_message(0x05, 0, 0, 1, 0, 0, &[])); // BYE

        let server = spawn_fake_zmc(sock.clone(), script, true);
        let mut reader = StreamSocketReader::new(5, test_config(&dir));
        reader.connect().await.unwrap();

        let ev = reader.next_event().await.expect("hello event");
        assert!(matches!(ev, SocketEvent::VideoParams { .. }));
        assert!(matches!(
            reader.next_event().await,
            Err(SourceError::Closed)
        ));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stats_and_unknown_messages_are_skipped() {
        let dir = test_sock_dir("stats");
        let sock = dir.join("stream_6.sock");

        let mut stats_payload = 100u64.to_le_bytes().to_vec();
        stats_payload.extend_from_slice(&3u64.to_le_bytes());

        let mut script = Vec::new();
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            0,
            0,
            &hello_payload(h264_codec_id(), &extradata()),
        ));
        script.extend_from_slice(&encode_message(0x04, 0, 0, 0, 0, 0, &stats_payload));
        script.extend_from_slice(&encode_message(0x7E, 0, 0, 0, 0, 0, &[1, 2, 3])); // unknown type
        script.extend_from_slice(&encode_message(0x02, 3, 0, 0, 0, 0, &[4, 5])); // unknown stream id
        script.extend_from_slice(&encode_message(
            0x02,
            0,
            FLAG_KEYFRAME,
            1,
            0,
            500,
            &idr_au(),
        ));

        let server = spawn_fake_zmc(sock, script, false);
        let mut reader = StreamSocketReader::new(6, test_config(&dir));
        reader.connect().await.unwrap();

        // STATS / unknown messages produce no events; media still flows.
        let events = collect_events(&mut reader, 4).await;
        assert!(matches!(events[0], SocketEvent::VideoParams { .. }));
        assert!(matches!(events[3], SocketEvent::Video(_)));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn command_responses_are_surfaced_and_malformed_ones_skipped() {
        let dir = test_sock_dir("responses");
        let sock = dir.join("stream_12.sock");

        let reply = br#"{"data":"","message":"dispatched","ok":true,"request_id":7}"#;
        let mut script = Vec::new();
        script.extend_from_slice(&encode_message(0x12, 2, 0, 0, 0, 0, b"garbage"));
        script.extend_from_slice(&encode_message(0x12, 2, 0, 0, 0, 0, reply));
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            0,
            0,
            &hello_payload(h264_codec_id(), &extradata()),
        ));

        let server = spawn_fake_zmc(sock, script, false);
        let mut reader = StreamSocketReader::new(12, test_config(&dir));
        reader.connect().await.expect("connect");

        let events = collect_events(&mut reader, 2).await;
        let SocketEvent::CommandResponse(resp) = &events[0] else {
            panic!("expected CommandResponse, got {:?}", events[0]);
        };
        assert_eq!(resp.request_id, 7);
        assert!(resp.ok);
        assert!(matches!(events[1], SocketEvent::VideoParams { .. }));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn monitor_events_are_surfaced_between_media() {
        let dir = test_sock_dir("events");
        let sock = dir.join("stream_10.sock");

        // HELLO, a snapshot EVENT replayed on connect, a detection EVENT, then
        // a keyframe — media and events interleave on the one connection.
        let snapshot = event_payload(EVENT_SNAPSHOT, &tlv_u16(0x07, 0));
        let detection_json = r#"{"objects":[{"label":"car","confidence":0.8}]}"#;
        let mut det_tail = tlv_u64(0x01, 1_700_000_000_000_000);
        det_tail.extend_from_slice(&tlv(0x10, detection_json.as_bytes()));
        let detection = event_payload(EVENT_DETECTION, &det_tail);

        let mut script = Vec::new();
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            0,
            0,
            &hello_payload(h264_codec_id(), &extradata()),
        ));
        // EVENT: type 0x06 on the monitor stream (id 2), own sequence counter.
        script.extend_from_slice(&encode_message(0x06, 2, 0, 0, 0, 0, &snapshot));
        script.extend_from_slice(&encode_message(0x06, 2, 0, 1, 0, 0, &detection));
        script.extend_from_slice(&encode_message(
            0x03,
            0,
            FLAG_KEYFRAME,
            5,
            0,
            9_000_000,
            &idr_au(),
        ));

        let server = spawn_fake_zmc(sock, script, false);
        let mut reader = StreamSocketReader::new(10, test_config(&dir));
        reader.connect().await.expect("connect");

        // VideoParams, snapshot event, detection event, then 3 keyframe NALs.
        let events = collect_events(&mut reader, 6).await;
        assert!(matches!(events[0], SocketEvent::VideoParams { .. }));

        let SocketEvent::MonitorEvent(snap) = &events[1] else {
            panic!("expected snapshot MonitorEvent, got {:?}", events[1]);
        };
        assert_eq!(snap.code, EVENT_SNAPSHOT);
        assert_eq!(snap.health_code, Some(0));

        let SocketEvent::MonitorEvent(det) = &events[2] else {
            panic!("expected detection MonitorEvent, got {:?}", events[2]);
        };
        assert_eq!(det.code, EVENT_DETECTION);
        assert_eq!(det.json_detail.as_deref(), Some(detection_json));

        // Media still flows after the events.
        assert!(matches!(events[3], SocketEvent::Video(_)));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn generation_bump_rebases_timestamps() {
        let dir = test_sock_dir("generation");
        let sock = dir.join("stream_7.sock");

        let mut script = Vec::new();
        // Generation 0: hello + one keyframe at pts 5s.
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            0,
            0,
            &hello_payload(h264_codec_id(), &extradata()),
        ));
        script.extend_from_slice(&encode_message(
            0x02,
            0,
            FLAG_KEYFRAME,
            0,
            0,
            5_000_000,
            &idr_au(),
        ));
        // Camera reconfigured: generation 1, fresh HELLO, pts restarts at 1s.
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            1,
            0,
            &hello_payload(h264_codec_id(), &extradata()),
        ));
        script.extend_from_slice(&encode_message(
            0x02,
            0,
            FLAG_KEYFRAME,
            0,
            1,
            1_000_000,
            &idr_au(),
        ));

        let server = spawn_fake_zmc(sock, script, false);
        let mut reader = StreamSocketReader::new(7, test_config(&dir));
        reader.connect().await.unwrap();

        // gen 0: params + 3 NALs; gen 1: params + 3 NALs.
        let events = collect_events(&mut reader, 8).await;
        let SocketEvent::Video(first_gen_idr) = &events[3] else {
            panic!("expected video at index 3");
        };
        assert_eq!(first_gen_idr.timestamp_us, 0);
        assert!(matches!(events[4], SocketEvent::VideoParams { .. }));
        let SocketEvent::Video(second_gen_idr) = &events[7] else {
            panic!("expected video at index 7");
        };
        // Re-based: the new epoch's first packet also starts at zero rather
        // than going negative/huge against the old base.
        assert_eq!(second_gen_idr.timestamp_us, 0);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn corrupt_header_is_a_protocol_error() {
        let dir = test_sock_dir("corrupt");
        let sock = dir.join("stream_8.sock");

        let mut script = encode_message(0x01, 0, 0, 0, 0, 0, &hello_payload(h264_codec_id(), &[]));
        script.extend_from_slice(&[0xFF; HEADER_SIZE]); // garbage header

        let server = spawn_fake_zmc(sock, script, false);
        let mut reader = StreamSocketReader::new(8, test_config(&dir));
        reader.connect().await.unwrap();

        assert!(matches!(
            reader.next_event().await,
            Ok(SocketEvent::VideoParams { .. })
        ));
        assert!(matches!(
            reader.next_event().await,
            Err(SourceError::Protocol(_))
        ));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eacces_on_connect_names_the_stream_socket_group() {
        // zmc creates the socket 0660; if zm-api's user isn't in
        // ZM_STREAM_SOCKET_GROUP every stream fails with a bare "permission
        // denied" that points at nothing. The error must name the fix.
        let err = classify_connect_error(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            Path::new("/run/zm/stream_1.sock"),
        );
        assert!(matches!(err, SourceError::PermissionDenied { .. }));
        let msg = err.to_string();
        assert!(msg.contains("ZM_STREAM_SOCKET_GROUP"), "got: {msg}");
        assert!(msg.contains("/run/zm/stream_1.sock"), "got: {msg}");
    }

    #[test]
    fn other_connect_errors_stay_generic_io() {
        let err = classify_connect_error(
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
            Path::new("/run/zm/stream_1.sock"),
        );
        assert!(matches!(err, SourceError::Io(_)));
    }
}
