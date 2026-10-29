//! Live view through the router, from a stream socket (#236).
//!
//! `common::fake_zmc::FakeZmc` serves a monitor's stream socket the way zmc
//! does, with a real H.264 clip, and `TestApp::spawn_live` turns live view on
//! as `AppState::new` does. So the snapshot and HLS answers here come from the
//! real reader, segmenter and encoder, not the "not configured" paths.
//!
//!   APP_PROFILE=test-db cargo test --test it_live_streaming -- --include-ignored

mod common;

use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use common::fake_zmc::{FakeZmc, HEIGHT, WIDTH};
use common::fixtures::{insert_monitor, RowGuard};
use common::harness::{superuser_token, TestApp};
use serde_json::json;

/// The width and height in a JPEG's start-of-frame segment.
fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < jpeg.len() {
        if jpeg[i] != 0xFF {
            return None;
        }
        let marker = jpeg[i + 1];
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if (0xC0..=0xC2).contains(&marker) {
            let h = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]) as u32;
            let w = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_snapshot_is_a_jpeg_of_the_camera() {
    let app_db = TestApp::spawn().await;
    let m = insert_monitor(&app_db.db, "LiveSnap")
        .await
        .expect("monitor");
    let _m = RowGuard::monitor(m.id);
    let zmc = FakeZmc::start(m.id);
    let app = TestApp::spawn_live(zmc.socks_dir()).await;

    let r = app
        .get(
            &format!("/api/v3/monitors/{}/snapshot", m.id),
            &superuser_token(),
        )
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    assert_eq!(&r.body[..2], &[0xFF, 0xD8], "a JPEG");
    assert_eq!(jpeg_size(&r.body), Some((WIDTH, HEIGHT)));
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn no_snapshot_without_a_stream_socket() {
    let app_db = TestApp::spawn().await;
    let m = insert_monitor(&app_db.db, "LiveNoSock")
        .await
        .expect("monitor");
    let _m = RowGuard::monitor(m.id);
    // A zmc for another monitor: this one has no socket.
    let zmc = FakeZmc::start(m.id + 1_000_000);
    let app = TestApp::spawn_live(zmc.socks_dir()).await;

    let r = app
        .get(
            &format!("/api/v3/monitors/{}/snapshot", m.id),
            &superuser_token(),
        )
        .await;
    assert!(
        r.status().is_client_error() || r.status().is_server_error(),
        "{}: {}",
        r.status(),
        r.text()
    );
}

/// A monitor whose zmc is restarting (no stream socket yet) answers 503
/// with Retry-After, not the 404 of a monitor that doesn't exist (#206).
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_monitor_without_its_socket_is_unavailable_not_missing() {
    let app_db = TestApp::spawn().await;
    let m = insert_monitor(&app_db.db, "LiveRestart")
        .await
        .expect("monitor");
    let _m = RowGuard::monitor(m.id);
    // A zmc for another monitor: this one's socket is gone, as while its
    // zmc restarts.
    let zmc = FakeZmc::start(m.id + 1_000_000);
    let app = TestApp::spawn_live(zmc.socks_dir()).await;
    let token = superuser_token();

    let snapshot = app
        .get(&format!("/api/v3/monitors/{}/snapshot", m.id), &token)
        .await;
    let start = app
        .post_json(
            &format!("/api/v3/live/{}/start", m.id),
            &token,
            &json!({ "enable_hls": true }),
        )
        .await;
    for (what, r) in [("snapshot", &snapshot), ("live start", &start)] {
        assert_eq!(
            r.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{what}: {}",
            r.text()
        );
        let retry: u64 = r
            .headers
            .get("retry-after")
            .unwrap_or_else(|| panic!("{what}: no retry-after"))
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(retry >= 1, "{what}: retry-after {retry}");
    }

    // Another server's monitor never has its socket here: 404, not a 503
    // to retry forever.
    let other = TestApp::spawn_live_with(zmc.socks_dir(), |state| state.server_id = Some(1)).await;
    {
        use sea_orm::{ActiveModelTrait, Set};
        let mut am: zm_api::entity::monitors::ActiveModel = m.clone().into();
        am.server_id = Set(Some(2));
        am.update(&app_db.db).await.unwrap();
    }
    let r = other
        .get(&format!("/api/v3/monitors/{}/snapshot", m.id), &token)
        .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "{}", r.text());

    // A monitor that doesn't exist is still a 404.
    let missing = m.id + 2_000_000;
    let r = app
        .get(&format!("/api/v3/monitors/{missing}/snapshot"), &token)
        .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "{}", r.text());
    let r = app
        .post_json(
            &format!("/api/v3/live/{missing}/start"),
            &token,
            &json!({ "enable_hls": true }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "{}", r.text());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn live_hls_plays() {
    let app_db = TestApp::spawn().await;
    let m = insert_monitor(&app_db.db, "LiveHls")
        .await
        .expect("monitor");
    let _m = RowGuard::monitor(m.id);
    let zmc = FakeZmc::start(m.id);
    let app = TestApp::spawn_live(zmc.socks_dir()).await;
    let token = superuser_token();
    let base = format!("/api/v3/live/{}", m.id);

    let r = app
        .post_json(
            &format!("{base}/start"),
            &token,
            &json!({ "enable_hls": true }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());

    // Wait for the first finished segment.
    let deadline = Instant::now() + Duration::from_secs(20);
    let playlist = loop {
        let r = app.get(&format!("{base}/hls/live.m3u8"), &token).await;
        if r.status() == StatusCode::OK && r.text().contains(".m4s") {
            break r.text();
        }
        assert!(
            Instant::now() < deadline,
            "no segment in time: {}",
            r.text()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };

    let master = app.get(&format!("{base}/hls/master.m3u8"), &token).await;
    assert_eq!(master.status(), StatusCode::OK, "{}", master.text());
    assert!(
        master.text().contains("avc1"),
        "codec in master: {}",
        master.text()
    );

    let init = app.get(&format!("{base}/hls/init.mp4"), &token).await;
    assert_eq!(init.status(), StatusCode::OK);
    let segment_name = playlist
        .lines()
        .find(|l| l.ends_with(".m4s") && !l.starts_with('#'))
        .and_then(|l| l.rsplit('/').next())
        .expect("a segment line");
    let segment = app
        .request(Method::GET, &format!("{base}/hls/{segment_name}"))
        .bearer(&token)
        .send()
        .await;
    assert_eq!(segment.status(), StatusCode::OK);

    // init + segment is a fragmented MP4 that FFmpeg opens and decodes at the
    // camera's size.
    let file = zmc.socks_dir().join("live.mp4");
    std::fs::write(
        &file,
        [init.body.as_slice(), segment.body.as_slice()].concat(),
    )
    .unwrap();
    let (w, h, frames) = decode(&file);
    assert_eq!((w, h), (WIDTH, HEIGHT));
    assert!(frames > 0, "decoded no frames");

    let r = app.get(&format!("{base}/stats"), &token).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let r = app.delete(&format!("{base}/stop"), &token).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "{}", r.text());
}

/// The video size of `path` and how many frames decode from it.
fn decode(path: &std::path::Path) -> (u32, u32, usize) {
    ffmpeg_next::init().unwrap();
    let mut input = ffmpeg_next::format::input(path).expect("open fMP4");
    let stream = input
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .expect("video stream");
    let index = stream.index();
    let mut decoder = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())
        .unwrap()
        .decoder()
        .video()
        .unwrap();
    let mut frames = 0;
    let mut frame = ffmpeg_next::frame::Video::empty();
    for (s, packet) in input.packets() {
        if s.index() == index && decoder.send_packet(&packet).is_ok() {
            while decoder.receive_frame(&mut frame).is_ok() {
                frames += 1;
            }
        }
    }
    let _ = decoder.send_eof();
    while decoder.receive_frame(&mut frame).is_ok() {
        frames += 1;
    }
    (decoder.width(), decoder.height(), frames)
}

mod whep {
    //! A receive-only WebRTC viewer, as a browser behind WHEP is: offers
    //! H.264, gathers its host candidates, and counts the RTP that arrives.

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
    use webrtc::peer_connection::{
        MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCIceGatheringState, RTCPeerConnectionState, RTCSessionDescription,
    };
    use webrtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};

    pub struct Viewer {
        pub pc: Arc<dyn PeerConnection>,
        pub connected: tokio::sync::watch::Receiver<bool>,
        pub packets: Arc<AtomicU64>,
    }

    struct Events {
        connected: tokio::sync::watch::Sender<bool>,
        gathered: tokio::sync::watch::Sender<bool>,
        packets: Arc<AtomicU64>,
    }

    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for Events {
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

    /// A viewer and its gathered offer.
    pub async fn viewer() -> (Viewer, String) {
        let mut media = MediaEngine::default();
        media
            .register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: RTCRtpCodec {
                        mime_type: "video/H264".into(),
                        clock_rate: 90000,
                        channels: 0,
                        sdp_fmtp_line:
                            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                                .into(),
                        rtcp_feedback: vec![],
                    },
                    payload_type: 102,
                },
                RtpCodecKind::Video,
            )
            .unwrap();
        let (connected_tx, connected) = tokio::sync::watch::channel(false);
        let (gathered_tx, mut gathered) = tokio::sync::watch::channel(false);
        let packets = Arc::new(AtomicU64::new(0));
        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_media_engine(media)
                .with_handler(Arc::new(Events {
                    connected: connected_tx,
                    gathered: gathered_tx,
                    packets: packets.clone(),
                }))
                .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
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

    pub async fn accept(viewer: &Viewer, answer: String) {
        viewer
            .pc
            .set_remote_description(RTCSessionDescription::answer(answer).unwrap())
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_whep_viewer_receives_video() {
    let app_db = TestApp::spawn().await;
    let m = insert_monitor(&app_db.db, "LiveWhep")
        .await
        .expect("monitor");
    let _m = RowGuard::monitor(m.id);
    let zmc = FakeZmc::start(m.id);
    let app = TestApp::spawn_live(zmc.socks_dir()).await;
    let token = superuser_token();

    let (mut viewer, offer) = whep::viewer().await;
    let r = app
        .request(Method::POST, &format!("/api/v3/live/{}/webrtc/whep", m.id))
        .bearer(&token)
        .raw_body(&offer, "application/sdp")
        .send()
        .await;
    assert_eq!(r.status(), StatusCode::CREATED, "{}", r.text());
    let location = r
        .headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("session URL in Location")
        .to_string();
    assert!(r.text().contains("H264/90000"), "{}", r.text());
    whep::accept(&viewer, r.text()).await;

    tokio::time::timeout(Duration::from_secs(10), viewer.connected.wait_for(|c| *c))
        .await
        .expect("connects within 10s")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while viewer.packets.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "no RTP within 10s of connecting");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let r = app.delete(&location, &token).await;
    assert!(r.status().is_success(), "{}: {}", r.status(), r.text());
    let r = app.delete(&location, &token).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "the session is gone");
    let _ = viewer.pc.close().await;
}
