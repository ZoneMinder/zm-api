//! WS-Discovery client for ONVIF (Probe → ProbeMatch over UDP multicast).
//!
//! ONVIF device discovery uses the WS-Discovery protocol: a SOAP-over-UDP
//! `Probe` message multicast to `239.255.255.250:3702` carrying the device type
//! `dn:NetworkVideoTransmitter`. Conforming devices answer with a unicast
//! `ProbeMatches` envelope (back to the prober's source port) listing one or
//! more `ProbeMatch` entries. Each match carries:
//!
//! - an endpoint reference address (`wsa:Address`, typically a `urn:uuid:…`),
//! - one or more transport addresses (`d:XAddrs`, space-separated URLs),
//! - the device `d:Types` (e.g. `dn:NetworkVideoTransmitter tds:Device`),
//! - the device `d:Scopes` (space-separated `onvif://…` scope URIs encoding the
//!   friendly name, hardware model, location, and supported profiles).
//!
//! This module builds the Probe SOAP body, runs the multicast send/collect loop
//! over a tokio [`UdpSocket`], and parses the replies with quick-xml into typed
//! [`ProbeMatch`] structs. The parser is **namespace-prefix agnostic** (it
//! matches on XML local-names, not prefixes) and tolerant of missing optional
//! fields, because real cameras vary wildly in the prefixes and elements they
//! emit.
//!
//! Note: discovery is deliberately *not* routed through [`OnvifTransport`] —
//! that transport is HTTP/SOAP, whereas WS-Discovery is SOAP-over-UDP with its
//! own addressing (WS-Addressing) headers and a multicast send model.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use tokio::net::UdpSocket;
use tokio::time::Instant;

use crate::onvif::error::{OnvifError, OnvifResult};

/// The IPv4 WS-Discovery multicast group.
const WS_DISCOVERY_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
/// The WS-Discovery UDP port.
const WS_DISCOVERY_PORT: u16 = 3702;

/// WS-Addressing `To` value for the discovery role.
const WSA_TO_DISCOVERY: &str = "urn:schemas-xmlsoap-org:ws:2005:04:discovery";
/// WS-Addressing `Action` for a Probe.
const WSA_ACTION_PROBE: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe";

/// The ONVIF device type to probe for: a Network Video Transmitter (camera).
///
/// The `dn:` prefix is bound to the ONVIF network WSDL namespace in the Probe
/// body below; cameras match a Probe whose `Types` names this device type.
const NVT_TYPE: &str = "dn:NetworkVideoTransmitter";

/// A single discovered ONVIF device, parsed from one `ProbeMatch`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeMatch {
    /// Endpoint reference address — the device's stable logical identity,
    /// typically a `urn:uuid:…` URN. Empty if the device omitted it.
    pub endpoint_reference: String,
    /// Transport addresses (the `XAddrs` list) where the device's ONVIF
    /// services can be reached, e.g. `http://192.168.1.10/onvif/device_service`.
    pub xaddrs: Vec<String>,
    /// The raw device type tokens (e.g. `NetworkVideoTransmitter`, `Device`).
    pub types: Vec<String>,
    /// The raw `onvif://…` scope URIs advertised by the device.
    pub scopes: Vec<String>,
    /// Friendly name parsed from a `…/name/<value>` scope, if present.
    pub name: Option<String>,
    /// Hardware model parsed from a `…/hardware/<value>` scope, if present.
    pub hardware: Option<String>,
    /// Location parsed from a `…/location/<value>` scope, if present.
    pub location: Option<String>,
}

impl ProbeMatch {
    /// Apply the parsed scope URIs to derive the friendly `name`, `hardware`,
    /// and `location` fields. Scope segments are percent-decoded leniently.
    fn derive_scope_fields(&mut self) {
        for scope in &self.scopes {
            if let Some(value) = scope_value(scope, "name") {
                self.name = Some(value);
            } else if let Some(value) = scope_value(scope, "hardware") {
                self.hardware = Some(value);
            } else if let Some(value) = scope_value(scope, "location") {
                self.location = Some(value);
            }
        }
    }
}

/// The most devices one probe returns. Replies are unauthenticated UDP, so
/// any host on the network can answer, as often and with as many devices as
/// it likes. Without a limit a flood of made-up devices grows the result,
/// and the cost of de-duplicating it, for the whole collection window.
const MAX_MATCHES: usize = 256;

/// WS-Discovery probe client.
///
/// Stateless apart from configuration; one client can issue many probes.
#[derive(Debug, Clone)]
pub struct DiscoveryClient {
    /// How long to keep collecting `ProbeMatches` replies after sending the
    /// Probe before returning the accumulated set.
    timeout: Duration,
}

impl Default for DiscoveryClient {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(4),
        }
    }
}

impl DiscoveryClient {
    /// Construct a discovery client with the given collection window.
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Build the SOAP-over-UDP `Probe` envelope for the given message id.
    ///
    /// The body probes for `dn:NetworkVideoTransmitter` devices. The
    /// `message_id` (a `urn:uuid:…`) lets responders correlate their
    /// `RelatesTo`; it is generated fresh per probe.
    pub fn build_probe(message_id: &str) -> String {
        format!(
            concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                "<e:Envelope ",
                "xmlns:e=\"http://www.w3.org/2003/05/soap-envelope\" ",
                "xmlns:w=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" ",
                "xmlns:d=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\" ",
                "xmlns:dn=\"http://www.onvif.org/ver10/network/wsdl\">",
                "<e:Header>",
                "<w:MessageID>{message_id}</w:MessageID>",
                "<w:To e:mustUnderstand=\"true\">{to}</w:To>",
                "<w:Action e:mustUnderstand=\"true\">{action}</w:Action>",
                "</e:Header>",
                "<e:Body>",
                "<d:Probe>",
                "<d:Types>{nvt}</d:Types>",
                "<d:Scopes/>",
                "</d:Probe>",
                "</e:Body>",
                "</e:Envelope>",
            ),
            message_id = message_id,
            to = WSA_TO_DISCOVERY,
            action = WSA_ACTION_PROBE,
            nvt = NVT_TYPE,
        )
    }

    /// Send a multicast Probe and collect `ProbeMatch` replies for the
    /// configured window.
    ///
    /// Binds an ephemeral UDP socket on `0.0.0.0`, multicasts the Probe to the
    /// WS-Discovery group, then reads unicast replies until the timeout
    /// elapses. Duplicate devices (same endpoint reference) are de-duplicated;
    /// the first reply wins. Returns the discovered devices (possibly empty).
    pub async fn probe(&self) -> OnvifResult<Vec<ProbeMatch>> {
        let message_id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
        let body = Self::build_probe(&message_id);

        // Bind to an ephemeral port on all interfaces. Multicast *send* does
        // not require joining the group; ProbeMatches arrive unicast back to
        // our source port.
        let bind: SocketAddr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
        let socket = UdpSocket::bind(bind)
            .await
            .map_err(|e| OnvifError::Discovery(format!("bind: {e}")))?;
        socket
            .set_broadcast(true)
            .map_err(|e| OnvifError::Discovery(format!("set_broadcast: {e}")))?;

        let target = SocketAddrV4::new(WS_DISCOVERY_ADDR, WS_DISCOVERY_PORT);
        socket
            .send_to(body.as_bytes(), target)
            .await
            .map_err(|e| OnvifError::Discovery(format!("send_to: {e}")))?;

        self.collect(&socket).await
    }

    /// Read and parse replies from `socket` until the collection window
    /// elapses, de-duplicating by endpoint reference.
    async fn collect(&self, socket: &UdpSocket) -> OnvifResult<Vec<ProbeMatch>> {
        let deadline = Instant::now() + self.timeout;
        let mut found: Vec<ProbeMatch> = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }

            let recv = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await;
            let n = match recv {
                // Timed out — collection window is over.
                Err(_elapsed) => break,
                Ok(Ok((n, _peer))) => n,
                // A transient socket error shouldn't abort the whole probe;
                // keep listening until the window closes.
                Ok(Err(_)) => continue,
            };

            let xml = String::from_utf8_lossy(&buf[..n]);
            // A single datagram may carry one ProbeMatches envelope with one or
            // more ProbeMatch children; parse them all.
            for mut m in parse_probe_matches(&xml) {
                m.derive_scope_fields();
                let dup = !m.endpoint_reference.is_empty()
                    && found
                        .iter()
                        .any(|e| e.endpoint_reference == m.endpoint_reference);
                if !dup {
                    found.push(m);
                }
                if found.len() >= MAX_MATCHES {
                    return Ok(found);
                }
            }
        }

        Ok(found)
    }
}

/// Parse a WS-Discovery `ProbeMatches` envelope into zero or more
/// [`ProbeMatch`] structs.
///
/// This is namespace-prefix agnostic: it keys off XML local-names so any of
/// `d:ProbeMatch`, `wsdd:ProbeMatch`, or a default-namespaced `ProbeMatch`
/// parse identically. Missing optional fields are simply left at their
/// defaults rather than causing a failure. Scope-derived fields (`name`,
/// `hardware`, `location`) are **not** populated here — call
/// [`ProbeMatch::derive_scope_fields`] (the [`DiscoveryClient`] does this
/// automatically).
pub fn parse_probe_matches(xml: &str) -> Vec<ProbeMatch> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut matches: Vec<ProbeMatch> = Vec::new();
    // The match currently being assembled (between ProbeMatch start/end).
    let mut current: Option<ProbeMatch> = None;
    // Local-name stack so we can interpret text in context (e.g. an `Address`
    // belongs to the endpoint reference only when nested under
    // `EndpointReference`).
    let mut stack: Vec<String> = Vec::new();
    // Text of the element currently open. quick-xml ≥0.38 splits text around
    // entity references (separate `GeneralRef` events) — e.g. an XAddrs URL
    // containing `&amp;` — so fragments are stitched here and applied on End.
    let mut pending = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "ProbeMatch" {
                    current = Some(ProbeMatch::default());
                }
                stack.push(local);
                pending.clear();
            }
            Ok(Event::Text(t)) => {
                pending.push_str(&super::xml::text_content(&t));
            }
            Ok(Event::GeneralRef(r)) => {
                pending.push_str(&super::xml::general_ref_content(&r));
            }
            Ok(Event::End(e)) => {
                let text = std::mem::take(&mut pending);
                if !text.is_empty() {
                    if let Some(m) = current.as_mut() {
                        apply_text(m, &stack, &text);
                    }
                }
                let local = local_name(e.name().as_ref());
                if local == "ProbeMatch" {
                    if let Some(m) = current.take() {
                        matches.push(m);
                    }
                }
                stack.pop();
            }
            Ok(Event::Eof) => break,
            // Malformed XML mid-stream: return whatever we assembled so far
            // rather than panicking. A single bad datagram won't kill a probe.
            Err(_) => break,
            _ => {}
        }
    }

    matches
}

/// Route a text node to the right field of the in-progress match based on the
/// element local-name stack.
fn apply_text(m: &mut ProbeMatch, stack: &[String], text: &str) {
    let Some(local) = stack.last().map(String::as_str) else {
        return;
    };
    match local {
        // The endpoint reference address lives under `EndpointReference`.
        "Address"
            if stack.iter().any(|s| s == "EndpointReference")
                && m.endpoint_reference.is_empty() =>
        {
            m.endpoint_reference = text.trim().to_string();
        }
        // XAddrs is a space-separated list of transport URLs.
        "XAddrs" => {
            m.xaddrs.extend(text.split_whitespace().map(str::to_string));
        }
        // Types is a space-separated list of (possibly prefixed) QNames; keep
        // the local part so `dn:NetworkVideoTransmitter` → `NetworkVideoTransmitter`.
        "Types" => {
            m.types.extend(text.split_whitespace().map(local_name));
        }
        // Scopes is a space-separated list of `onvif://…` scope URIs.
        "Scopes" => {
            m.scopes.extend(text.split_whitespace().map(str::to_string));
        }
        _ => {}
    }
}

/// Extract the decoded value of a named ONVIF scope, if present.
///
/// ONVIF scope URIs look like `onvif://www.onvif.org/<category>/<value>` where
/// `<value>` may itself contain slashes (e.g. a location path) and is
/// percent-encoded. Returns the percent-decoded value following the first
/// `/<category>/` segment, or `None` if this scope is a different category.
fn scope_value(scope: &str, category: &str) -> Option<String> {
    // Strip the scheme + authority so we're left with the path. We don't
    // require a specific authority (`www.onvif.org`) because some devices use
    // their own; we only need the `/<category>/<value>` shape.
    let path = scope
        .strip_prefix("onvif://")
        .and_then(|rest| rest.split_once('/'))
        .map(|(_authority, path)| path)
        // Fall back to treating the whole string as a path for malformed
        // scopes so a missing scheme doesn't lose data.
        .unwrap_or(scope);

    let needle = format!("{category}/");
    let value = path.strip_prefix(&needle)?;
    if value.is_empty() {
        return None;
    }
    Some(percent_decode(value))
}

/// Lenient percent-decoder for scope values. Invalid escapes are passed
/// through verbatim rather than erroring.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_val(bytes[i + 1]);
            let lo = hex_val(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Map an ASCII hex digit to its value.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Strip an XML namespace prefix, returning the local name.
fn local_name(qname: &str) -> String {
    match qname.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => qname.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic single-camera ProbeMatches reply with conventional `d:`/`wsa:`
    /// prefixes (the shape most commonly seen from Axis/Hikvision firmware).
    const NORMAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope
    xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope"
    xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
    xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
    xmlns:dn="http://www.onvif.org/ver10/network/wsdl"
    xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
  <SOAP-ENV:Header>
    <wsa:MessageID>urn:uuid:11111111-2222-3333-4444-555555555555</wsa:MessageID>
    <wsa:RelatesTo>urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee</wsa:RelatesTo>
    <wsa:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:To>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</wsa:Action>
  </SOAP-ENV:Header>
  <SOAP-ENV:Body>
    <d:ProbeMatches>
      <d:ProbeMatch>
        <wsa:EndpointReference>
          <wsa:Address>urn:uuid:abcdef01-1234-5678-9abc-def012345678</wsa:Address>
        </wsa:EndpointReference>
        <d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types>
        <d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/Profile/Streaming onvif://www.onvif.org/name/ACME%20Cam%201 onvif://www.onvif.org/hardware/IPC-Model-X onvif://www.onvif.org/location/country/us</d:Scopes>
        <d:XAddrs>http://192.168.1.10/onvif/device_service http://[fe80::1]/onvif/device_service</d:XAddrs>
        <d:MetadataVersion>1</d:MetadataVersion>
      </d:ProbeMatch>
    </d:ProbeMatches>
  </SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;

    /// The same payload with *different namespace prefixes* (default-namespaced
    /// discovery, `a:` for addressing) to prove prefix-agnostic parsing.
    const PREFIX_VARIED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Envelope
    xmlns="http://www.w3.org/2003/05/soap-envelope"
    xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing"
    xmlns:wsdd="http://schemas.xmlsoap.org/ws/2005/04/discovery"
    xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <Body>
    <wsdd:ProbeMatches>
      <wsdd:ProbeMatch>
        <a:EndpointReference>
          <a:Address>urn:uuid:99999999-8888-7777-6666-555555555555</a:Address>
        </a:EndpointReference>
        <wsdd:Types>dn:NetworkVideoTransmitter</wsdd:Types>
        <wsdd:Scopes>onvif://www.onvif.org/name/Lobby%20Door onvif://www.onvif.org/hardware/DS-2CD</wsdd:Scopes>
        <wsdd:XAddrs>http://10.0.0.5/onvif/device_service</wsdd:XAddrs>
      </wsdd:ProbeMatch>
    </wsdd:ProbeMatches>
  </Body>
</Envelope>"#;

    /// A reply missing all optional fields except a single XAddr — must parse
    /// without panicking and leave the optional fields empty/None.
    const MISSING_OPTIONALS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"
    xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery">
  <e:Body>
    <d:ProbeMatches>
      <d:ProbeMatch>
        <d:XAddrs>http://172.16.0.9/onvif/device_service</d:XAddrs>
      </d:ProbeMatch>
    </d:ProbeMatches>
  </e:Body>
</e:Envelope>"#;

    /// A single datagram carrying multiple ProbeMatch children (some firmwares
    /// batch matches), plus we also exercise parsing two *separate* datagrams.
    const BATCH: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
    xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing"
    xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
    xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <s:Body>
    <d:ProbeMatches>
      <d:ProbeMatch>
        <w:EndpointReference><w:Address>urn:uuid:cam-0001</w:Address></w:EndpointReference>
        <d:Types>dn:NetworkVideoTransmitter</d:Types>
        <d:Scopes>onvif://www.onvif.org/name/Cam%20One onvif://www.onvif.org/hardware/HW-1</d:Scopes>
        <d:XAddrs>http://192.168.1.21/onvif/device_service</d:XAddrs>
      </d:ProbeMatch>
      <d:ProbeMatch>
        <w:EndpointReference><w:Address>urn:uuid:cam-0002</w:Address></w:EndpointReference>
        <d:Types>dn:NetworkVideoTransmitter</d:Types>
        <d:Scopes>onvif://www.onvif.org/name/Cam%20Two onvif://www.onvif.org/hardware/HW-2</d:Scopes>
        <d:XAddrs>http://192.168.1.22/onvif/device_service</d:XAddrs>
      </d:ProbeMatch>
    </d:ProbeMatches>
  </s:Body>
</s:Envelope>"#;

    #[test]
    fn parses_normal_response() {
        let mut matches = parse_probe_matches(NORMAL);
        assert_eq!(matches.len(), 1);
        let m = &mut matches[0];
        m.derive_scope_fields();

        assert_eq!(
            m.endpoint_reference,
            "urn:uuid:abcdef01-1234-5678-9abc-def012345678"
        );
        assert_eq!(
            m.xaddrs,
            vec![
                "http://192.168.1.10/onvif/device_service".to_string(),
                "http://[fe80::1]/onvif/device_service".to_string(),
            ]
        );
        // Types should be reduced to local names.
        assert!(m.types.contains(&"NetworkVideoTransmitter".to_string()));
        assert!(m.types.contains(&"Device".to_string()));
        // Scope-derived friendly fields (percent-decoded).
        assert_eq!(m.name.as_deref(), Some("ACME Cam 1"));
        assert_eq!(m.hardware.as_deref(), Some("IPC-Model-X"));
        assert_eq!(m.location.as_deref(), Some("country/us"));
    }

    #[test]
    fn parses_prefix_varied_response_identically() {
        let mut matches = parse_probe_matches(PREFIX_VARIED);
        assert_eq!(matches.len(), 1);
        let m = &mut matches[0];
        m.derive_scope_fields();

        assert_eq!(
            m.endpoint_reference,
            "urn:uuid:99999999-8888-7777-6666-555555555555"
        );
        assert_eq!(
            m.xaddrs,
            vec!["http://10.0.0.5/onvif/device_service".to_string()]
        );
        assert_eq!(m.name.as_deref(), Some("Lobby Door"));
        assert_eq!(m.hardware.as_deref(), Some("DS-2CD"));
        assert!(m.location.is_none());
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        let mut matches = parse_probe_matches(MISSING_OPTIONALS);
        assert_eq!(matches.len(), 1);
        let m = &mut matches[0];
        m.derive_scope_fields();

        assert!(m.endpoint_reference.is_empty());
        assert!(m.types.is_empty());
        assert!(m.scopes.is_empty());
        assert!(m.name.is_none());
        assert!(m.hardware.is_none());
        assert!(m.location.is_none());
        assert_eq!(
            m.xaddrs,
            vec!["http://172.16.0.9/onvif/device_service".to_string()]
        );
    }

    #[test]
    fn parses_multi_camera_batch_in_one_envelope() {
        let mut matches = parse_probe_matches(BATCH);
        assert_eq!(matches.len(), 2);
        for m in &mut matches {
            m.derive_scope_fields();
        }

        assert_eq!(matches[0].endpoint_reference, "urn:uuid:cam-0001");
        assert_eq!(matches[0].name.as_deref(), Some("Cam One"));
        assert_eq!(
            matches[0].xaddrs,
            vec!["http://192.168.1.21/onvif/device_service".to_string()]
        );

        assert_eq!(matches[1].endpoint_reference, "urn:uuid:cam-0002");
        assert_eq!(matches[1].name.as_deref(), Some("Cam Two"));
        assert_eq!(
            matches[1].xaddrs,
            vec!["http://192.168.1.22/onvif/device_service".to_string()]
        );
    }

    #[test]
    fn de_duplicates_repeated_endpoint_references_across_datagrams() {
        // Simulate the collect loop's de-dup: the same camera answers twice
        // (cameras often send 1-3 duplicate ProbeMatches). We keep the first.
        let mut found: Vec<ProbeMatch> = Vec::new();
        for _ in 0..3 {
            for mut m in parse_probe_matches(NORMAL) {
                m.derive_scope_fields();
                let dup = !m.endpoint_reference.is_empty()
                    && found
                        .iter()
                        .any(|e| e.endpoint_reference == m.endpoint_reference);
                if !dup {
                    found.push(m);
                }
            }
        }
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn build_probe_contains_required_addressing_and_type() {
        let probe = DiscoveryClient::build_probe("urn:uuid:test-message-id");
        assert!(probe.contains("<d:Probe>"));
        assert!(probe.contains("urn:uuid:test-message-id"));
        assert!(probe.contains(WSA_TO_DISCOVERY));
        assert!(probe.contains(WSA_ACTION_PROBE));
        assert!(probe.contains("NetworkVideoTransmitter"));
        // SOAP 1.2 envelope namespace.
        assert!(probe.contains("http://www.w3.org/2003/05/soap-envelope"));
    }

    #[test]
    fn scope_value_handles_missing_scheme_and_categories() {
        // Correct category.
        assert_eq!(
            scope_value("onvif://www.onvif.org/name/Front%20Door", "name").as_deref(),
            Some("Front Door")
        );
        // Different category → None.
        assert!(scope_value("onvif://www.onvif.org/Profile/Streaming", "name").is_none());
        // Empty value → None.
        assert!(scope_value("onvif://www.onvif.org/name/", "name").is_none());
        // Vendor authority other than www.onvif.org still works.
        assert_eq!(
            scope_value("onvif://example.com/hardware/Model-Z", "hardware").as_deref(),
            Some("Model-Z")
        );
    }

    #[test]
    fn malformed_xml_does_not_panic() {
        // Truncated mid-element — must return gracefully (no panic).
        let truncated = &NORMAL[..NORMAL.len() / 2];
        let _ = parse_probe_matches(truncated);
        // Total garbage.
        let _ = parse_probe_matches("<<<not xml>>>");
        let _ = parse_probe_matches("");
    }

    /// A host that answers a probe with a flood of made-up devices gets a
    /// bounded list back, and the probe ends early.
    #[tokio::test]
    async fn a_flood_of_replies_stops_at_the_limit() {
        let camera = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let client_addr = client.local_addr().unwrap();

        let flood = tokio::spawn(async move {
            for n in 0..(MAX_MATCHES + 100) {
                let reply = NORMAL.replace(
                    "urn:uuid:abcdef01-1234-5678-9abc-def012345678",
                    &format!("urn:uuid:flood-{n}"),
                );
                camera.send_to(reply.as_bytes(), client_addr).await.unwrap();
                if n % 32 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        });

        let dc = DiscoveryClient::new(Duration::from_secs(20));
        let started = Instant::now();
        let found = dc.collect(&client).await.unwrap();
        flood.abort();
        assert_eq!(found.len(), MAX_MATCHES);
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    /// End-to-end socket smoke test: bind a local responder, point a client at
    /// the loopback, and confirm the send/collect plumbing parses a reply.
    /// Uses an explicit unicast send (not the multicast group) so it runs in
    /// CI sandboxes without multicast routing.
    #[tokio::test]
    async fn collect_parses_replies_over_real_socket() {
        // The "camera": bind a socket that will answer the client.
        let camera = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let camera_addr = camera.local_addr().unwrap();

        // The "client" socket we'll drive collect() on.
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let client_addr = client.local_addr().unwrap();

        // Camera task: wait for the probe, reply with two datagrams (one of
        // them a duplicate of NORMAL to exercise de-dup).
        let cam = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let (_n, peer) = camera.recv_from(&mut buf).await.unwrap();
            assert_eq!(peer, client_addr);
            camera.send_to(NORMAL.as_bytes(), peer).await.unwrap();
            camera.send_to(BATCH.as_bytes(), peer).await.unwrap();
            // Duplicate of NORMAL — must be de-duplicated by endpoint ref.
            camera.send_to(NORMAL.as_bytes(), peer).await.unwrap();
        });

        // Drive the probe: send to the camera, then collect.
        let dc = DiscoveryClient::new(Duration::from_millis(400));
        client.send_to(b"probe", camera_addr).await.unwrap();
        let found = dc.collect(&client).await.unwrap();
        cam.await.unwrap();

        // NORMAL (1 unique) + BATCH (2) = 3 unique devices; the duplicate
        // NORMAL is dropped.
        assert_eq!(found.len(), 3);
        let refs: Vec<&str> = found
            .iter()
            .map(|m| m.endpoint_reference.as_str())
            .collect();
        assert!(refs.contains(&"urn:uuid:abcdef01-1234-5678-9abc-def012345678"));
        assert!(refs.contains(&"urn:uuid:cam-0001"));
        assert!(refs.contains(&"urn:uuid:cam-0002"));
    }
}
