//! ONVIF camera discovery orchestration (service layer).
//!
//! This module is the integration seam between the reusable, transport-level
//! ONVIF client in [`crate::onvif`] and the HTTP API. It exposes two
//! operations:
//!
//! - [`probe`] — run WS-Discovery on the local network and return the set of
//!   discovered [`CameraCandidate`]s (one per camera that answered).
//! - [`inspect`] — given a device-service XAddr (and optional credentials),
//!   query the Device + Media services and return an [`InspectResult`] with the
//!   device's identity, media profiles, and resolved RTSP stream URIs.
//!
//! ## SSRF safety
//!
//! `inspect` makes server-side outbound requests to a caller-supplied URL, so
//! it is a classic SSRF sink. Two independent gates defend it:
//!
//! 1. **Scheme/shape gate** — the target must pass
//!    [`is_safe_onvif_url`](crate::dto::request::monitor::is_safe_onvif_url),
//!    which bounds the length, rejects control characters/CR-LF, and restricts
//!    the scheme to `http`/`https`/`rtsp`/`rtsps` (blocking `file://`,
//!    `unix://`, `gopher://`, …).
//! 2. **Destination gate** — the host must be a private IP *literal*: RFC1918 /
//!    loopback / link-local (IPv4) or ULA / loopback / link-local (IPv6). This
//!    stops a caller from coercing the server into reaching arbitrary public
//!    hosts. We deliberately do **not** perform DNS resolution (to avoid
//!    DNS-rebinding TOCTOU), so a non-literal hostname is **always rejected** —
//!    even if it would resolve to a private address.
//!
//! Caveat: `169.254.169.254` (the cloud-metadata endpoint) is link-local and
//! therefore permitted, because link-local is exactly where cameras live;
//! hardened deployments should additionally restrict via network policy.

#![allow(clippy::result_large_err)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, instrument, warn};
use url::{Host, Url};
use utoipa::ToSchema;

use garde::Validate;

use crate::dto::request::discovery::OnboardRequest;
use crate::dto::request::monitor::{is_safe_onvif_url, CreateMonitorRequest};
use crate::dto::response::MonitorResponse;
use crate::entity::sea_orm_active_enums::MonitorType;
use crate::error::{AppError, AppResult};
use crate::onvif::device::DeviceClient;
use crate::onvif::discovery::DiscoveryClient;
use crate::onvif::media::{MediaClient, StreamTransport};
use crate::onvif::types::{Credentials, ServiceUrls};
use crate::onvif::{OnvifError, OnvifTransport};
use crate::server::state::AppState;

/// A single camera discovered by WS-Discovery, projected for the API.
///
/// This is the de-duplicated, scope-decoded view of one `ProbeMatch`: the
/// stable endpoint reference, the transport addresses (`xaddrs`) where the
/// device's ONVIF services live, and the friendly identity fields parsed from
/// the device's advertised scopes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CameraCandidate {
    /// Device endpoint reference (typically a `urn:uuid:…`); the stable logical
    /// identity. May be empty if the device omitted it.
    pub endpoint_reference: String,
    /// Transport addresses (the `XAddrs` list) — ONVIF service URLs such as
    /// `http://192.168.1.10/onvif/device_service`. The first is the natural
    /// `inspect` target.
    pub xaddrs: Vec<String>,
    /// Device type tokens (e.g. `NetworkVideoTransmitter`).
    pub types: Vec<String>,
    /// Friendly device name, parsed from the `name` scope, if advertised.
    pub name: Option<String>,
    /// Hardware/model string, parsed from the `hardware` scope, if advertised.
    pub hardware: Option<String>,
    /// Location string, parsed from the `location` scope, if advertised.
    pub location: Option<String>,
    /// Id of an existing monitor matching this device (by ONVIF-URL / RTSP host),
    /// if already onboarded. `None` ⇒ a new device the UI can offer to add.
    pub monitor_id: Option<u32>,
}

impl From<crate::onvif::discovery::ProbeMatch> for CameraCandidate {
    fn from(m: crate::onvif::discovery::ProbeMatch) -> Self {
        Self {
            endpoint_reference: m.endpoint_reference,
            xaddrs: m.xaddrs,
            types: m.types,
            name: m.name,
            hardware: m.hardware,
            location: m.location,
            monitor_id: None,
        }
    }
}

/// One media profile resolved during [`inspect`], with its stream URI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InspectProfile {
    /// Profile token (the handle used for subsequent Media calls).
    pub token: String,
    /// Human-readable profile name, if present.
    pub name: Option<String>,
    /// Video encoding (`H264`, `H265`, `JPEG`, …), if known.
    pub encoding: Option<String>,
    /// Encoded video width in pixels, if known.
    pub width: Option<u32>,
    /// Encoded video height in pixels, if known.
    pub height: Option<u32>,
    /// Resolved RTP-Unicast/RTSP stream URI for this profile, if the device
    /// answered `GetStreamUri`. `None` when the lookup failed for this profile
    /// (the rest of the inspection still succeeds).
    pub stream_uri: Option<String>,
}

/// Result of inspecting a single device: identity plus its media profiles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InspectResult {
    /// The device-service XAddr that was inspected.
    pub device_service: String,
    /// Device manufacturer, if reported.
    pub manufacturer: Option<String>,
    /// Device model, if reported.
    pub model: Option<String>,
    /// Firmware version, if reported.
    pub firmware_version: Option<String>,
    /// Hardware serial number, if reported.
    pub serial_number: Option<String>,
    /// Hardware identifier, if reported.
    pub hardware_id: Option<String>,
    /// Resolved Media service XAddr, if the device advertises one.
    pub media_service: Option<String>,
    /// PTZ service XAddr, if advertised.
    pub ptz_service: Option<String>,
    /// Events service XAddr, if advertised.
    pub events_service: Option<String>,
    /// The device's media profiles, each with its resolved stream URI.
    pub profiles: Vec<InspectProfile>,
    /// Id of an existing monitor matching this device, if already onboarded.
    pub monitor_id: Option<u32>,
}

/// Run WS-Discovery and return the discovered cameras.
///
/// Multicasts a `Probe` for `NetworkVideoTransmitter` devices and collects
/// `ProbeMatches` for `timeout`, de-duplicating by endpoint reference. Returns an
/// empty vector when no cameras answer (not an error).
#[instrument(level = "debug", skip(state))]
pub async fn probe(state: &AppState, timeout: Duration) -> AppResult<Vec<CameraCandidate>> {
    info!(?timeout, "Running ONVIF WS-Discovery probe.");
    let client = DiscoveryClient::new(timeout);
    let matches = client.probe().await.map_err(onvif_to_app_error)?;
    info!(count = matches.len(), "WS-Discovery probe complete.");

    // Cross-reference discovered devices against existing monitors so the UI can
    // show new vs. already-onboarded. One monitor query for the whole batch.
    let monitors = crate::repo::monitors::find_all(state.db(), None)
        .await
        .unwrap_or_default();
    Ok(matches
        .into_iter()
        .map(|m| {
            let mut c = CameraCandidate::from(m);
            c.monitor_id = c
                .xaddrs
                .iter()
                .find_map(|x| match_monitor_by_host(&monitors, x));
            c
        })
        .collect())
}

/// The host component of a URL, lowercased, if parseable.
fn url_host(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

/// Find an existing monitor whose ONVIF URL or capture `Path` points at the same
/// host as `url`. Best-effort host match (the stable identity across probes).
fn match_monitor_by_host(monitors: &[crate::entity::monitors::Model], url: &str) -> Option<u32> {
    let target = url_host(url)?;
    monitors
        .iter()
        .find(|m| {
            url_host(&m.onvif_url).as_deref() == Some(target.as_str())
                || m.path.as_deref().and_then(url_host).as_deref() == Some(target.as_str())
        })
        .map(|m| m.id)
}

/// Resolve `existing monitor for this device URL`, querying monitors once.
async fn existing_monitor_for_url(state: &AppState, url: &str) -> Option<u32> {
    let monitors = crate::repo::monitors::find_all(state.db(), None)
        .await
        .ok()?;
    match_monitor_by_host(&monitors, url)
}

/// Inspect a single device by its device-service XAddr.
///
/// Enforces the SSRF gates (see the module docs) against `xaddr`, then queries
/// `GetDeviceInformation` + `GetCapabilities` on the Device service and
/// `GetProfiles` + `GetStreamUri` on the Media service (if advertised), folding
/// the results into an [`InspectResult`].
///
/// `creds` are the optional WS-Security credentials for the device; pass `None`
/// for cameras with authentication disabled.
#[instrument(level = "debug", skip(state, creds), fields(xaddr = %xaddr))]
pub async fn inspect(
    state: &AppState,
    xaddr: &str,
    creds: Option<Credentials>,
) -> AppResult<InspectResult> {
    info!("Inspecting ONVIF device.");
    ensure_inspect_target_allowed(xaddr)?;

    // Its own client, which doesn't follow redirects: the device's address
    // was checked, and a redirect would take the request somewhere that
    // wasn't (the server's loopback, or the internet).
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AppError::InternalServerError(format!("http client: {e}")))?;
    let transport = OnvifTransport::new(client);

    // --- Device service: identity + capabilities --------------------------
    let device = DeviceClient::new(transport.clone(), xaddr, creds.clone());

    let info = device
        .get_device_information()
        .await
        .map_err(onvif_to_app_error)?;

    // Resolve per-service endpoints. Prefer the modern GetServices list; fall
    // back to GetCapabilities; fall back again to just the device XAddr.
    let urls = resolve_service_urls(&device, xaddr).await;

    let mut result = InspectResult {
        device_service: xaddr.to_string(),
        manufacturer: info.manufacturer,
        model: info.model,
        firmware_version: info.firmware_version,
        serial_number: info.serial_number,
        hardware_id: info.hardware_id,
        media_service: urls.media.clone(),
        ptz_service: urls.ptz.clone(),
        events_service: urls.events.clone(),
        profiles: Vec::new(),
        monitor_id: existing_monitor_for_url(state, xaddr).await,
    };

    // --- Media service: profiles + stream URIs ----------------------------
    if let Some(media_url) = urls.media.as_deref() {
        // The media XAddr was surfaced by the device itself; still gate it, in
        // case a hostile device points us at an off-network address.
        if let Err(e) = ensure_inspect_target_allowed(media_url) {
            warn!(error = %e, media_url, "Skipping media inspection: address not allowed.");
        } else {
            let media = MediaClient::new(transport, media_url, creds);
            match media.get_profiles().await {
                Ok(profiles) => {
                    for p in profiles {
                        let stream_uri = match media
                            .get_stream_uri(&p.token, StreamTransport::RtspUnicast)
                            .await
                        {
                            Ok(uri) => Some(uri.uri),
                            Err(e) => {
                                warn!(error = %e, token = %p.token,
                                    "GetStreamUri failed for profile.");
                                None
                            }
                        };
                        result.profiles.push(InspectProfile {
                            token: p.token,
                            name: p.name,
                            encoding: p.encoding,
                            width: p.width,
                            height: p.height,
                            stream_uri,
                        });
                    }
                }
                Err(e) => {
                    warn!(error = %e, "GetProfiles failed; returning identity only.");
                }
            }
        }
    }

    info!(
        profiles = result.profiles.len(),
        "ONVIF device inspection complete."
    );
    Ok(result)
}

/// Onboard a discovered ONVIF device as a new `Ffmpeg` monitor: inspect it, pick
/// a media profile's RTSP stream URI, and create the monitor with the device's
/// ONVIF URL + credentials. RTSP credentials are stored in the monitor's
/// `User`/`Pass` (cameras typically share them with ONVIF), so the zm-next
/// worker authenticates via the out-of-band credential path.
#[instrument(level = "debug", skip(state, req), fields(xaddr = %req.xaddr))]
pub async fn onboard(state: &AppState, req: OnboardRequest) -> AppResult<MonitorResponse> {
    let creds = if req.username.is_empty() {
        None
    } else {
        Some(Credentials::new(req.username.clone(), req.password.clone()))
    };
    let result = inspect(state, &req.xaddr, creds).await?;

    // Pick the requested profile, else the first with a resolved RTSP URI.
    let profile = match &req.profile_token {
        Some(tok) => result.profiles.iter().find(|p| &p.token == tok),
        None => result.profiles.iter().find(|p| p.stream_uri.is_some()),
    }
    .ok_or_else(|| {
        AppError::BadRequestError(
            "device exposed no media profile with a resolvable RTSP stream URI".to_string(),
        )
    })?;
    let stream_uri = profile.stream_uri.clone().ok_or_else(|| {
        AppError::BadRequestError(format!(
            "selected profile '{}' has no RTSP stream URI",
            profile.token
        ))
    })?;

    let name = req
        .name
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| result.model.clone())
        .unwrap_or_else(|| "ONVIF Camera".to_string());

    let has_creds = !req.username.is_empty();
    let create = CreateMonitorRequest {
        name,
        r#type: MonitorType::Ffmpeg,
        path: Some(stream_uri),
        width: Some(
            profile
                .width
                .and_then(|w| u16::try_from(w).ok())
                .filter(|w| *w >= 1)
                .unwrap_or(1),
        ),
        height: Some(
            profile
                .height
                .and_then(|h| u16::try_from(h).ok())
                .filter(|h| *h >= 1)
                .unwrap_or(1),
        ),
        storage_id: Some(req.storage_id.filter(|s| *s >= 1).unwrap_or(1)),
        method: Some("rtpRtsp".to_string()),
        onvif_url: Some(req.xaddr.clone()),
        onvif_username: Some(req.username.clone()),
        onvif_password: Some(req.password.clone()),
        user: has_creds.then(|| req.username.clone()),
        pass: has_creds.then(|| req.password.clone()),
        ..Default::default()
    };
    create.validate().map_err(AppError::InvalidInputError)?;
    info!(name = %create.name, "Onboarding ONVIF device as a new monitor.");
    crate::service::monitor::create(state, create).await
}

/// Resolve the device's per-service XAddrs, tolerating devices that only
/// implement one of `GetServices` / `GetCapabilities`.
async fn resolve_service_urls(device: &DeviceClient, xaddr: &str) -> ServiceUrls {
    match device.resolve_service_urls().await {
        Ok(urls) if urls.media.is_some() || urls.ptz.is_some() || urls.events.is_some() => urls,
        _ => match device.get_capabilities().await {
            Ok(caps) => caps.into_service_urls(xaddr),
            Err(e) => {
                warn!(error = %e, "GetServices/GetCapabilities failed; device only.");
                ServiceUrls::from_device(xaddr.to_string())
            }
        },
    }
}

/// Enforce the SSRF gates for an `inspect` target URL.
///
/// Fails with [`AppError::BadRequestError`] when the URL is malformed or its
/// scheme is disallowed, and [`AppError::PermissionDeniedError`] when the
/// destination host is a public/unresolvable address (an exfiltration risk).
fn ensure_inspect_target_allowed(xaddr: &str) -> AppResult<()> {
    target_allowed(xaddr, &own_addresses())
}

use crate::util::net_guard::{is_server_or_metadata, own_addresses};

/// [`ensure_inspect_target_allowed`] with the server's addresses passed in.
fn target_allowed(xaddr: &str, own: &[IpAddr]) -> AppResult<()> {
    // Gate 1: scheme/shape. Reuse the monitor DTO's hardened validator so the
    // accepted-scheme policy stays in one place.
    is_safe_onvif_url(xaddr, &())
        .map_err(|e| AppError::BadRequestError(format!("unsafe ONVIF inspect URL: {e}")))?;

    let url = Url::parse(xaddr)
        .map_err(|e| AppError::BadRequestError(format!("invalid ONVIF inspect URL: {e}")))?;

    // Gate 2: destination must be a private/loopback IP literal. We refuse to
    // resolve DNS here (rebinding TOCTOU); a non-literal host is rejected so a
    // caller cannot point us at an arbitrary public name. Match on the typed
    // `url::Host` so IPv6 literals (which `host_str()` returns bracketed, e.g.
    // `[fe80::1]`, defeating `IpAddr::from_str`) classify correctly.
    let literal = match url.host() {
        Some(Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        _ => None,
    };
    if let Some(ip) = literal.filter(|&ip| is_server_or_metadata(ip, own)) {
        return Err(AppError::PermissionDeniedError(format!(
            "ONVIF inspect target {ip} is this server or a cloud metadata address"
        )));
    }
    match url.host() {
        Some(Host::Ipv4(v4)) if is_private_v4(v4) => Ok(()),
        Some(Host::Ipv6(v6)) if is_private_v6(v6) => Ok(()),
        Some(Host::Ipv4(v4)) => Err(AppError::PermissionDeniedError(format!(
            "ONVIF inspect target {v4} is not a private (RFC1918/loopback/link-local) address"
        ))),
        Some(Host::Ipv6(v6)) => Err(AppError::PermissionDeniedError(format!(
            "ONVIF inspect target {v6} is not a private (ULA/loopback/link-local) address"
        ))),
        Some(Host::Domain(d)) => Err(AppError::PermissionDeniedError(format!(
            "ONVIF inspect target {d} is not an IP literal; only private/loopback/link-local \
             IP literals may be inspected (hostnames are rejected to avoid DNS rebinding)"
        ))),
        None => Err(AppError::BadRequestError(
            "ONVIF inspect URL has no host".to_string(),
        )),
    }
}

/// RFC1918 / loopback / link-local IPv4 classification (no `std` unstable APIs).
fn is_private_v4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_loopback() || ip.is_link_local()
}

/// Loopback / unique-local (fc00::/7) / link-local (fe80::/10) IPv6, plus
/// IPv4-mapped addresses re-checked through the v4 rules.
fn is_private_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    // IPv4-mapped (::ffff:a.b.c.d) — classify by the embedded v4 address.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_private_v4(v4);
    }
    let seg = ip.segments();
    // Unique local address: fc00::/7.
    let ula = (seg[0] & 0xfe00) == 0xfc00;
    // Link-local unicast: fe80::/10.
    let link_local = (seg[0] & 0xffc0) == 0xfe80;
    ula || link_local
}

/// Map an [`OnvifError`] onto the API's [`AppError`] taxonomy.
///
/// Auth failures and SOAP "not authorized" faults become 401; timeouts and
/// transport faults become 503 (the device, not the API, is unavailable);
/// discovery/parse problems become 502-style internal/bad-gateway mapped to
/// [`AppError::InternalServerError`].
fn onvif_to_app_error(e: OnvifError) -> AppError {
    match e {
        OnvifError::Auth => AppError::UnauthorizedError("ONVIF authentication failed".to_string()),
        OnvifError::Soap { code, reason } => {
            // A Sender/NotAuthorized fault is an auth problem; everything else
            // is an upstream device error.
            if code.to_ascii_lowercase().contains("notauthorized")
                || reason.to_ascii_lowercase().contains("not authorized")
            {
                AppError::UnauthorizedError(format!("ONVIF device rejected credentials: {reason}"))
            } else {
                AppError::ServiceUnavailableError(format!("ONVIF device fault {code}: {reason}"))
            }
        }
        OnvifError::Timeout => {
            AppError::ServiceUnavailableError("ONVIF device timed out".to_string())
        }
        // The details (refused, reset, TLS, a redirect) stay in the log: in
        // the response they would map out which ports answer.
        OnvifError::Http(err) => {
            warn!(error = %err, "ONVIF transport error.");
            AppError::ServiceUnavailableError("the device did not answer as an ONVIF camera".into())
        }
        OnvifError::Discovery(msg) => {
            AppError::InternalServerError(format!("WS-Discovery failed: {msg}"))
        }
        OnvifError::Parse(msg) => {
            warn!(error = %msg, "Malformed ONVIF response.");
            AppError::ServiceUnavailableError("the device's answer is not ONVIF".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_v4_ranges_are_allowed() {
        for ip in [
            "10.0.0.5",
            "172.16.4.4",
            "192.168.1.10",
            "127.0.0.1",
            "169.254.1.2", // link-local
        ] {
            assert!(is_private_v4(ip.parse().unwrap()), "{ip} should be private");
        }
    }

    #[test]
    fn public_v4_addresses_are_not_private() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "172.32.0.1"] {
            assert!(!is_private_v4(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[test]
    fn private_v6_classification() {
        assert!(is_private_v6("::1".parse().unwrap())); // loopback
        assert!(is_private_v6("fd00::1".parse().unwrap())); // ULA
        assert!(is_private_v6("fe80::1".parse().unwrap())); // link-local
                                                            // IPv4-mapped private/public delegate to the v4 rules.
        assert!(is_private_v6("::ffff:192.168.1.1".parse().unwrap()));
        assert!(!is_private_v6("::ffff:8.8.8.8".parse().unwrap()));
        // Public v6.
        assert!(!is_private_v6("2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn inspect_target_allows_private_http_device() {
        for u in [
            "http://192.168.1.10/onvif/device_service",
            "http://10.0.0.5:8080/onvif/device_service",
            // Link-local cameras (no DHCP) are fine; only the metadata address isn't.
            "http://169.254.10.20/onvif",
        ] {
            assert!(target_allowed(u, &[]).is_ok(), "{u} should be allowed");
        }
    }

    /// The server itself is never a camera: inspecting it would let a
    /// Monitors editor probe the server's own ports and services.
    #[test]
    fn inspect_target_rejects_the_server_itself() {
        let own: Vec<IpAddr> = vec!["192.168.0.45".parse().unwrap(), "fd12::45".parse().unwrap()];
        for u in [
            "http://127.0.0.1/onvif",
            "http://127.0.0.53:8080/onvif",
            "http://[::1]/onvif",
            "http://[::ffff:127.0.0.1]/onvif",
            "http://0.0.0.0/onvif",
            "http://192.168.0.45:3306/",
            "http://[fd12::45]/onvif",
            "http://[::ffff:192.168.0.45]/onvif",
        ] {
            let err = target_allowed(u, &own).unwrap_err();
            assert!(
                matches!(err, AppError::PermissionDeniedError(_)),
                "{u}: {err:?}"
            );
        }
        assert!(target_allowed("http://192.168.0.46/onvif", &own).is_ok());
    }

    #[test]
    fn inspect_target_rejects_cloud_metadata_addresses() {
        for u in [
            "http://169.254.169.254/latest/meta-data/",
            "http://[fd00:ec2::254]/",
        ] {
            let err = ensure_inspect_target_allowed(u).unwrap_err();
            assert!(
                matches!(err, AppError::PermissionDeniedError(_)),
                "{u}: {err:?}"
            );
        }
    }

    /// What a non-camera answered stays in the log, not the response, so
    /// inspect can't read back an internal service.
    #[test]
    fn parse_errors_do_not_echo_the_reply() {
        let e = onvif_to_app_error(OnvifError::Parse("<secret>root:x:0:0</secret>".into()));
        assert!(!format!("{e:?}").contains("secret"), "{e:?}");
    }

    #[test]
    fn inspect_target_rejects_public_address() {
        let err = ensure_inspect_target_allowed("http://8.8.8.8/onvif").unwrap_err();
        assert!(matches!(err, AppError::PermissionDeniedError(_)), "{err:?}");
    }

    #[test]
    fn inspect_target_allows_private_ipv6_device() {
        // host_str() returns these bracketed (e.g. "[fe80::1]"); the typed
        // url::Host classification must still accept them.
        for u in [
            "http://[fe80::1]/onvif/device_service", // link-local
            "http://[fd00::1234]:8080/onvif",        // ULA
        ] {
            // A fixed address list: macOS gives lo0 fe80::1.
            assert!(target_allowed(u, &[]).is_ok(), "{u} should be allowed");
        }
    }

    #[test]
    fn inspect_target_rejects_public_ipv6() {
        let err = ensure_inspect_target_allowed("http://[2001:4860:4860::8888]/onvif").unwrap_err();
        assert!(matches!(err, AppError::PermissionDeniedError(_)), "{err:?}");
    }

    #[test]
    fn inspect_target_rejects_cloud_metadata_via_hostname() {
        // A bare hostname is not an IP literal → rejected (no DNS resolution).
        let err = ensure_inspect_target_allowed("http://metadata.google.internal/").unwrap_err();
        assert!(matches!(err, AppError::PermissionDeniedError(_)), "{err:?}");
    }

    #[test]
    fn inspect_target_rejects_dangerous_scheme() {
        // file:// is blocked by is_safe_onvif_url → BadRequest, not a 403.
        let err = ensure_inspect_target_allowed("file:///etc/passwd").unwrap_err();
        assert!(matches!(err, AppError::BadRequestError(_)), "{err:?}");
    }

    #[test]
    fn inspect_target_rejects_unix_scheme() {
        let err = ensure_inspect_target_allowed("unix:///var/run/x.sock").unwrap_err();
        assert!(matches!(err, AppError::BadRequestError(_)), "{err:?}");
    }

    #[test]
    fn inspect_target_rejects_garbage_url() {
        let err = ensure_inspect_target_allowed("http://").unwrap_err();
        assert!(matches!(err, AppError::BadRequestError(_)), "{err:?}");
    }

    #[test]
    fn candidate_from_probe_match_carries_fields() {
        let m = crate::onvif::discovery::ProbeMatch {
            endpoint_reference: "urn:uuid:abc".to_string(),
            xaddrs: vec!["http://192.168.1.10/onvif/device_service".to_string()],
            types: vec!["NetworkVideoTransmitter".to_string()],
            name: Some("Lobby".to_string()),
            ..Default::default()
        };
        let c = CameraCandidate::from(m);
        assert_eq!(c.endpoint_reference, "urn:uuid:abc");
        assert_eq!(c.xaddrs.len(), 1);
        assert_eq!(c.name.as_deref(), Some("Lobby"));
    }

    #[test]
    fn onvif_auth_error_maps_to_unauthorized() {
        assert!(matches!(
            onvif_to_app_error(OnvifError::Auth),
            AppError::UnauthorizedError(_)
        ));
    }

    #[test]
    fn onvif_notauthorized_fault_maps_to_unauthorized() {
        let e = OnvifError::Soap {
            code: "ter:NotAuthorized".to_string(),
            reason: "Sender not authorized".to_string(),
        };
        assert!(matches!(
            onvif_to_app_error(e),
            AppError::UnauthorizedError(_)
        ));
    }

    #[test]
    fn onvif_timeout_maps_to_service_unavailable() {
        assert!(matches!(
            onvif_to_app_error(OnvifError::Timeout),
            AppError::ServiceUnavailableError(_)
        ));
    }

    #[test]
    fn onvif_discovery_error_maps_to_internal() {
        assert!(matches!(
            onvif_to_app_error(OnvifError::Discovery("bind: nope".into())),
            AppError::InternalServerError(_)
        ));
    }
}
