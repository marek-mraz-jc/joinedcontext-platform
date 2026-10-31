//! Whether an IP address is on the public internet (T-4776, AG-65): the one rule the outbound
//! guard applies to a resolved address and manifest validation applies to an address written
//! into a URL (F-PLT2-019).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Whether an address is on the public internet: not loopback, private, link-local, shared
/// (carrier-grade NAT), unspecified, broadcast, multicast or reserved, in either family, and not
/// an IPv6 address that embeds one of those IPv4 addresses.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        // 100.64.0.0/10, shared address space (RFC 6598): what a cluster's pods often live in.
        || (a == 100 && (64..128).contains(&b))
        // 192.0.0.0/24, IETF protocol assignments; 198.18.0.0/15, benchmarking; 240/4, reserved.
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    // 64:ff9b::/96, the NAT64 prefix: the address behind it is the IPv4 one that is reached.
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        let o = ip.octets();
        return is_public_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // 2002::/16, 6to4: the IPv4 address in its next 32 bits is the one reached (F-R2PLT2-022).
    if segments[0] == 0x2002 {
        let o = ip.octets();
        return is_public_v4(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || ip.is_unique_local()
        // 2001::/32, Teredo, tunnels to an IPv4 address it hides; fec0::/10, deprecated site-local.
        || (segments[0] == 0x2001 && segments[1] == 0)
        || (segments[0] & 0xffc0) == 0xfec0
        || ip.is_unicast_link_local()
        // ::/96, the deprecated IPv4-compatible form, and 2001:db8::/32, documentation.
        || segments[..6] == [0, 0, 0, 0, 0, 0]
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

/// The host of an authority past its userinfo: without the port, the scp form's `:path`
/// (`git@host:org/repo`) or an IPv6 literal's brackets' tail.
pub(crate) fn remote_host(host_port: &str) -> &str {
    match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host_port.split(':').next().unwrap_or_default(),
    }
}

/// Names that answer inside the installation, never on the internet: `localhost` (RFC 6761),
/// mDNS (RFC 6762), the home network (RFC 8375), Kubernetes Services and the usual private zones.
const INTERNAL_SUFFIXES: [&str; 7] = [
    "localhost",
    "local",
    "localdomain",
    "internal",
    "lan",
    "home.arpa",
    "svc",
];

/// Refuses a host no remote origin can have (F-PLT2-019): an address written into the URL that
/// is not on the public internet, in any form a URL parser reads as one (`2130706433`,
/// `0x7f.1`, `[::ffff:127.0.0.1]`), a name of one label (an in-cluster Service) or under an
/// internal suffix, and anything a parser would decode into another host (`%`, non-ASCII).
/// What a public name resolves to is judged when it is fetched, by the outbound guard.
pub(crate) fn check_remote_host(host: &str) -> std::result::Result<(), &'static str> {
    const PRIVATE: &str = "the host is a loopback, private, link-local or internal address; a \
                           remote names a public host such as git.example.org";
    if host.is_empty() {
        return Err("url names no host, e.g. https://git.example.org/org/repo.git");
    }
    if host.contains(':') {
        return match host.parse::<std::net::Ipv6Addr>() {
            Ok(ip) if is_public(ip.into()) => Ok(()),
            Ok(_) => Err(PRIVATE),
            Err(_) => Err("the host is not a valid IPv6 address"),
        };
    }
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    if !host
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
    {
        return Err(
            "the host holds only letters, digits, dots and hyphens; an internationalised \
                    name is written in its xn-- form",
        );
    }
    let last = host.rsplit('.').next().unwrap_or_default();
    // A URL parser reads a host whose last label is a number as an IPv4 address.
    if last.starts_with("0x") || last.bytes().all(|b| b.is_ascii_digit()) {
        return match host.parse::<std::net::Ipv4Addr>() {
            Ok(ip) if is_public(ip.into()) => Ok(()),
            Ok(_) => Err(PRIVATE),
            Err(_) => Err("an IPv4 address is written in dotted decimal, e.g. 203.0.113.7"),
        };
    }
    let internal = INTERNAL_SUFFIXES.iter().any(|suffix| {
        host == *suffix
            || host
                .strip_suffix(suffix)
                .is_some_and(|rest| rest.ends_with('.'))
    });
    if internal || !host.contains('.') {
        return Err(PRIVATE);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_private_range_of_either_family_is_not_public() {
        for ip in [
            "127.0.0.1",
            "10.43.0.10",
            "172.16.5.4",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.0.8",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a00:1",
            "::127.0.0.1",
            "2001:db8::1",
            "2002:7f00:1::",
            "2002:a00:1::1",
            "2001:0:4136:e378::1",
            "fec0::1",
        ] {
            assert!(!is_public(ip.parse().expect("an address")), "{ip}");
        }
    }

    #[test]
    fn a_public_address_is_public() {
        for ip in [
            "1.1.1.1",
            "116.203.134.249",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
            "64:ff9b::101:101",
            "2002:101:101::1",
        ] {
            assert!(is_public(ip.parse().expect("an address")), "{ip}");
        }
    }
}
