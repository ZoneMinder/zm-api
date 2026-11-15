//! Addresses zm-api must not send requests to on a user's behalf: this server
//! itself and cloud metadata services. Used wherever a stored or requested
//! address makes the server connect somewhere (ONVIF inspect, PTZ).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Cloud metadata services (AWS, GCP, Azure, ...; AWS's IPv6 one), which
/// hand out credentials to whatever asks from inside the instance.
pub const METADATA: [IpAddr; 2] = [
    IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
    IpAddr::V6(Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)),
];

/// This host's interface addresses. A service bound to all interfaces
/// answers on them as it does on loopback.
pub fn own_addresses() -> Vec<IpAddr> {
    let Ok(addrs) = nix::ifaddrs::getifaddrs() else {
        return Vec::new();
    };
    addrs
        .filter_map(|a| a.address)
        .filter_map(|sa| {
            if let Some(v4) = sa.as_sockaddr_in() {
                Some(IpAddr::V4(v4.ip()))
            } else {
                sa.as_sockaddr_in6().map(|v6| IpAddr::V6(v6.ip()))
            }
        })
        .collect()
}

/// Whether `address` names a cloud metadata service outright: its literal
/// address, or one of the names providers give it. No lookup is made, so this
/// suits request validation; it does not catch a name of the caller's own
/// that resolves there.
pub fn names_metadata_service(address: &str) -> bool {
    match address_host(address) {
        Some((url::Host::Ipv4(v4), _)) => METADATA.contains(&IpAddr::V4(v4)),
        Some((url::Host::Ipv6(v6), _)) => {
            let ip = v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4);
            METADATA.contains(&ip)
        }
        Some((url::Host::Domain(name), _)) => {
            // Only http(s) URLs get their host parsed as an address; in an
            // `rtsp://` one an IP arrives here as a name.
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            let bare = name.trim_start_matches('[').trim_end_matches(']');
            match bare.parse::<IpAddr>() {
                Ok(IpAddr::V6(v6)) => {
                    METADATA.contains(&v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4))
                }
                Ok(ip) => METADATA.contains(&ip),
                Err(_) => {
                    name == "metadata.google.internal"
                        || name == "metadata"
                        || name == "instance-data"
                }
            }
        }
        None => false,
    }
}

/// Whether `ip` is this server (loopback, the unspecified address, one of
/// `own`) or a cloud metadata service. IPv4-mapped IPv6 counts as IPv4.
pub fn is_server_or_metadata(ip: IpAddr, own: &[IpAddr]) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    ip.is_loopback() || ip.is_unspecified() || METADATA.contains(&ip) || own.contains(&ip)
}

/// The host and port a device address points at. Takes a URL
/// (`http://host:port/path`) or a bare `host[:port][/path]`, as
/// `ControlAddress` holds. `None` when there is no network host (a serial
/// device path, or an empty value).
pub fn address_host(address: &str) -> Option<(url::Host<String>, u16)> {
    let address = address.trim();
    if address.is_empty() || address.starts_with('/') {
        return None;
    }
    let url = if address.contains("://") {
        url::Url::parse(address).ok()?
    } else {
        url::Url::parse(&format!("http://{address}")).ok()?
    };
    let port = url.port_or_known_default().unwrap_or(80);
    Some((url.host()?.to_owned(), port))
}

/// The first address `address` reaches that is this server or a metadata
/// service, resolving a host name. A name that doesn't resolve passes: the
/// connection would fail anyway.
pub async fn forbidden_target(address: &str, own: &[IpAddr]) -> Option<IpAddr> {
    let (host, port) = address_host(address)?;
    let ips: Vec<IpAddr> = match host {
        url::Host::Ipv4(v4) => vec![IpAddr::V4(v4)],
        url::Host::Ipv6(v6) => vec![IpAddr::V6(v6)],
        url::Host::Domain(name) => match tokio::net::lookup_host((name.as_str(), port)).await {
            Ok(addrs) => addrs.map(|a| a.ip()).collect(),
            Err(_) => Vec::new(),
        },
    };
    ips.into_iter().find(|&ip| is_server_or_metadata(ip, own))
}

/// What a checked address resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum Checked {
    /// A literal IP (or no network host at all): nothing to pin.
    Literal,
    /// A host name and the addresses it resolved to, all allowed. Connect to
    /// these and not to the name: a second lookup may answer differently
    /// (DNS rebinding), handing the request to an address this check refused.
    Name(String, Vec<std::net::SocketAddr>),
}

/// Check `address` as [`forbidden_target`] does and say what it resolved to,
/// so the caller can connect to exactly the addresses that were checked.
/// `Err` holds the forbidden address.
pub async fn check_and_resolve(address: &str, own: &[IpAddr]) -> Result<Checked, IpAddr> {
    let Some((host, port)) = address_host(address) else {
        return Ok(Checked::Literal);
    };
    let name = match host {
        url::Host::Ipv4(v4) => return literal(IpAddr::V4(v4), own),
        url::Host::Ipv6(v6) => return literal(IpAddr::V6(v6), own),
        url::Host::Domain(name) => name,
    };
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((name.as_str(), port))
        .await
        .map(|a| a.collect())
        .unwrap_or_default();
    match addrs.iter().find(|a| is_server_or_metadata(a.ip(), own)) {
        Some(bad) => Err(bad.ip()),
        None => Ok(Checked::Name(name, addrs)),
    }
}

fn literal(ip: IpAddr, own: &[IpAddr]) -> Result<Checked, IpAddr> {
    if is_server_or_metadata(ip, own) {
        Err(ip)
    } else {
        Ok(Checked::Literal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_services_are_recognised_by_address_and_name() {
        for yes in [
            "http://169.254.169.254/latest/meta-data/",
            "rtsp://169.254.169.254:554/x",
            "http://[::ffff:169.254.169.254]/",
            "http://metadata.google.internal/computeMetadata/v1/",
        ] {
            assert!(names_metadata_service(yes), "{yes}");
        }
        for no in [
            "rtsp://192.168.1.10/stream",
            "http://127.0.0.1:8554/cam",
            "/dev/video0",
            "",
        ] {
            assert!(!names_metadata_service(no), "{no}");
        }
    }

    #[tokio::test]
    async fn a_checked_name_comes_back_with_the_addresses_it_was_checked_as() {
        // Literals need no pinning.
        assert_eq!(
            check_and_resolve("http://192.168.1.10/onvif", &[]).await,
            Ok(Checked::Literal)
        );
        assert!(check_and_resolve("http://127.0.0.1/onvif", &[])
            .await
            .is_err());
        // `localhost` resolves, to loopback, which is refused.
        assert!(check_and_resolve("http://localhost:8080/onvif", &[])
            .await
            .is_err());
        // A name that doesn't resolve is pinned to nothing, so the request
        // can't go anywhere unchecked.
        assert_eq!(
            check_and_resolve("http://no-such-host.invalid/onvif", &[]).await,
            Ok(Checked::Name("no-such-host.invalid".into(), vec![]))
        );
    }

    #[tokio::test]
    async fn refuses_the_server_and_metadata_however_written() {
        for address in [
            "http://127.0.0.1/onvif/ptz_service",
            "127.0.0.1:8080",
            "127.0.0.1",
            "169.254.169.254",
            "http://[::1]:80/",
            "[::1]",
            "http://[::ffff:127.0.0.1]/",
            "0.0.0.0:22",
            "localhost:3306",
            "https://[fd00:ec2::254]/",
        ] {
            assert!(
                forbidden_target(address, &[]).await.is_some(),
                "{address} should be refused"
            );
        }
        let own: [IpAddr; 1] = ["192.168.7.7".parse().unwrap()];
        assert!(forbidden_target("192.168.7.7:80", &own).await.is_some());
    }

    #[tokio::test]
    async fn lets_cameras_through() {
        for address in [
            "http://192.168.1.10/onvif/ptz_service",
            "192.168.1.10:80",
            "10.0.0.5",
            "http://[fe80::1]/",
            "/dev/ttyUSB0",
            "",
            "camera.invalid:80",
        ] {
            assert_eq!(forbidden_target(address, &[]).await, None, "{address}");
        }
    }
}
