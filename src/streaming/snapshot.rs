//! Monitor snapshot service
//!
//! Captures H.264 or H.265 keyframes from the live broadcast pipeline and
//! converts them to JPEG images using libavcodec/libswscale (via
//! `ffmpeg-next`). Per-monitor caching minimizes overhead for repeated
//! requests.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tracing::{debug, warn};

use crate::streaming::source::router::RouterError;
use crate::streaming::source::{
    h264_nal_type, h265_nal_type, SourceRouter, VideoCodec, VideoPacket,
};

/// Default cache TTL for snapshots
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(2);

/// Timeout for waiting for a keyframe from the broadcast channel
const KEYFRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// JPEG quality (2 = high quality, 31 = lowest). Maps to ffmpeg's qscale.
const JPEG_QUALITY: u32 = 2;

/// Errors from the snapshot service
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("Source not available for monitor {0}")]
    SourceNotAvailable(u32),

    #[error("Keyframe capture timed out for monitor {0}")]
    KeyframeTimeout(u32),

    #[error("Decode failed: {0}")]
    DecodeFailed(String),

    #[error("Encode failed: {0}")]
    EncodeFailed(String),

    #[error("Router error: {0}")]
    RouterError(#[from] RouterError),

    #[error("MP4 file not readable: {0}")]
    Mp4OpenFailed(String),
}

/// A cached JPEG snapshot
struct CachedSnapshot {
    jpeg: Vec<u8>,
    captured_at: Instant,
}

/// Service for capturing monitor snapshots as JPEG images
pub struct SnapshotService {
    source_router: Arc<SourceRouter>,
    cache: DashMap<u32, CachedSnapshot>,
    cache_ttl: Duration,
}

impl SnapshotService {
    /// Create a new snapshot service
    pub fn new(source_router: Arc<SourceRouter>, cache_ttl: Duration) -> Self {
        Self {
            source_router,
            cache: DashMap::new(),
            cache_ttl,
        }
    }

    /// Create with default TTL
    pub fn with_defaults(source_router: Arc<SourceRouter>) -> Self {
        Self::new(source_router, DEFAULT_CACHE_TTL)
    }

    /// Get a JPEG snapshot for a monitor, using cache when fresh
    pub async fn get_snapshot(&self, monitor_id: u32) -> Result<Vec<u8>, SnapshotError> {
        // Check cache
        if let Some(cached) = self.cache.get(&monitor_id) {
            if cached.captured_at.elapsed() < self.cache_ttl {
                debug!("Serving cached snapshot for monitor {}", monitor_id);
                return Ok(cached.jpeg.clone());
            }
        }

        // Capture fresh snapshot
        let (video_data, codec) = self.capture_keyframe(monitor_id).await?;
        let jpeg = Self::decode_to_jpeg(&video_data, codec).await?;

        // Update cache
        self.cache.insert(
            monitor_id,
            CachedSnapshot {
                jpeg: jpeg.clone(),
                captured_at: Instant::now(),
            },
        );

        Ok(jpeg)
    }

    /// Capture a keyframe access unit from the broadcast channel.
    ///
    /// Returns the assembled Annex B access unit together with its codec, so
    /// the caller can select the matching decoder.
    async fn capture_keyframe(
        &self,
        monitor_id: u32,
    ) -> Result<(Vec<u8>, VideoCodec), SnapshotError> {
        // Piggyback on an existing source when one is registered; otherwise a
        // temporary one is created and torn down below. Either way the source
        // is acquired through `get_source`, which restarts a dead reader: a
        // registered source whose reader was reaped (idle watchdog) would
        // otherwise never produce another packet, and the wait below would
        // always end in `KeyframeTimeout`.
        let created_temp = self.source_router.get_existing_source(monitor_id).is_none();
        let source = self
            .source_router
            .get_source(monitor_id)
            .await
            .map_err(|e| match e {
                RouterError::SocketNotFound(_) => SnapshotError::SourceNotAvailable(monitor_id),
                other => SnapshotError::RouterError(other),
            })?;

        let mut rx = source.subscribe_video();

        // Wait for a complete, decodable keyframe access unit with a timeout.
        let result = tokio::time::timeout(KEYFRAME_TIMEOUT, async {
            let mut collector = KeyframeCollector::new();

            loop {
                match rx.recv().await {
                    Ok(packet) => {
                        if let Some(au) = collector.push(&packet) {
                            // The codec is stable for the lifetime of a
                            // stream, so the completing packet's codec is the
                            // keyframe's codec.
                            return Ok((au, packet.codec));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            "Snapshot receiver lagged {} packets for monitor {}",
                            n, monitor_id
                        );
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(SnapshotError::SourceNotAvailable(monitor_id));
                    }
                }
            }
        })
        .await;

        // Drop our receiver and source handle *before* tearing down a temporary
        // source. `remove_source` aborts the reader and drops the map's entry,
        // but these locals are clones of the same broadcast channel / source
        // Arc — left alive until function end, they keep the channel open and
        // the MonitorSource refcount above zero, defeating the prompt teardown
        // this cleanup is meant to perform. See REVIEW_FIXES_PLAN §3.4.
        drop(rx);
        drop(source);

        // Cleanup temporary source if we created one
        if created_temp {
            let _ = self.source_router.remove_source(monitor_id).await;
        }

        match result {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(SnapshotError::KeyframeTimeout(monitor_id)),
        }
    }

    /// Convert raw Annex B NAL data to JPEG using libavcodec/libswscale.
    ///
    /// Runs on a blocking thread since ffmpeg-next is synchronous.
    async fn decode_to_jpeg(
        video_data: &[u8],
        codec: VideoCodec,
    ) -> Result<Vec<u8>, SnapshotError> {
        let data = video_data.to_vec();
        let _slot = decode_slot().await;
        tokio::task::spawn_blocking(move || Self::decode_to_jpeg_blocking(&data, codec))
            .await
            .map_err(|e| SnapshotError::DecodeFailed(format!("Task join error: {}", e)))?
    }

    /// Synchronous H.264/H.265 → JPEG conversion via libavcodec.
    fn decode_to_jpeg_blocking(
        video_data: &[u8],
        codec: VideoCodec,
    ) -> Result<Vec<u8>, SnapshotError> {
        use ffmpeg_next as ffmpeg;

        // Select the decoder matching the captured stream's codec.
        let codec_id = match codec {
            VideoCodec::H265 => ffmpeg::codec::Id::HEVC,
            VideoCodec::H264 | VideoCodec::Unknown => ffmpeg::codec::Id::H264,
        };
        let decoder_codec = ffmpeg::codec::decoder::find(codec_id).ok_or_else(|| {
            SnapshotError::DecodeFailed(format!("{} decoder not found", codec.as_str()))
        })?;

        let mut decoder_ctx = ffmpeg::codec::Context::new_with_codec(decoder_codec)
            .decoder()
            .video()
            .map_err(|e| SnapshotError::DecodeFailed(format!("Failed to open decoder: {}", e)))?;

        // Feed the raw Annex B data as a single packet
        let mut av_packet = ffmpeg::Packet::copy(video_data);
        av_packet.set_pts(Some(0));
        av_packet.set_dts(Some(0));

        decoder_ctx
            .send_packet(&av_packet)
            .map_err(|e| SnapshotError::DecodeFailed(format!("send_packet failed: {}", e)))?;
        decoder_ctx
            .send_eof()
            .map_err(|e| SnapshotError::DecodeFailed(format!("send_eof failed: {}", e)))?;

        // Receive decoded frame
        let mut decoded_frame = ffmpeg::frame::Video::empty();
        decoder_ctx
            .receive_frame(&mut decoded_frame)
            .map_err(|e| SnapshotError::DecodeFailed(format!("No frame decoded: {}", e)))?;

        if decoded_frame.width() == 0 || decoded_frame.height() == 0 {
            return Err(SnapshotError::DecodeFailed(
                "Decoded frame has zero dimensions".into(),
            ));
        }

        // Convert pixel format to YUVJ420P (MJPEG's native format)
        let target_format = ffmpeg::format::Pixel::YUVJ420P;
        let mut scaler = ffmpeg::software::scaling::Context::get(
            decoded_frame.format(),
            decoded_frame.width(),
            decoded_frame.height(),
            target_format,
            decoded_frame.width(),
            decoded_frame.height(),
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .map_err(|e| SnapshotError::EncodeFailed(format!("Scaler init failed: {}", e)))?;

        let mut yuv_frame = ffmpeg::frame::Video::empty();
        scaler
            .run(&decoded_frame, &mut yuv_frame)
            .map_err(|e| SnapshotError::EncodeFailed(format!("Scaler run failed: {}", e)))?;

        // Encode as MJPEG
        let encoder_codec = ffmpeg::codec::encoder::find(ffmpeg::codec::Id::MJPEG)
            .ok_or_else(|| SnapshotError::EncodeFailed("MJPEG encoder not found".into()))?;

        let mut encoder_ctx = ffmpeg::codec::Context::new_with_codec(encoder_codec)
            .encoder()
            .video()
            .map_err(|e| SnapshotError::EncodeFailed(format!("Failed to init encoder: {}", e)))?;

        encoder_ctx.set_width(decoded_frame.width());
        encoder_ctx.set_height(decoded_frame.height());
        encoder_ctx.set_format(target_format);
        encoder_ctx.set_time_base(ffmpeg::Rational(1, 25));
        encoder_ctx.set_quality(JPEG_QUALITY as usize);

        // Set global quality via priv_data for MJPEG (qscale maps to quality)
        let encoder_ctx = encoder_ctx.open().map_err(|e| {
            SnapshotError::EncodeFailed(format!("Failed to open MJPEG encoder: {}", e))
        })?;

        // Wrap in a new scope to encode
        let mut encoder = encoder_ctx;
        yuv_frame.set_pts(Some(0));

        encoder
            .send_frame(&yuv_frame)
            .map_err(|e| SnapshotError::EncodeFailed(format!("send_frame failed: {}", e)))?;
        encoder
            .send_eof()
            .map_err(|e| SnapshotError::EncodeFailed(format!("encoder send_eof failed: {}", e)))?;

        let mut encoded_packet = ffmpeg::Packet::empty();
        encoder
            .receive_packet(&mut encoded_packet)
            .map_err(|e| SnapshotError::EncodeFailed(format!("No JPEG packet received: {}", e)))?;

        let jpeg_data = encoded_packet
            .data()
            .ok_or_else(|| SnapshotError::EncodeFailed("Encoded packet has no data".into()))?
            .to_vec();

        if jpeg_data.is_empty() {
            return Err(SnapshotError::EncodeFailed(
                "MJPEG encoder produced empty output".into(),
            ));
        }

        Ok(jpeg_data)
    }
}

/// How many frames may be decoded at once: one per core, and at least two.
///
/// Each snapshot, thumbnail and frame request decodes video on a blocking
/// thread, and Tokio will start hundreds of those. A viewer asking for many
/// at once (a page of thumbnails, or on purpose) would have every core
/// decoding and nothing left for capture or the API. Requests past the limit
/// wait their turn.
fn decode_limit() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .max(2)
}

static DECODE_SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();

/// Wait for a place to decode. Dropped when the decode is done.
async fn decode_slot() -> tokio::sync::SemaphorePermit<'static> {
    DECODE_SLOTS
        .get_or_init(|| tokio::sync::Semaphore::new(decode_limit()))
        .acquire()
        .await
        .expect("the decode semaphore is never closed")
}

/// Extract the first decodable video frame from an MP4 and encode it as JPEG.
///
/// Async wrapper around the blocking libav pipeline; runs on a blocking thread
/// so the executor isn't tied up while the decoder works. `max_width` clamps
/// the output JPEG's width (aspect ratio preserved); a frame already smaller
/// than `max_width` is passed through at its native size.
pub async fn extract_mp4_thumbnail(
    path: PathBuf,
    max_width: u32,
) -> Result<Vec<u8>, SnapshotError> {
    let _slot = decode_slot().await;
    tokio::task::spawn_blocking(move || extract_mp4_thumbnail_blocking(&path, None, max_width))
        .await
        .map_err(|e| SnapshotError::DecodeFailed(format!("Task join error: {}", e)))?
}

/// Extract the frame shown `seconds` into an MP4 and encode it as JPEG.
///
/// Seeks to the keyframe at or before `seconds`, then decodes forward to the
/// first frame at or past it. A time past the end yields the last frame.
pub async fn extract_mp4_frame_at(
    path: PathBuf,
    seconds: f64,
    max_width: u32,
) -> Result<Vec<u8>, SnapshotError> {
    let _slot = decode_slot().await;
    tokio::task::spawn_blocking(move || {
        extract_mp4_thumbnail_blocking(&path, Some(seconds.max(0.0)), max_width)
    })
    .await
    .map_err(|e| SnapshotError::DecodeFailed(format!("Task join error: {}", e)))?
}

/// Blocking MP4 → JPEG keyframe extraction via libavformat + libavcodec.
///
/// Opens the container, picks the best video stream, builds a decoder from its
/// codec parameters, feeds packets until the first frame comes out, then scales
/// to `max_width` (preserving aspect) and MJPEG-encodes the result. This is
/// significantly cheaper than spawning the ffmpeg binary because libav* is
/// already loaded into the process — no fork, no per-call RSS spike, no moov
/// re-parse for every request.
///
/// With `at` set, seeks there first and decodes forward until a frame's
/// timestamp reaches it (see [`extract_mp4_frame_at`]).
fn extract_mp4_thumbnail_blocking(
    path: &Path,
    at: Option<f64>,
    max_width: u32,
) -> Result<Vec<u8>, SnapshotError> {
    use ffmpeg_next as ffmpeg;

    let mut ictx = ffmpeg::format::input(&path)
        .map_err(|e| SnapshotError::Mp4OpenFailed(format!("Failed to open {:?}: {}", path, e)))?;

    let video_stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| SnapshotError::DecodeFailed("No video stream in MP4".into()))?;
    let stream_index = video_stream.index();
    let time_base = f64::from(video_stream.time_base());
    let start_pts = video_stream.start_time();

    let decoder_ctx = ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())
        .map_err(|e| {
        SnapshotError::DecodeFailed(format!("Failed to build decoder context: {}", e))
    })?;
    let mut decoder = decoder_ctx
        .decoder()
        .video()
        .map_err(|e| SnapshotError::DecodeFailed(format!("Failed to init video decoder: {}", e)))?;

    // Feed packets until the decoder yields a frame. For ZoneMinder MP4s the
    // first video packet is an IDR, so this usually returns after one
    // send_packet/receive_frame round-trip — but bounded just in case.
    let mut decoded = ffmpeg::frame::Video::empty();
    let mut got_frame = false;
    let mut packets_consumed = 0usize;
    // A seek lands on the keyframe before the target, so allow a full GOP of
    // decoding after it; the first-frame case needs only a few packets.
    let max_packets: usize = if at.is_some() { 1_000 } else { 16 };

    if let Some(seconds) = at {
        // Container-level seek: timestamps in AV_TIME_BASE (microseconds).
        let ts = (seconds * 1_000_000.0) as i64;
        ictx.seek(ts, ..ts + 1)
            .map_err(|e| SnapshotError::DecodeFailed(format!("seek failed: {}", e)))?;
    }
    // A frame's position in seconds from the start of the stream.
    let frame_secs = |frame: &ffmpeg::frame::Video| {
        frame
            .timestamp()
            .or(frame.pts())
            .map(|pts| (pts - start_pts.max(0)) as f64 * time_base)
    };

    'packets: for (pkt_stream, packet) in ictx.packets() {
        if pkt_stream.index() != stream_index {
            continue;
        }
        decoder
            .send_packet(&packet)
            .map_err(|e| SnapshotError::DecodeFailed(format!("send_packet failed: {}", e)))?;
        let mut frame = ffmpeg::frame::Video::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            let reached = match (at, frame_secs(&frame)) {
                (Some(target), Some(secs)) => secs + 1e-3 >= target,
                _ => true,
            };
            decoded = frame.clone();
            got_frame = true;
            if reached {
                break 'packets;
            }
        }
        packets_consumed += 1;
        if packets_consumed >= max_packets {
            break;
        }
    }

    if !got_frame {
        // Flush — some decoders need EOF before the very first frame surfaces.
        decoder
            .send_eof()
            .map_err(|e| SnapshotError::DecodeFailed(format!("send_eof failed: {}", e)))?;
        decoder
            .receive_frame(&mut decoded)
            .map_err(|e| SnapshotError::DecodeFailed(format!("No frame decoded: {}", e)))?;
    }

    if decoded.width() == 0 || decoded.height() == 0 {
        return Err(SnapshotError::DecodeFailed(
            "Decoded frame has zero dimensions".into(),
        ));
    }

    let src_w = decoded.width();
    let src_h = decoded.height();
    let (dst_w, dst_h) = if src_w <= max_width {
        (src_w, src_h)
    } else {
        let scaled_h = ((src_h as u64) * (max_width as u64) / (src_w as u64)) as u32;
        // libswscale needs even dimensions for YUV420-family targets.
        (max_width & !1, scaled_h.max(2) & !1)
    };

    let target_format = ffmpeg::format::Pixel::YUVJ420P;
    let mut scaler = ffmpeg::software::scaling::Context::get(
        decoded.format(),
        src_w,
        src_h,
        target_format,
        dst_w,
        dst_h,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )
    .map_err(|e| SnapshotError::EncodeFailed(format!("Scaler init failed: {}", e)))?;

    let mut yuv_frame = ffmpeg::frame::Video::empty();
    scaler
        .run(&decoded, &mut yuv_frame)
        .map_err(|e| SnapshotError::EncodeFailed(format!("Scaler run failed: {}", e)))?;

    let encoder_codec = ffmpeg::codec::encoder::find(ffmpeg::codec::Id::MJPEG)
        .ok_or_else(|| SnapshotError::EncodeFailed("MJPEG encoder not found".into()))?;

    let mut encoder_ctx = ffmpeg::codec::Context::new_with_codec(encoder_codec)
        .encoder()
        .video()
        .map_err(|e| SnapshotError::EncodeFailed(format!("Failed to init encoder: {}", e)))?;

    encoder_ctx.set_width(dst_w);
    encoder_ctx.set_height(dst_h);
    encoder_ctx.set_format(target_format);
    encoder_ctx.set_time_base(ffmpeg::Rational(1, 25));
    encoder_ctx.set_quality(JPEG_QUALITY as usize);

    let mut encoder = encoder_ctx
        .open()
        .map_err(|e| SnapshotError::EncodeFailed(format!("Failed to open MJPEG encoder: {}", e)))?;

    yuv_frame.set_pts(Some(0));
    encoder
        .send_frame(&yuv_frame)
        .map_err(|e| SnapshotError::EncodeFailed(format!("send_frame failed: {}", e)))?;
    encoder
        .send_eof()
        .map_err(|e| SnapshotError::EncodeFailed(format!("encoder send_eof failed: {}", e)))?;

    let mut encoded_packet = ffmpeg::Packet::empty();
    encoder
        .receive_packet(&mut encoded_packet)
        .map_err(|e| SnapshotError::EncodeFailed(format!("No JPEG packet received: {}", e)))?;

    let jpeg = encoded_packet
        .data()
        .ok_or_else(|| SnapshotError::EncodeFailed("Encoded packet has no data".into()))?
        .to_vec();

    if jpeg.is_empty() {
        return Err(SnapshotError::EncodeFailed(
            "MJPEG encoder produced empty output".into(),
        ));
    }

    Ok(jpeg)
}

/// A codec parameter-set NAL unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParameterSet {
    /// Video Parameter Set — H.265 only.
    Vps,
    /// Sequence Parameter Set.
    Sps,
    /// Picture Parameter Set.
    Pps,
}

/// Classify a packet as a codec parameter set, if it is one.
///
/// H.264 carries SPS (NAL type 7) and PPS (8); H.265 additionally carries a
/// VPS (32), with SPS = 33 and PPS = 34. An `Unknown` codec is parsed as
/// H.264 — the socket HELLO resolves the codec, so a real
/// stream's packets always carry a concrete codec by the time one matters.
fn parameter_set_kind(packet: &VideoPacket) -> Option<ParameterSet> {
    match packet.codec {
        VideoCodec::H264 | VideoCodec::Unknown => match h264_nal_type(&packet.data) {
            Some(7) => Some(ParameterSet::Sps),
            Some(8) => Some(ParameterSet::Pps),
            _ => None,
        },
        VideoCodec::H265 => match h265_nal_type(&packet.data) {
            Some(32) => Some(ParameterSet::Vps),
            Some(33) => Some(ParameterSet::Sps),
            Some(34) => Some(ParameterSet::Pps),
            _ => None,
        },
    }
}

/// Assembles a complete, decodable keyframe access unit from the individual
/// NAL units delivered by the source broadcast channel.
///
/// ZoneMinder emits each NAL — every parameter set and every keyframe slice —
/// as its own packet. Only the keyframe slices carry `is_keyframe`; the
/// parameter sets do not. A decoder cannot decode a keyframe slice without
/// the parameter sets it references, so a collector that started at the first
/// `is_keyframe` packet would hand the decoder a parameter-set-less access
/// unit and `send_packet` would reject it. This collector instead remembers
/// the most recent parameter sets — SPS/PPS for H.264, plus the VPS for
/// H.265 — and prepends them to the keyframe.
struct KeyframeCollector {
    /// Most recently seen VPS NAL (H.265 only), if any.
    vps: Option<Vec<u8>>,
    /// Most recently seen SPS NAL (with start code), if any.
    sps: Option<Vec<u8>>,
    /// Most recently seen PPS NAL (with start code), if any.
    pps: Option<Vec<u8>>,
    /// The keyframe access unit being assembled: the parameter sets followed
    /// by every keyframe slice sharing the keyframe's timestamp.
    keyframe: Option<Vec<u8>>,
    /// Timestamp of the keyframe access unit, used to detect its end.
    keyframe_ts: Option<i64>,
}

impl KeyframeCollector {
    fn new() -> Self {
        Self {
            vps: None,
            sps: None,
            pps: None,
            keyframe: None,
            keyframe_ts: None,
        }
    }

    /// Feed one broadcast packet. Returns `Some(au)` once a full keyframe
    /// access unit has been assembled — signalled by a later packet whose
    /// timestamp has advanced past the keyframe's.
    fn push(&mut self, packet: &VideoPacket) -> Option<Vec<u8>> {
        // Parameter sets arrive as their own NAL units ahead of the keyframe
        // and are not flagged `is_keyframe`. Keep the latest of each so they
        // can be prepended — a keyframe slice is undecodable without them.
        match parameter_set_kind(packet) {
            Some(ParameterSet::Vps) => {
                self.vps = Some(packet.data.clone());
                return None;
            }
            Some(ParameterSet::Sps) => {
                self.sps = Some(packet.data.clone());
                return None;
            }
            Some(ParameterSet::Pps) => {
                self.pps = Some(packet.data.clone());
                return None;
            }
            None => {}
        }

        match self.keyframe_ts {
            // Still inside the keyframe access unit: every NAL sharing its
            // timestamp is another slice of the same coded picture.
            Some(ts) if packet.timestamp_us == ts => {
                if let Some(kf) = &mut self.keyframe {
                    kf.extend_from_slice(&packet.data);
                }
                None
            }
            // A later timestamp means the keyframe access unit is complete.
            Some(_) => self.keyframe.take(),
            // No keyframe yet — start one when a keyframe slice arrives,
            // prefixed with the parameter sets the decoder needs. The order
            // VPS → SPS → PPS is the order a decoder expects.
            None => {
                if packet.is_keyframe {
                    let mut au = Vec::new();
                    for ps in [&self.vps, &self.sps, &self.pps].into_iter().flatten() {
                        au.extend_from_slice(ps);
                    }
                    au.extend_from_slice(&packet.data);
                    self.keyframe = Some(au);
                    self.keyframe_ts = Some(packet.timestamp_us);
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes past the limit wait; they are not all started at once.
    #[tokio::test]
    async fn decodes_are_limited_to_the_number_of_cores() {
        assert!(decode_limit() >= 2);
        let held: Vec<_> =
            futures_util::future::join_all((0..decode_limit()).map(|_| decode_slot())).await;
        // Every place is taken, so one more has to wait.
        let waiting =
            tokio::time::timeout(std::time::Duration::from_millis(50), decode_slot()).await;
        assert!(waiting.is_err(), "a decode past the limit must wait");
        drop(held);
        // And gets its turn once one finishes.
        let turn = tokio::time::timeout(std::time::Duration::from_secs(5), decode_slot()).await;
        assert!(turn.is_ok());
    }

    /// Build a single-NAL `VideoPacket`. `nal_type` is the raw NAL header byte
    /// (lower 5 bits select the type: 7 = SPS, 8 = PPS, 5 = IDR slice).
    fn nal_packet(nal_type: u8, ts: i64, is_keyframe: bool) -> VideoPacket {
        VideoPacket {
            monitor_id: 1,
            timestamp_us: ts,
            data: vec![0x00, 0x00, 0x00, 0x01, nal_type, 0xAA, 0xBB],
            is_keyframe,
            codec: VideoCodec::H264,
        }
    }

    /// Count Annex B start codes — i.e. the number of NAL units in a buffer.
    fn nal_count(au: &[u8]) -> usize {
        au.windows(4).filter(|w| w == &[0, 0, 0, 1]).count()
    }

    #[test]
    fn test_keyframe_collector_prepends_sps_pps() {
        // The regression: ZoneMinder emits SPS and PPS as their own NAL units
        // ahead of the IDR, and neither is flagged `is_keyframe`. The
        // assembled access unit must still carry them — without the parameter
        // sets it references, the IDR slice is undecodable and `send_packet`
        // rejects it.
        let mut c = KeyframeCollector::new();
        assert!(c.push(&nal_packet(0x67, 100, false)).is_none()); // SPS
        assert!(c.push(&nal_packet(0x68, 100, false)).is_none()); // PPS
        assert!(c.push(&nal_packet(0x65, 100, true)).is_none()); // IDR slice
                                                                 // The next picture's packet completes the keyframe access unit.
        let au = c
            .push(&nal_packet(0x41, 200, false))
            .expect("keyframe access unit expected");
        assert_eq!(nal_count(&au), 3, "AU must hold SPS + PPS + IDR");
        assert_eq!(h264_nal_type(&au), Some(7), "AU must begin with the SPS");
    }

    #[test]
    fn test_keyframe_collector_collects_multi_slice_idr() {
        // A 4K IDR spans several slice NALs sharing one timestamp; every one
        // of them belongs in the keyframe access unit.
        let mut c = KeyframeCollector::new();
        c.push(&nal_packet(0x67, 100, false));
        c.push(&nal_packet(0x68, 100, false));
        c.push(&nal_packet(0x65, 100, true));
        c.push(&nal_packet(0x65, 100, true));
        c.push(&nal_packet(0x65, 100, true));
        let au = c
            .push(&nal_packet(0x41, 200, false))
            .expect("keyframe access unit expected");
        assert_eq!(nal_count(&au), 5, "AU must hold SPS + PPS + 3 IDR slices");
    }

    #[test]
    fn test_keyframe_collector_ignores_packets_before_keyframe() {
        // P-frames arriving before the first keyframe are discarded.
        let mut c = KeyframeCollector::new();
        assert!(c.push(&nal_packet(0x41, 100, false)).is_none());
        assert!(c.push(&nal_packet(0x41, 200, false)).is_none());
        c.push(&nal_packet(0x67, 300, false));
        c.push(&nal_packet(0x68, 300, false));
        c.push(&nal_packet(0x65, 300, true));
        let au = c
            .push(&nal_packet(0x41, 400, false))
            .expect("keyframe access unit expected");
        assert_eq!(nal_count(&au), 3);
    }

    #[test]
    fn test_keyframe_collector_no_keyframe_returns_none() {
        // A stream of P-frames alone never yields an access unit.
        let mut c = KeyframeCollector::new();
        for ts in 0..10 {
            assert!(c.push(&nal_packet(0x41, ts, false)).is_none());
        }
    }

    /// Build a single-NAL H.265 `VideoPacket`. `nal_type` is the 6-bit HEVC
    /// NAL type (32 = VPS, 33 = SPS, 34 = PPS, 19 = IDR_W_RADL, 1 = non-IRAP).
    fn h265_packet(nal_type: u8, ts: i64, is_keyframe: bool) -> VideoPacket {
        // HEVC's two-byte NAL header carries the type in bits 1–6 of byte 0.
        let header0 = (nal_type << 1) & 0x7E;
        VideoPacket {
            monitor_id: 1,
            timestamp_us: ts,
            data: vec![0x00, 0x00, 0x00, 0x01, header0, 0x01, 0xAA, 0xBB],
            is_keyframe,
            codec: VideoCodec::H265,
        }
    }

    #[test]
    fn test_keyframe_collector_h265_prepends_vps_sps_pps() {
        // H.265 needs three parameter sets — VPS, SPS, PPS — ahead of the
        // IDR, and none of them is flagged `is_keyframe`.
        let mut c = KeyframeCollector::new();
        assert!(c.push(&h265_packet(32, 100, false)).is_none()); // VPS
        assert!(c.push(&h265_packet(33, 100, false)).is_none()); // SPS
        assert!(c.push(&h265_packet(34, 100, false)).is_none()); // PPS
        assert!(c.push(&h265_packet(19, 100, true)).is_none()); // IDR_W_RADL
        let au = c
            .push(&h265_packet(1, 200, false))
            .expect("keyframe access unit expected");
        assert_eq!(nal_count(&au), 4, "AU must hold VPS + SPS + PPS + IDR");
        assert_eq!(h265_nal_type(&au), Some(32), "AU must begin with the VPS");
    }

    #[test]
    fn test_keyframe_collector_h265_multi_slice_idr() {
        // A multi-slice H.265 IDR: every slice shares the keyframe timestamp.
        let mut c = KeyframeCollector::new();
        c.push(&h265_packet(32, 100, false));
        c.push(&h265_packet(33, 100, false));
        c.push(&h265_packet(34, 100, false));
        c.push(&h265_packet(19, 100, true));
        c.push(&h265_packet(19, 100, true));
        let au = c
            .push(&h265_packet(1, 200, false))
            .expect("keyframe access unit expected");
        assert_eq!(
            nal_count(&au),
            5,
            "AU must hold VPS + SPS + PPS + 2 IDR slices"
        );
    }

    /// Encode a single blank 64×64 keyframe with the named libavcodec encoder
    /// and return the Annex B bitstream (parameter sets inline, since no
    /// `GLOBAL_HEADER` flag is set). Returns `None` when the encoder is not
    /// available in this ffmpeg build, so callers can skip gracefully.
    fn generate_test_video(encoder_name: &str) -> Option<Vec<u8>> {
        use ffmpeg_next as ffmpeg;

        let encoder_codec = ffmpeg::codec::encoder::find_by_name(encoder_name)?;
        let mut encoder_ctx = ffmpeg::codec::Context::new_with_codec(encoder_codec)
            .encoder()
            .video()
            .ok()?;

        encoder_ctx.set_width(64);
        encoder_ctx.set_height(64);
        encoder_ctx.set_format(ffmpeg::format::Pixel::YUV420P);
        encoder_ctx.set_time_base(ffmpeg::Rational(1, 25));
        encoder_ctx.set_gop(10);

        let mut opts = ffmpeg::Dictionary::new();
        // `ultrafast` is a valid preset for both libx264 and libx265.
        opts.set("preset", "ultrafast");

        let mut encoder = encoder_ctx.open_with(opts).ok()?;

        // Create a blank frame
        let mut frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, 64, 64);
        // Fill Y plane with black (0), U/V with 128 (neutral)
        for plane in 0..3u32 {
            let data = frame.data_mut(plane as usize);
            let fill_val = if plane == 0 { 0u8 } else { 128u8 };
            data.fill(fill_val);
        }
        frame.set_pts(Some(0));

        encoder.send_frame(&frame).ok()?;
        encoder.send_eof().ok()?;

        // Collect all encoded packets — without GLOBAL_HEADER flag,
        // parameter sets are inline in the first packet's Annex B stream.
        let mut bytes = Vec::new();

        let mut packet = ffmpeg::Packet::empty();
        while encoder.receive_packet(&mut packet).is_ok() {
            if let Some(data) = packet.data() {
                bytes.extend_from_slice(data);
            }
        }

        if bytes.is_empty() {
            None
        } else {
            Some(bytes)
        }
    }

    #[tokio::test]
    async fn test_decode_to_jpeg_with_valid_h264() {
        ffmpeg_next::init().ok();

        let h264_data = match generate_test_video("libx264") {
            Some(data) => data,
            None => {
                eprintln!("Skipping test: libx264 encoder not available");
                return;
            }
        };

        let jpeg = SnapshotService::decode_to_jpeg(&h264_data, VideoCodec::H264).await;
        match jpeg {
            Ok(data) => {
                assert!(data.len() > 2, "JPEG should have content");
                assert_eq!(data[0], 0xFF, "JPEG should start with SOI marker byte 1");
                assert_eq!(data[1], 0xD8, "JPEG should start with SOI marker byte 2");
            }
            Err(e) => {
                panic!("decode_to_jpeg failed: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_decode_to_jpeg_h265() {
        ffmpeg_next::init().ok();

        let h265_data = match generate_test_video("libx265") {
            Some(data) => data,
            None => {
                eprintln!("Skipping test: libx265 encoder not available");
                return;
            }
        };

        let jpeg = SnapshotService::decode_to_jpeg(&h265_data, VideoCodec::H265).await;
        match jpeg {
            Ok(data) => {
                assert!(data.len() > 2, "JPEG should have content");
                assert_eq!(&data[..2], &[0xFF, 0xD8], "JPEG must start with SOI");
            }
            Err(e) => {
                panic!("decode_to_jpeg (H.265) failed: {}", e);
            }
        }
    }

    /// Regression: the idle reaper (`stop_reader`) aborts a session's reader
    /// but leaves the `MonitorSource` registered. A later snapshot that
    /// piggybacks on that stale source must restart the reader instead of
    /// subscribing to a producer-less broadcast channel and timing out.
    #[tokio::test]
    async fn test_capture_keyframe_revives_reaped_reader() {
        use crate::configure::streaming::ZoneMinderConfig;
        use crate::streaming::source::protocol::test_encode::*;
        use crate::streaming::source::protocol::FLAG_KEYFRAME;
        use crate::streaming::source::router::RouterConfig;
        use crate::streaming::source::stream_socket::test_support::*;

        let dir = test_sock_dir("snap_revive");

        // Fake zmc connect script: HELLO carrying SPS+PPS extradata, the
        // cached KEYFRAME (reader prepends the parameter sets), then a
        // P-slice at a later pts — the packet that lets the collector close
        // the keyframe access unit.
        let sps = [0x00, 0x00, 0x00, 0x01, 0x67, 0x4D, 0x00, 0x33];
        let pps = [0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80];
        let mut extradata = sps.to_vec();
        extradata.extend_from_slice(&pps);
        let idr = [0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00];
        let p_slice = [0x00, 0x00, 0x00, 0x01, 0x41, 0x9A, 0x21];

        let mut script = Vec::new();
        script.extend_from_slice(&encode_message(
            0x01,
            0,
            0,
            0,
            0,
            0,
            &hello_payload(h264_codec_id(), &extradata),
        ));
        script.extend_from_slice(&encode_message(
            0x03,
            0,
            FLAG_KEYFRAME,
            0,
            0,
            1_000_000,
            &idr,
        ));
        script.extend_from_slice(&encode_message(0x02, 0, 0, 1, 0, 1_100_000, &p_slice));
        let server = spawn_fake_zmc(dir.join("stream_42.sock"), script, false);

        let config = RouterConfig {
            zoneminder: ZoneMinderConfig {
                socks_path: dir.to_string_lossy().into_owned(),
                ..ZoneMinderConfig::default()
            },
            ..RouterConfig::default()
        };
        let router = Arc::new(SourceRouter::with_config(config));

        // A live session ran and was idle-reaped: the source stays registered
        // while its reader task is aborted.
        router.get_source(42).await.expect("get_source");
        router.stop_reader(42).await.expect("stop_reader");

        let service = SnapshotService::with_defaults(Arc::clone(&router));
        let (au, codec) = service
            .capture_keyframe(42)
            .await
            .expect("snapshot must revive the reaped reader, not time out");
        assert_eq!(codec, VideoCodec::H264);
        assert!(
            au.windows(5).any(|w| w == [0x00, 0x00, 0x00, 0x01, 0x65]),
            "assembled AU must contain the IDR slice"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_decode_to_jpeg_with_invalid_data() {
        ffmpeg_next::init().ok();

        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let result = SnapshotService::decode_to_jpeg(&garbage, VideoCodec::H264).await;
        assert!(result.is_err(), "Should fail on invalid H.264 data");
    }

    #[tokio::test]
    async fn test_cache_ttl() {
        use crate::configure::streaming::ZoneMinderConfig;
        use crate::streaming::source::router::RouterConfig;

        let config = RouterConfig {
            zoneminder: ZoneMinderConfig {
                socks_path: "/tmp/zm_snapshot_test_nonexistent".to_string(),
                ..ZoneMinderConfig::default()
            },
            ..RouterConfig::default()
        };
        let router = Arc::new(SourceRouter::with_config(config));
        let service = SnapshotService::new(Arc::clone(&router), Duration::from_millis(100));

        // Manually insert a cached snapshot
        service.cache.insert(
            1,
            CachedSnapshot {
                jpeg: vec![0xFF, 0xD8, 0xFF, 0xE0], // fake JPEG SOI
                captured_at: Instant::now(),
            },
        );

        // Should return cached value
        let result = service.get_snapshot(1).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![0xFF, 0xD8, 0xFF, 0xE0]);

        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(150)).await;

        // After TTL, cache miss triggers capture which will fail (no socket)
        let result = service.get_snapshot(1).await;
        assert!(result.is_err());
    }

    /// Build a small MP4 fixture by shelling out to the system `ffmpeg`
    /// binary. Returns `None` when the binary is missing — the test using
    /// this helper then skips, matching the pattern of the encoder tests.
    fn generate_test_mp4(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let out = dir.join("thumbnail-fixture.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=320x240:rate=5",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
            ])
            .arg(&out)
            .status()
            .ok()?;
        if status.success() && out.exists() {
            Some(out)
        } else {
            None
        }
    }

    #[tokio::test]
    async fn test_extract_mp4_thumbnail_returns_jpeg() {
        ffmpeg_next::init().ok();

        let tmp = match tempfile::tempdir() {
            Ok(t) => t,
            Err(_) => return,
        };
        let mp4 = match generate_test_mp4(tmp.path()) {
            Some(p) => p,
            None => {
                eprintln!("Skipping test: ffmpeg binary not available to build fixture");
                return;
            }
        };

        let jpeg = extract_mp4_thumbnail(mp4, 160)
            .await
            .expect("thumbnail extraction must succeed for valid MP4");

        assert!(jpeg.len() > 4, "JPEG should have content");
        assert_eq!(
            &jpeg[..2],
            &[0xFF, 0xD8],
            "Output must begin with the JPEG SOI marker"
        );
        // EOI marker is the last two bytes of a well-formed JPEG.
        assert_eq!(
            &jpeg[jpeg.len() - 2..],
            &[0xFF, 0xD9],
            "Output must end with the JPEG EOI marker"
        );
    }

    /// Average luma of a JPEG, 0–255.
    fn mean_luma(jpeg: &[u8]) -> f64 {
        let img = image::load_from_memory(jpeg)
            .expect("decode jpeg")
            .to_luma8();
        img.pixels().map(|p| f64::from(p.0[0])).sum::<f64>() / f64::from(img.width() * img.height())
    }

    #[tokio::test]
    async fn extract_mp4_frame_at_returns_the_frame_at_that_time() {
        ffmpeg_next::init().ok();
        let Ok(tmp) = tempfile::tempdir() else { return };
        // Two seconds at 10fps with a 2s GOP: black, then white. Reaching the
        // white half means decoding forward from the only keyframe.
        let out = tmp.path().join("black-white.mp4");
        let ok = std::process::Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=64x64:r=10:d=1[b];color=c=white:s=64x64:r=10:d=1[w];[b][w]concat=n=2:v=1")
            .args(["-c:v", "libx264", "-g", "20", "-pix_fmt", "yuv420p"])
            .arg(&out)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("Skipping test: ffmpeg binary not available to build fixture");
            return;
        }

        let early = extract_mp4_frame_at(out.clone(), 0.2, 64)
            .await
            .expect("frame at 0.2s");
        let late = extract_mp4_frame_at(out.clone(), 1.5, 64)
            .await
            .expect("frame at 1.5s");
        let past_end = extract_mp4_frame_at(out, 60.0, 64)
            .await
            .expect("frame past end");
        assert!(mean_luma(&early) < 40.0, "0.2s should be black");
        assert!(mean_luma(&late) > 215.0, "1.5s should be white");
        assert!(
            mean_luma(&past_end) > 215.0,
            "past the end gives the last frame"
        );
    }

    #[tokio::test]
    async fn test_extract_mp4_thumbnail_missing_file() {
        ffmpeg_next::init().ok();

        let err = extract_mp4_thumbnail(
            std::path::PathBuf::from("/nonexistent/path/does-not-exist.mp4"),
            160,
        )
        .await
        .expect_err("Should fail when MP4 cannot be opened");

        assert!(
            matches!(err, SnapshotError::Mp4OpenFailed(_)),
            "Expected Mp4OpenFailed, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_cache_miss_triggers_capture() {
        use crate::configure::streaming::ZoneMinderConfig;
        use crate::streaming::source::router::RouterConfig;

        let config = RouterConfig {
            zoneminder: ZoneMinderConfig {
                socks_path: "/tmp/zm_snapshot_test_nonexistent2".to_string(),
                ..ZoneMinderConfig::default()
            },
            ..RouterConfig::default()
        };
        let router = Arc::new(SourceRouter::with_config(config));
        let service = SnapshotService::with_defaults(Arc::clone(&router));

        // Fresh monitor with no cache — should fail because the stream socket doesn't exist
        let result = service.get_snapshot(999).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SnapshotError::SourceNotAvailable(id) => assert_eq!(id, 999),
            SnapshotError::RouterError(RouterError::SocketNotFound(id)) => assert_eq!(id, 999),
            other => panic!("Unexpected error: {:?}", other),
        }
    }
}
