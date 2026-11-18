//! SOAP-over-HTTP transport for ONVIF.
//!
//! ONVIF uses SOAP 1.2 (`application/soap+xml`, with an `action` parameter in
//! the Content-Type). This module wraps a service-specific body in a SOAP 1.2
//! envelope, optionally injects a WS-Security UsernameToken header, POSTs it,
//! and maps a SOAP Fault response to [`OnvifError::Soap`].

use std::time::Duration;

use crate::onvif::error::{OnvifError, OnvifResult};
use crate::onvif::security::{generate_nonce_created, wsse_username_token};
use crate::onvif::types::Credentials;

/// SOAP-over-HTTP transport sharing a single reqwest client.
#[derive(Debug, Clone)]
pub struct OnvifTransport {
    client: reqwest::Client,
}

impl OnvifTransport {
    /// Construct a transport over an existing reqwest client. Sharing the
    /// client lets callers reuse the connection pool and TLS config.
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Perform a single SOAP 1.2 call.
    ///
    /// `body_xml` is the inner body content (the operation element, e.g.
    /// `<tds:GetDeviceInformation/>`); it is wrapped in a SOAP envelope. When
    /// `creds` is `Some`, a fresh WS-Security UsernameToken header is injected.
    /// On HTTP success the raw response body XML is returned; a SOAP Fault
    /// (whether returned with a 2xx or 4xx/5xx status) is mapped to
    /// [`OnvifError::Soap`].
    pub async fn call(
        &self,
        service_url: &str,
        soap_action: &str,
        body_xml: &str,
        creds: Option<&Credentials>,
        timeout: Duration,
    ) -> OnvifResult<String> {
        // WS-Addressing headers. ONVIF (WS-BaseNotification, used by the Events
        // service) requires wsa:To/Action/MessageID; many devices also read the
        // action from the wsa:Action header rather than the Content-Type param.
        // Emitted for every call (harmless for Device/Media/PTZ, mandatory for
        // Events). MessageID is a fresh urn:uuid per request.
        let pinned = ensure_target_allowed(service_url).await?;
        let client = pinned.as_ref().unwrap_or(&self.client);
        let message_id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
        let wsa = wsa_headers(service_url, soap_action, &message_id);
        let security = match creds {
            Some(c) => {
                let (nonce, created) = generate_nonce_created();
                wsse_username_token(c, &nonce, &created)
            }
            None => String::new(),
        };
        let payload = envelope(&format!("{wsa}{security}"), body_xml);

        // SOAP 1.2 carries the action in the Content-Type parameter.
        let content_type = format!("application/soap+xml; charset=utf-8; action=\"{soap_action}\"");

        let resp = client
            .post(service_url)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .timeout(timeout)
            .body(payload)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    OnvifError::Timeout
                } else {
                    OnvifError::Http(e)
                }
            })?;

        let status = resp.status();
        let text = read_capped(resp).await?;

        // A SOAP Fault may arrive with a non-2xx status (commonly 400/500) or,
        // for some devices, a 200. Parse the body either way before deciding.
        if let Some((code, reason)) = parse_soap_fault(&text) {
            return Err(OnvifError::Soap { code, reason });
        }

        if !status.is_success() {
            // Many cameras reject bad credentials with a bare HTTP 401/403 and
            // an empty (non-SOAP) body — they enforce HTTP-level auth alongside
            // or instead of WS-Security. Surface that as an auth error so it
            // maps to 401, not a generic "device unavailable".
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(OnvifError::Auth);
            }
            return Err(OnvifError::Parse(format!(
                "http {status} with no SOAP fault: {}",
                truncate(&text, 512)
            )));
        }

        Ok(text)
    }
}

/// Refuse a service address that reaches this server or a cloud metadata
/// service. `service_url` is a stored `ONVIF_URL` (anyone with Monitors Edit
/// sets it) or an address a device returned, and the request carries the
/// camera's credentials.
/// Refuse a target that is this server or a metadata service. For a host
/// name, returns a client that connects only to the addresses the check saw:
/// the shared client would look the name up again, and a name that answers
/// with a camera's address first and `127.0.0.1` second (DNS rebinding) would
/// get past the check.
async fn ensure_target_allowed(service_url: &str) -> OnvifResult<Option<reqwest::Client>> {
    use crate::util::net_guard::{check_and_resolve, own_addresses, Checked};
    match check_and_resolve(service_url, &own_addresses()).await {
        Err(ip) => Err(OnvifError::Refused(format!(
            "{service_url} reaches {ip}, which is this server or a cloud metadata address"
        ))),
        Ok(Checked::Literal) => Ok(None),
        Ok(Checked::Name(name, addrs)) if addrs.is_empty() => {
            Err(OnvifError::Refused(format!("{name} does not resolve")))
        }
        Ok(Checked::Name(name, addrs)) => reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(&name, &addrs)
            .build()
            .map(Some)
            .map_err(OnvifError::Http),
    }
}

/// The most a device's response may hold. ONVIF replies are a few kilobytes;
/// the largest (GetProfiles, a full PullMessages batch) stay well under this.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// The response body as text, refused once it passes [`MAX_RESPONSE_BYTES`]
/// so a device can't make the server buffer an endless reply.
async fn read_capped(mut resp: reqwest::Response) -> OnvifResult<String> {
    let mut body = Vec::new();
    loop {
        let chunk = resp.chunk().await.map_err(|e| {
            if e.is_timeout() {
                OnvifError::Timeout
            } else {
                OnvifError::Http(e)
            }
        })?;
        let Some(chunk) = chunk else { break };
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(OnvifError::Parse(format!(
                "response is larger than {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// SOAP 1.2 envelope namespace.
const SOAP_ENV_NS: &str = "http://www.w3.org/2003/05/soap-envelope";
/// WS-Addressing 1.0 namespace.
const WSA_NS: &str = "http://www.w3.org/2005/08/addressing";

/// Build the WS-Addressing SOAP header block (To / Action / MessageID / ReplyTo).
///
/// `xmlns:wsa` and the `s:` prefix are declared on the envelope (see
/// [`envelope`]), so the elements here reference them without redeclaring.
fn wsa_headers(to: &str, action: &str, message_id: &str) -> String {
    format!(
        concat!(
            "<wsa:To s:mustUnderstand=\"1\">{to}</wsa:To>",
            "<wsa:Action s:mustUnderstand=\"1\">{action}</wsa:Action>",
            "<wsa:MessageID>{mid}</wsa:MessageID>",
            "<wsa:ReplyTo><wsa:Address>",
            "http://www.w3.org/2005/08/addressing/anonymous",
            "</wsa:Address></wsa:ReplyTo>",
        ),
        to = xml_escape(to),
        action = xml_escape(action),
        mid = xml_escape(message_id),
    )
}

/// Build a SOAP 1.2 envelope around an optional header and a body.
///
/// `header_xml` may be empty (no `<s:Header>` is emitted in that case). The
/// envelope declares both the SOAP and WS-Addressing namespaces so header
/// fragments (WS-Addressing, WS-Security) can use the `s:` and `wsa:` prefixes.
pub fn envelope(header_xml: &str, body_xml: &str) -> String {
    let header = if header_xml.is_empty() {
        String::new()
    } else {
        format!("<s:Header>{header_xml}</s:Header>")
    };
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<s:Envelope xmlns:s=\"{ns}\" xmlns:wsa=\"{wsa}\">",
            "{header}",
            "<s:Body>{body}</s:Body>",
            "</s:Envelope>",
        ),
        ns = SOAP_ENV_NS,
        wsa = WSA_NS,
        header = header,
        body = body_xml,
    )
}

/// Minimal XML text/attribute escaper for values interpolated into the SOAP
/// header (service URLs and action URIs can contain `&`).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Extract a SOAP 1.2 Fault `(code, reason)` from a response body, if present.
///
/// Namespace-prefix-agnostic: matches the local element names `Fault`,
/// `Code`/`Value`, and `Reason`/`Text` regardless of the bound prefix.
fn parse_soap_fault(xml: &str) -> Option<(String, String)> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut in_fault = false;
    let mut in_code_value = false;
    let mut in_subcode_value = false;
    let mut in_reason_text = false;
    // The top-level Code/Value (e.g. `s:Sender`) is the generic SOAP class; the
    // ONVIF machine-readable discriminator (e.g. `ter:NotAuthorized`) lives in
    // the nested Subcode/Value. Capture both and prefer the deepest Subcode so
    // callers can key on the specific ONVIF code regardless of Reason language.
    let mut code: Option<String> = None;
    let mut subcode: Option<String> = None;
    let mut reason: Option<String> = None;
    // Local-name stack to interpret Value/Text context.
    let mut stack: Vec<String> = Vec::new();
    // Accumulates the text of the capturing element currently open. quick-xml
    // ≥0.38 splits text around entity references (emitted as `GeneralRef`), so
    // fragments are stitched here and committed on the element's End tag.
    let mut capture: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                match local.as_str() {
                    "Fault" => in_fault = true,
                    "Value" if in_fault && parent_is(&stack, "Code") => {
                        in_code_value = true;
                        capture = Some(String::new());
                    }
                    "Value" if in_fault && parent_is(&stack, "Subcode") => {
                        in_subcode_value = true;
                        capture = Some(String::new());
                    }
                    "Text" if in_fault && parent_is(&stack, "Reason") => {
                        in_reason_text = true;
                        capture = Some(String::new());
                    }
                    _ => {}
                }
                stack.push(local);
            }
            Ok(Event::Text(t)) => {
                if let Some(buf) = capture.as_mut() {
                    buf.push_str(&super::xml::text_content(&t));
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if let Some(buf) = capture.as_mut() {
                    buf.push_str(&super::xml::general_ref_content(&r));
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref());
                match local.as_str() {
                    "Value" => {
                        if let Some(buf) = capture.take().filter(|b| !b.is_empty()) {
                            if in_code_value && code.is_none() {
                                // Top-level code: first (outermost) Value wins.
                                code = Some(buf);
                            } else if in_subcode_value {
                                // Subcode: last (innermost / most specific) Value wins.
                                subcode = Some(buf);
                            }
                        }
                        in_code_value = false;
                        in_subcode_value = false;
                    }
                    "Text" => {
                        if let Some(buf) = capture.take().filter(|b| !b.is_empty()) {
                            if in_reason_text && reason.is_none() {
                                reason = Some(buf);
                            }
                        }
                        in_reason_text = false;
                    }
                    "Fault" => in_fault = false,
                    _ => {}
                }
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }

    // Prefer the specific ONVIF Subcode over the generic SOAP class.
    let code = subcode.or(code);
    if code.is_some() || reason.is_some() {
        Some((
            code.unwrap_or_else(|| "unknown".to_string()),
            reason.unwrap_or_else(|| "unknown".to_string()),
        ))
    } else {
        None
    }
}

/// True when the element immediately enclosing the current one has the given
/// local name.
fn parent_is(stack: &[String], local: &str) -> bool {
    stack.last().map(|s| s.as_str()) == Some(local)
}

/// Strip an XML namespace prefix, returning the local name.
fn local_name(qname: &str) -> String {
    match qname.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => qname.to_string(),
    }
}

/// Truncate a string to at most `max` bytes for error messages.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find a char boundary at or below max.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The address may be a stored `ONVIF_URL` or one a device handed back
    /// (a subscription reference, a media XAddr), so the transport itself
    /// refuses this server and cloud metadata. The listener would accept a
    /// connection if the request were sent.
    #[tokio::test]
    async fn refuses_this_server_and_metadata_before_connecting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let transport = OnvifTransport::new(reqwest::Client::new());
        for url in [
            format!("http://127.0.0.1:{port}/onvif/event_service"),
            format!("http://localhost:{port}/onvif/event_service"),
            format!("http://[::ffff:127.0.0.1]:{port}/Subscription?Idx=1"),
            "http://169.254.169.254/latest/meta-data/".to_string(),
        ] {
            let err = transport
                .call(&url, "urn:x", "<x/>", None, Duration::from_secs(2))
                .await
                .unwrap_err();
            assert!(matches!(err, OnvifError::Refused(_)), "{url}: {err:?}");
        }
        let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(accepted.is_err(), "a request reached the listener");
    }

    fn response_of(len: usize) -> reqwest::Response {
        reqwest::Response::from(axum::http::Response::new(vec![b'a'; len]))
    }

    #[tokio::test]
    async fn a_response_past_the_size_cap_is_an_error() {
        let ok = read_capped(response_of(MAX_RESPONSE_BYTES)).await.unwrap();
        assert_eq!(ok.len(), MAX_RESPONSE_BYTES);
        let err = read_capped(response_of(MAX_RESPONSE_BYTES + 1))
            .await
            .unwrap_err();
        assert!(matches!(err, OnvifError::Parse(_)), "{err:?}");
    }

    #[test]
    fn envelope_without_header_omits_header_element() {
        let xml = envelope("", "<tds:GetDeviceInformation/>");
        assert!(xml.contains("<s:Envelope"));
        assert!(xml.contains("xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\""));
        assert!(!xml.contains("<s:Header>"));
        assert!(xml.contains("<s:Body><tds:GetDeviceInformation/></s:Body>"));
    }

    #[test]
    fn envelope_with_header_wraps_header() {
        let xml = envelope("<wsse:Security/>", "<x/>");
        assert!(xml.contains("<s:Header><wsse:Security/></s:Header>"));
        assert!(xml.contains("<s:Body><x/></s:Body>"));
    }

    #[test]
    fn parses_soap12_fault_with_prefix() {
        let xml = r#"<?xml version="1.0"?>
        <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope">
          <SOAP-ENV:Body>
            <SOAP-ENV:Fault>
              <SOAP-ENV:Code><SOAP-ENV:Value>SOAP-ENV:Sender</SOAP-ENV:Value></SOAP-ENV:Code>
              <SOAP-ENV:Reason><SOAP-ENV:Text xml:lang="en">Not Authorized</SOAP-ENV:Text></SOAP-ENV:Reason>
            </SOAP-ENV:Fault>
          </SOAP-ENV:Body>
        </SOAP-ENV:Envelope>"#;
        let (code, reason) = parse_soap_fault(xml).expect("fault parsed");
        assert_eq!(code, "SOAP-ENV:Sender");
        assert_eq!(reason, "Not Authorized");
    }

    #[test]
    fn parses_soap12_fault_default_namespace() {
        // No prefix on the SOAP elements (default xmlns).
        let xml = r#"<Envelope xmlns="http://www.w3.org/2003/05/soap-envelope">
          <Body><Fault>
            <Code><Value>Receiver</Value></Code>
            <Reason><Text>Internal Error</Text></Reason>
          </Fault></Body></Envelope>"#;
        let (code, reason) = parse_soap_fault(xml).expect("fault parsed");
        assert_eq!(code, "Receiver");
        assert_eq!(reason, "Internal Error");
    }

    #[test]
    fn fault_prefers_specific_subcode_over_top_level_code() {
        // The top-level Code/Value is the generic SOAP class `s:Sender`; the
        // ONVIF discriminator `ter:NotAuthorized` is in the Subcode. We must
        // surface the Subcode so auth detection works regardless of the Reason
        // language (here German).
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
          <s:Body><s:Fault>
            <s:Code>
              <s:Value>s:Sender</s:Value>
              <s:Subcode><s:Value>ter:NotAuthorized</s:Value></s:Subcode>
            </s:Code>
            <s:Reason><s:Text xml:lang="de">Zugriff verweigert</s:Text></s:Reason>
          </s:Fault></s:Body></s:Envelope>"#;
        let (code, reason) = parse_soap_fault(xml).expect("fault parsed");
        assert_eq!(code, "ter:NotAuthorized");
        assert_eq!(reason, "Zugriff verweigert");
    }

    #[test]
    fn fault_uses_innermost_nested_subcode() {
        // SOAP 1.2 Subcodes nest recursively; the innermost is the most
        // specific and must win.
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
          <s:Body><s:Fault>
            <s:Code>
              <s:Value>s:Sender</s:Value>
              <s:Subcode>
                <s:Value>ter:InvalidArgVal</s:Value>
                <s:Subcode><s:Value>ter:InvalidArgs</s:Value></s:Subcode>
              </s:Subcode>
            </s:Code>
            <s:Reason><s:Text>bad</s:Text></s:Reason>
          </s:Fault></s:Body></s:Envelope>"#;
        let (code, _) = parse_soap_fault(xml).expect("fault parsed");
        assert_eq!(code, "ter:InvalidArgs");
    }

    #[test]
    fn fault_without_subcode_uses_top_level_code() {
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
          <s:Body><s:Fault>
            <s:Code><s:Value>s:Receiver</s:Value></s:Code>
            <s:Reason><s:Text>boom</s:Text></s:Reason>
          </s:Fault></s:Body></s:Envelope>"#;
        let (code, _) = parse_soap_fault(xml).expect("fault parsed");
        assert_eq!(code, "s:Receiver");
    }

    #[test]
    fn envelope_declares_wsa_namespace() {
        let xml = envelope("", "<x/>");
        assert!(xml.contains("xmlns:wsa=\"http://www.w3.org/2005/08/addressing\""));
    }

    #[test]
    fn wsa_headers_carry_to_action_and_messageid() {
        // A subscription address with a query string must be XML-escaped.
        let h = wsa_headers(
            "http://cam/onvif/Subscription?id=0&x=1",
            "http://www.onvif.org/ver10/events/wsdl/PullMessages",
            "urn:uuid:abc",
        );
        assert!(h.contains(
            "<wsa:To s:mustUnderstand=\"1\">http://cam/onvif/Subscription?id=0&amp;x=1</wsa:To>"
        ));
        assert!(h.contains(
            "<wsa:Action s:mustUnderstand=\"1\">http://www.onvif.org/ver10/events/wsdl/PullMessages</wsa:Action>"
        ));
        assert!(h.contains("<wsa:MessageID>urn:uuid:abc</wsa:MessageID>"));
        assert!(h.contains("addressing/anonymous"));
    }

    #[test]
    fn non_fault_response_returns_none() {
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
          <s:Body><tds:GetDeviceInformationResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
            <tds:Manufacturer>Acme</tds:Manufacturer>
          </tds:GetDeviceInformationResponse></s:Body></s:Envelope>"#;
        assert!(parse_soap_fault(xml).is_none());
    }
}
