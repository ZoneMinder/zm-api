//! Media files for tests, made with the ffmpeg binary (#239).
//!
//! Locally, a test that needs one skips when ffmpeg isn't installed. In CI
//! (`CI` set, as GitHub Actions sets it) it fails instead, so a runner
//! without ffmpeg can't quietly turn these tests into passes.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// Whether the ffmpeg binary runs. Panics in CI when it doesn't.
pub fn have_ffmpeg() -> bool {
    let ok = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok && std::env::var_os("CI").is_some() {
        panic!("the ffmpeg binary is required in CI (media fixtures need it)");
    }
    if !ok {
        eprintln!("Skipping: ffmpeg binary not available to build a media fixture");
    }
    ok
}

/// Two seconds at 10 fps, black then white, `size` (e.g. `"64x64"`), H.264,
/// into `path`. `extra` adds ffmpeg output flags. False when there's no
/// ffmpeg (outside CI).
pub fn black_then_white_mp4(path: &Path, size: &str, extra: &[&str]) -> bool {
    if !have_ffmpeg() {
        return false;
    }
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(format!(
            "color=c=black:s={size}:r=10:d=1[b];color=c=white:s={size}:r=10:d=1[w];[b][w]concat=n=2:v=1"
        ))
        .args(["-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p"])
        .args(extra)
        .arg(path)
        .status()
        .expect("run ffmpeg");
    assert!(
        status.success(),
        "ffmpeg failed to write {}",
        path.display()
    );
    true
}
