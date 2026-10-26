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
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        // ::/96, the deprecated IPv4-compatible form, and 2001:db8::/32, documentation.
        || segments[..6] == [0, 0, 0, 0, 0, 0]
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
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
        ] {
            assert!(is_public(ip.parse().expect("an address")), "{ip}");
        }
    }
}
