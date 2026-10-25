//! A stand-in for zmc's per-monitor stream socket (#236).
//!
//! Integration tests otherwise run with live view switched off, so they only
//! reach the "not configured" answers. [`FakeZmc`] listens on
//! `stream_{id}.sock` in a directory of its own and, for each client, sends a
//! video HELLO followed by real H.264 access units at 10 fps, in a loop: a
//! 320×240 clip from `tests/fixtures/` with a keyframe every second. Frames
//! go out as zmc sends them (`docs/stream_socket.rst` in ZoneMinder):
//! KEYFRAME messages for IDR access units, MEDIA for the rest.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

const CLIP: &[u8] = include_bytes!("../fixtures/testsrc-320x240-10fps.h264");
const FRAME_INTERVAL: Duration = Duration::from_millis(100);

pub const WIDTH: u32 = 320;
pub const HEIGHT: u32 = 240;

/// A fake zmc serving one monitor's stream socket. Dropping it stops the
/// server and removes its directory.
pub struct FakeZmc {
    dir: PathBuf,
    task: JoinHandle<()>,
}

impl FakeZmc {
    /// Serve `monitor_id`'s stream socket from a new directory.
    pub fn start(monitor_id: u32) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zmapi-fakezmc-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fake zmc directory");
        let listener = UnixListener::bind(dir.join(format!("stream_{monitor_id}.sock")))
            .expect("bind fake stream socket");

        let (extradata, frames) = split_clip(CLIP);
        let hello = hello_payload(&extradata);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let hello = hello.clone();
                let frames = frames.clone();
                tokio::spawn(async move {
                    if stream
                        .write_all(&message(HELLO, 0, 0, 0, &hello))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let mut n: u32 = 0;
                    loop {
                        for (au, keyframe) in &frames {
                            let (kind, flags) = if *keyframe {
                                (KEYFRAME, FLAG_KEYFRAME)
                            } else {
                                (MEDIA, 0)
                            };
                            let pts = i64::from(n) * FRAME_INTERVAL.as_micros() as i64;
                            if stream
                                .write_all(&message(kind, flags, n, pts, au))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            n += 1;
                            tokio::time::sleep(FRAME_INTERVAL).await;
                        }
                    }
                });
            }
        });
        Self { dir, task }
    }

    /// The directory to use as `ZM_PATH_SOCKS`.
    pub fn socks_dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for FakeZmc {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const HELLO: u8 = 0x01;
const MEDIA: u8 = 0x02;
const KEYFRAME: u8 = 0x03;
const FLAG_KEYFRAME: u8 = 0x01;
const VERSION: u8 = 1;
/// Header bytes after the length field: version, type, stream, flags,
/// sequence, generation, pts.
const HEADER_AFTER_LENGTH: u32 = 20;

/// One message on stream 0 (video), generation 1.
fn message(kind: u8, flags: u8, sequence: u32, pts_us: i64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(24 + payload.len());
    out.extend_from_slice(&(HEADER_AFTER_LENGTH + payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&[VERSION, kind, 0, flags]);
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(pts_us as u64).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn tlv(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(value);
    out
}

/// A video HELLO as zmc builds it: codec id and name, the SPS and PPS as
/// extradata, size and frame rate.
fn hello_payload(extradata: &[u8]) -> Vec<u8> {
    let h264 = ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_H264 as u32;
    [
        tlv(0x01, &h264.to_le_bytes()),
        tlv(0x0B, b"h264"),
        tlv(0x02, extradata),
        tlv(0x03, &WIDTH.to_le_bytes()),
        tlv(0x04, &HEIGHT.to_le_bytes()),
        tlv(0x05, &10u32.to_le_bytes()),
        tlv(0x06, &1u32.to_le_bytes()),
    ]
    .concat()
}

/// The clip's SPS and PPS (Annex B) and its access units, each with whether
/// it holds an IDR slice. x264 writes one slice per picture here, so every
/// VCL NAL closes an access unit; parameter sets and SEI before it belong to
/// it.
fn split_clip(clip: &[u8]) -> (Vec<u8>, Vec<(Vec<u8>, bool)>) {
    let mut extradata = Vec::new();
    let mut frames = Vec::new();
    let mut current = Vec::new();
    let mut keyframe = false;
    for nal in nals(clip) {
        let kind = nal[0] & 0x1f;
        if (kind == 7 || kind == 8) && frames.is_empty() {
            extradata.extend_from_slice(&[0, 0, 0, 1]);
            extradata.extend_from_slice(nal);
        }
        current.extend_from_slice(&[0, 0, 0, 1]);
        current.extend_from_slice(nal);
        keyframe |= kind == 5;
        if kind == 1 || kind == 5 {
            frames.push((std::mem::take(&mut current), keyframe));
            keyframe = false;
        }
    }
    assert!(
        frames.first().is_some_and(|f| f.1),
        "clip starts with an IDR"
    );
    (extradata, frames)
}

/// The NAL units of an Annex B stream, without start codes.
fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut end = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
            // A four-byte start code leaves its leading zero on this NAL.
            while end > s && data[end - 1] == 0 {
                end -= 1;
            }
            &data[s..end]
        })
        .collect()
}
