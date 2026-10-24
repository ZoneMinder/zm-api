//! The server against a fake monitor, played by `retina` (a real RTSP
//! client) and probed with raw requests for the error paths.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use retina::client::{PlayOptions, SessionOptions, SetupOptions, Transport};
use retina::codec::{CodecItem, ParametersRef};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::streaming::rtsp::rtp::nal_units;

const H264: &[u8] = include_bytes!("testdata/h264.annexb");
const H265: &[u8] = include_bytes!("testdata/h265.annexb");
const FRAME_US: i64 = 200_000;

#[derive(Clone)]
struct Fake {
    codec: VideoCodec,
    stream: &'static [u8],
    valid: Arc<AtomicBool>,
    packets: broadcast::Sender<VideoPacket>,
    reauth: Duration,
}

fn is_vcl(codec: VideoCodec, nal: &[u8]) -> bool {
    match codec {
        VideoCodec::H265 => (nal[0] >> 1) & 0x3F < 32,
        _ => (1..=5).contains(&(nal[0] & 0x1F)),
    }
}

fn is_key(codec: VideoCodec, nal: &[u8]) -> bool {
    match codec {
        VideoCodec::H265 => (16..=21).contains(&((nal[0] >> 1) & 0x3F)),
        _ => nal[0] & 0x1F == 5,
    }
}

fn annexb(nal: &[u8]) -> Vec<u8> {
    let mut v = vec![0, 0, 0, 1];
    v.extend_from_slice(nal);
    v
}

impl Fake {
    fn new(codec: VideoCodec, stream: &'static [u8]) -> Self {
        let (packets, _) = broadcast::channel(256);
        let fake = Self {
            codec,
            stream,
            valid: Arc::new(AtomicBool::new(true)),
            packets,
            reauth: Duration::from_secs(60),
        };
        // zmc's stream socket, one NAL per packet, looping the fixture.
        let feed = fake.clone();
        tokio::spawn(async move {
            let mut ts = FRAME_US;
            loop {
                for nal in nal_units(feed.stream) {
                    let _ = feed.packets.send(VideoPacket {
                        monitor_id: 1,
                        timestamp_us: ts,
                        data: annexb(nal),
                        is_keyframe: is_key(feed.codec, nal),
                        codec: feed.codec,
                    });
                    if is_vcl(feed.codec, nal) {
                        ts += FRAME_US;
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        });
        fake
    }

    /// Parameter sets plus the first picture.
    fn keyframe(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nal_units(self.stream) {
            out.extend(annexb(nal));
            if is_vcl(self.codec, nal) {
                break;
            }
        }
        out
    }
}

impl Backend for Fake {
    async fn authorize(&self, token: &str, monitor_id: u32) -> Result<Grant, u16> {
        match (token, monitor_id) {
            ("good", 1) if self.valid.load(Ordering::SeqCst) => Ok(Grant {
                uid: 7,
                exp: chrono::Utc::now().timestamp() + 3600,
            }),
            ("good", 1) => Err(401),
            ("good", _) => Err(404),
            _ => Err(401),
        }
    }

    async fn open(&self, monitor_id: u32) -> Result<Media, u16> {
        if monitor_id != 1 {
            return Err(404);
        }
        Ok(Media {
            codec: self.codec,
            keyframe: self.keyframe(),
            keyframe_us: 0,
            packets: self.packets.subscribe(),
        })
    }

    fn reauth_interval(&self) -> Duration {
        self.reauth
    }

    fn auth_deadline(&self) -> Duration {
        Duration::from_millis(500)
    }
}

async fn start(fake: Fake) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (sock, peer) = listener.accept().await.unwrap();
            tokio::spawn(serve(sock, fake.clone(), peer.to_string()));
        }
    });
    port
}

/// Play `url` with retina; the stream's pixel size and the first frames.
async fn play(url: &str, frames: usize) -> ((u32, u32), Vec<bool>) {
    let mut session =
        retina::client::Session::describe(url::Url::parse(url).unwrap(), SessionOptions::default())
            .await
            .expect("describe");
    session
        .setup(
            0,
            SetupOptions::default().transport(Transport::Tcp(Default::default())),
        )
        .await
        .expect("setup");
    let dims = match session.streams()[0].parameters() {
        Some(ParametersRef::Video(v)) => v.pixel_dimensions(),
        other => panic!("no video parameters: {other:?}"),
    };
    let mut demuxed = session
        .play(PlayOptions::default())
        .await
        .expect("play")
        .demuxed()
        .expect("demux");
    let mut keys = Vec::new();
    while keys.len() < frames {
        let item = tokio::time::timeout(Duration::from_secs(5), demuxed.next())
            .await
            .expect("a frame within 5 s")
            .expect("stream open")
            .expect("valid item");
        if let CodecItem::VideoFrame(f) = item {
            keys.push(f.is_random_access_point());
        }
    }
    (dims, keys)
}

#[tokio::test]
async fn retina_plays_h264() {
    let port = start(Fake::new(VideoCodec::H264, H264)).await;
    let (dims, keys) = play(&format!("rtsp://127.0.0.1:{port}/live/1?st=good"), 6).await;
    assert_eq!(dims, (64, 64));
    assert!(keys[0], "starts on the cached keyframe");
    assert!(keys.iter().skip(1).any(|k| *k), "later keyframes arrive");
}

#[tokio::test]
async fn retina_plays_h265() {
    let port = start(Fake::new(VideoCodec::H265, H265)).await;
    let (dims, keys) = play(&format!("rtsp://127.0.0.1:{port}/live/1?st=good"), 6).await;
    assert_eq!(dims, (64, 64));
    assert!(keys[0]);
}

/// Send one request and read the status code and headers of the reply.
async fn exchange(sock: &mut TcpStream, req: &str) -> (u16, String) {
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut byte))
            .await
            .expect("reply within 5 s")
            .unwrap();
        if n == 0 {
            return (0, String::new());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8(head).unwrap();
    let mut body_len = 0;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Content-Length: ") {
            body_len = v.parse().unwrap();
        }
    }
    let mut body = vec![0u8; body_len];
    sock.read_exact(&mut body).await.unwrap();
    (text[9..12].parse().unwrap(), text)
}

fn req(method: &str, url: &str, cseq: u32, extra: &str) -> String {
    format!("{method} {url} RTSP/1.0\r\nCSeq: {cseq}\r\n{extra}\r\n")
}

#[tokio::test]
async fn links_are_required_and_bad_ones_get_401_until_the_limit() {
    let port = start(Fake::new(VideoCodec::H264, H264)).await;
    let base = format!("rtsp://127.0.0.1:{port}/live/1");
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (code, _) = exchange(&mut sock, &req("OPTIONS", "*", 1, "")).await;
    assert_eq!(code, 200);
    let (code, _) = exchange(&mut sock, &req("DESCRIBE", &base, 2, "")).await;
    assert_eq!(code, 401, "no link");
    for cseq in 3..=5 {
        let url = format!("{base}?st=forged");
        let (code, _) = exchange(&mut sock, &req("DESCRIBE", &url, cseq, "")).await;
        assert_eq!(code, 401, "bad link, connection still open");
    }
    let (code, _) = exchange(&mut sock, &req("OPTIONS", "*", 6, "")).await;
    assert_eq!(code, 0, "closed after three failed checks");
}

#[tokio::test]
async fn a_connection_stays_on_its_monitor_and_tcp_only() {
    let port = start(Fake::new(VideoCodec::H264, H264)).await;
    let url = format!("rtsp://127.0.0.1:{port}/live/1?st=good");
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (code, text) = exchange(&mut sock, &req("DESCRIBE", &url, 1, "")).await;
    assert_eq!(code, 200, "{text}");
    let other = format!("rtsp://127.0.0.1:{port}/live/2");
    let (code, _) = exchange(&mut sock, &req("DESCRIBE", &other, 2, "")).await;
    assert_eq!(code, 403, "bound to monitor 1");
    let track = format!("rtsp://127.0.0.1:{port}/live/1/track0");
    let udp = "Transport: RTP/AVP;unicast;client_port=5000-5001\r\n";
    let (code, _) = exchange(&mut sock, &req("SETUP", &track, 3, udp)).await;
    assert_eq!(code, 461, "UDP refused");
    let (code, _) = exchange(&mut sock, &req("PLAY", &url, 4, "Session: nope\r\n")).await;
    assert_eq!(code, 454);
    let tcp =
        "Transport: RTP/AVP;unicast;client_port=5000-5001,RTP/AVP/TCP;unicast;interleaved=2-3\r\n";
    let (code, text) = exchange(&mut sock, &req("SETUP", &track, 5, tcp)).await;
    assert_eq!(code, 200);
    assert!(text.contains("interleaved=2-3"), "{text}");
}

#[tokio::test]
async fn a_revoked_link_ends_the_stream() {
    let mut fake = Fake::new(VideoCodec::H264, H264);
    fake.reauth = Duration::from_millis(300);
    let valid = fake.valid.clone();
    let port = start(fake).await;
    let url = format!("rtsp://127.0.0.1:{port}/live/1?st=good");
    let mut session =
        retina::client::Session::describe(url::Url::parse(&url).unwrap(), Default::default())
            .await
            .unwrap();
    session
        .setup(
            0,
            SetupOptions::default().transport(Transport::Tcp(Default::default())),
        )
        .await
        .unwrap();
    let mut demuxed = session
        .play(PlayOptions::default())
        .await
        .unwrap()
        .demuxed()
        .unwrap();
    demuxed.next().await.unwrap().unwrap();
    valid.store(false, Ordering::SeqCst);
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(_)) = demuxed.next().await {}
    })
    .await;
    assert!(ended.is_ok(), "stream still running after revocation");
}

#[test]
fn transports() {
    assert_eq!(tcp_channel("RTP/AVP/TCP;unicast;interleaved=0-1"), Some(0));
    assert_eq!(
        tcp_channel("RTP/AVP;unicast,RTP/AVP/TCP;interleaved=4-5"),
        Some(4)
    );
    assert_eq!(tcp_channel("RTP/AVP/TCP;unicast"), Some(0));
    assert_eq!(tcp_channel("RTP/AVP;unicast;client_port=1-2"), None);
    assert_eq!(tcp_channel("RTP/AVP/TCP;interleaved=255-256"), None);
}

/// FFmpeg (Home Assistant's stream worker uses it) reads the stream, with
/// TCP forced and with its default of trying UDP first. Skipped when
/// `ffprobe` isn't installed.
#[tokio::test]
async fn ffprobe_reads_the_stream() {
    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("ffprobe not installed; skipping");
        return;
    }
    let port = start(Fake::new(VideoCodec::H264, H264)).await;
    let url = format!("rtsp://127.0.0.1:{port}/live/1?st=good");
    for transport in [&["-rtsp_transport", "tcp"][..], &[][..]] {
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new("ffprobe")
                .args(["-v", "error"])
                .args(transport)
                .args([
                    "-show_entries",
                    "stream=codec_name,width,height",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&url)
                .output(),
        )
        .await
        .expect("ffprobe finished")
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.trim(),
            "h264,64,64",
            "{transport:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A connection that never presents a working link is closed at the
/// deadline, however busy it keeps itself; one that did isn't.
#[tokio::test]
async fn a_connection_must_authorize_before_the_deadline() {
    let port = start(Fake::new(VideoCodec::H264, H264)).await;
    let mut idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut authed = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let url = format!("rtsp://127.0.0.1:{port}/live/1?st=good");
    let (code, _) = exchange(&mut authed, &req("DESCRIBE", &url, 1, "")).await;
    assert_eq!(code, 200);

    let mut cseq = 1;
    let mut open = true;
    for _ in 0..10 {
        cseq += 1;
        let (code, _) = exchange(&mut idle, &req("OPTIONS", "*", cseq, "")).await;
        if code == 0 {
            open = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!open, "OPTIONS keepalives held the slot past the deadline");

    let (code, _) = exchange(&mut authed, &req("OPTIONS", "*", cseq + 1, "")).await;
    assert_eq!(code, 200, "an authorized connection stays");
}
