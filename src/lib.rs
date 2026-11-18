// Use jemalloc as the process-wide allocator. This crate is a long-running,
// allocation-bursty media server (HLS segmenting, WebRTC, ffmpeg decode/mux
// buffers): under glibc malloc, freed buffers are retained in per-arena free
// lists rather than returned to the OS, so RSS sits at its high-water mark for
// the life of the process. jemalloc returns freed pages aggressively, keeping
// RSS close to the live working set. Defined here in the library so every
// binary that links it (the `zm-api` server and `fixture-doctor`) and the test
// harness all pick it up. Gated off MSVC, where jemalloc is unavailable.
// Guarded by tests/jemalloc.rs.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Install the rustls `CryptoProvider` process-wide.
///
/// Must be called before any TLS/DTLS usage (WebRTC DTLS, axum-server TLS,
/// sqlx TLS connections). Safe to call multiple times — subsequent calls are
/// ignored if a provider is already installed.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub mod client;
pub mod cluster;
pub mod configure;
pub mod constant;
pub mod daemon;
pub mod dto;
pub mod entity;
pub mod enum_traits;
pub mod error;
pub mod handlers;
pub mod migration;
#[cfg(feature = "onvif-core")]
pub mod onvif;
pub mod ptz;
pub mod repo;
pub mod routes;
pub mod server;
pub mod service;
pub mod streaming;
pub mod util;
pub mod zm_shm;
