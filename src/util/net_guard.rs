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

#[cfg(test)]
mod tests {
    use super::*;

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
