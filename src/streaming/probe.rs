//! Probe recorded-event media for codec, dimensions and duration.
//!
//! Uses the ffmpeg libraries (libavformat/libavcodec) via `ffmpeg-next` — the
//! same approach as [`crate::streaming::snapshot`] — never the `ffmpeg`/`ffprobe`
//! binary. Results are cached per event id: a recorded file's properties are
//! immutable, so the cache never needs invalidation.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use dashmap::DashMap;

use crate::streaming::source::VideoCodec;

/// Probed properties of a recorded event's video file.
#[derive(Debug, Clone, Copy)]
pub struct MediaInfo {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub duration_seconds: f64,
}

impl MediaInfo {
    /// Whether the codec is universally playable in a browser `<video>` element.
    /// H.264 is; HEVC is not (Safari / hardware-Chrome only).
    pub fn playable_direct(&self) -> bool {
        matches!(self.codec, VideoCodec::H264)
    }
}

fn cache() -> &'static DashMap<u64, MediaInfo> {
    static CACHE: OnceLock<DashMap<u64, MediaInfo>> = OnceLock::new();
    CACHE.get_or_init(DashMap::new)
}

/// The most probe results kept. One is added for every event anyone plays
/// and none was ever dropped, so over a long uptime the cache grew with the
/// number of events ever recorded.
const MAX_CACHED_PROBES: usize = 10_000;

/// Cache `info`, starting over when the cache is full. A dropped result
/// costs one more probe of that file.
fn remember(cache: &DashMap<u64, MediaInfo>, event_id: u64, info: MediaInfo) {
    if cache.len() >= MAX_CACHED_PROBES {
        cache.clear();
    }
    cache.insert(event_id, info);
}

/// Probe an event's video file, caching the result by event id.
pub async fn probe_event_media(event_id: u64, path: PathBuf) -> Result<MediaInfo, String> {
    if let Some(info) = cache().get(&event_id) {
        return Ok(*info);
    }
    let info = tokio::task::spawn_blocking(move || probe_blocking(&path))
        .await
        .map_err(|e| format!("probe task failed: {e}"))??;
    remember(cache(), event_id, info);
    Ok(info)
}

fn probe_blocking(path: &Path) -> Result<MediaInfo, String> {
    use ffmpeg_next as ffmpeg;

    let ictx = ffmpeg::format::input(&path).map_err(|e| format!("open {path:?}: {e}"))?;
    let container_duration = ictx.duration(); // AV_TIME_BASE units, may be 0

    let stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| "no video stream".to_string())?;

    let stream_duration = stream.duration();
    let time_base = stream.time_base();

    let ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|e| format!("decoder context: {e}"))?;
    let codec = match ctx.id() {
        ffmpeg::codec::Id::H264 => VideoCodec::H264,
        ffmpeg::codec::Id::HEVC => VideoCodec::H265,
        _ => VideoCodec::Unknown,
    };
    let decoder = ctx
        .decoder()
        .video()
        .map_err(|e| format!("video decoder: {e}"))?;

    let duration_seconds = if stream_duration > 0 && time_base.denominator() != 0 {
        stream_duration as f64 * f64::from(time_base.numerator())
            / f64::from(time_base.denominator())
    } else if container_duration > 0 {
        container_duration as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE)
    } else {
        0.0
    };

    Ok(MediaInfo {
        codec,
        width: decoder.width(),
        height: decoder.height(),
        duration_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_cache_does_not_grow_without_limit() {
        let cache = DashMap::new();
        for id in 0..(MAX_CACHED_PROBES as u64 + 10) {
            remember(&cache, id, info(VideoCodec::H264));
        }
        assert!(cache.len() <= MAX_CACHED_PROBES);
        assert!(cache.contains_key(&(MAX_CACHED_PROBES as u64 + 9)));
    }

    fn info(codec: VideoCodec) -> MediaInfo {
        MediaInfo {
            codec,
            width: 1920,
            height: 1080,
            duration_seconds: 12.0,
        }
    }

    #[test]
    fn only_h264_is_directly_playable() {
        assert!(info(VideoCodec::H264).playable_direct());
        assert!(!info(VideoCodec::H265).playable_direct());
        assert!(!info(VideoCodec::Unknown).playable_direct());
    }
}
