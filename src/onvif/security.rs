//! WS-Security UsernameToken (PasswordDigest) for ONVIF.
//!
//! ONVIF devices authenticate SOAP requests with the OASIS WS-Security
//! UsernameToken profile using a SHA-1 password digest:
//!
//! ```text
//! PasswordDigest = base64( SHA-1( nonce_bytes || created_utf8 || password_utf8 ) )
//! ```
//!
//! where `nonce_bytes` are the raw (decoded) nonce bytes, `created` is the
//! UTC ISO-8601 timestamp (the exact string carried in `<wsu:Created>`), and
//! `password` is the shared secret. The digest correctness is pinned by a
//! known-answer test against an independent oracle (see the test below).

use base64::Engine as _;
use rand::Rng as _;
use sha1::{Digest, Sha1};

use crate::onvif::types::Credentials;

/// Compute the WS-Security PasswordDigest.
///
/// Returns `base64( SHA-1( nonce || created || password ) )`, where `nonce`
/// is the raw nonce bytes (NOT the base64 text), `created` is the exact UTF-8
/// timestamp string placed in `<wsu:Created>`, and `password` is the secret.
pub fn password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(created.as_bytes());
    hasher.update(password.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// Generate a fresh `(nonce, created)` pair.
///
/// The nonce is 16 cryptographically-random bytes; `created` is the current
/// UTC time formatted as ISO-8601 with a trailing `Z` (e.g.
/// `2026-06-20T07:50:45Z`). Returns the raw nonce bytes (the caller is
/// responsible for base64-encoding it where the wire format requires it).
pub fn generate_nonce_created() -> (Vec<u8>, String) {
    let mut nonce = [0u8; 16];
    rand::rng().fill_bytes(&mut nonce);
    let created = jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    (nonce.to_vec(), created)
}

/// Build the `<wsse:Security>` SOAP header XML carrying a UsernameToken with a
/// PasswordDigest, base64 Nonce, and Created timestamp.
///
/// `nonce` is the raw nonce bytes; it is base64-encoded for the `<wsse:Nonce>`
/// element while the digest is computed over the raw bytes.
pub fn wsse_username_token(creds: &Credentials, nonce: &[u8], created: &str) -> String {
    let digest = password_digest(nonce, created, &creds.password);
    let nonce_b64 = base64::engine::general_purpose::STANDARD.encode(nonce);
    let username = xml_escape(&creds.username);

    format!(
        concat!(
            "<wsse:Security s:mustUnderstand=\"1\" ",
            "xmlns:wsse=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\" ",
            "xmlns:wsu=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd\">",
            "<wsse:UsernameToken>",
            "<wsse:Username>{username}</wsse:Username>",
            "<wsse:Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest\">{digest}</wsse:Password>",
            "<wsse:Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">{nonce}</wsse:Nonce>",
            "<wsu:Created>{created}</wsu:Created>",
            "</wsse:UsernameToken>",
            "</wsse:Security>",
        ),
        username = username,
        digest = digest,
        nonce = nonce_b64,
        created = xml_escape(created),
    )
}

/// Minimal XML text escaping for attribute/element content.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // KNOWN-ANSWER TEST.
    //
    // The expected digest is computed by an INDEPENDENT oracle, not by this
    // code, so a bug in `password_digest` cannot make the test agree with
    // itself. Oracle (python3 stdlib):
    //
    //   python3 -c "
    //   import hashlib, base64
    //   nonce = bytes([0x01,0x02,0x03,0x04,0x05,0x06,0x07,0x08,
    //                  0x09,0x0a,0x0b,0x0c,0x0d,0x0e,0x0f,0x10])
    //   created = '2010-09-16T07:50:45Z'
    //   password = 'userpassword'
    //   print(base64.b64encode(hashlib.sha1(
    //       nonce + created.encode() + password.encode()).digest()).decode())
    //   "
    //   => +Pguicb8KqpblLaMKmFs60b2s3k=
    //
    // Equivalent openssl pipeline:
    //   printf '\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x102010-09-16T07:50:45Zuserpassword' \
    //     | openssl dgst -sha1 -binary | openssl base64
    const KAT_NONCE: [u8; 16] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        0x10,
    ];
    const KAT_CREATED: &str = "2010-09-16T07:50:45Z";
    const KAT_PASSWORD: &str = "userpassword";
    const KAT_DIGEST: &str = "+Pguicb8KqpblLaMKmFs60b2s3k=";

    #[test]
    fn password_digest_matches_external_oracle() {
        let got = password_digest(&KAT_NONCE, KAT_CREATED, KAT_PASSWORD);
        assert_eq!(got, KAT_DIGEST);
    }

    #[test]
    fn wsse_token_embeds_digest_nonce_and_created() {
        let creds = Credentials::new("admin", KAT_PASSWORD);
        let xml = wsse_username_token(&creds, &KAT_NONCE, KAT_CREATED);
        // Digest computed over the raw nonce.
        assert!(xml.contains(KAT_DIGEST), "digest missing: {xml}");
        // Nonce carried base64-encoded.
        assert!(
            xml.contains("AQIDBAUGBwgJCgsMDQ4PEA=="),
            "base64 nonce missing: {xml}"
        );
        assert!(xml.contains("<wsu:Created>2010-09-16T07:50:45Z</wsu:Created>"));
        assert!(xml.contains("<wsse:Username>admin</wsse:Username>"));
        assert!(xml.contains("#PasswordDigest"));
    }

    #[test]
    fn generated_nonce_and_created_are_well_formed() {
        let (nonce, created) = generate_nonce_created();
        assert_eq!(nonce.len(), 16);
        assert!(created.ends_with('Z'));
        // Round-trips as an RFC3339/ISO-8601 instant.
        assert!(created.parse::<jiff::Timestamp>().is_ok());
    }

    #[test]
    fn username_is_xml_escaped() {
        let creds = Credentials::new("a<b&c", "pw");
        let (nonce, created) = generate_nonce_created();
        let xml = wsse_username_token(&creds, &nonce, &created);
        assert!(xml.contains("<wsse:Username>a&lt;b&amp;c</wsse:Username>"));
    }
}
