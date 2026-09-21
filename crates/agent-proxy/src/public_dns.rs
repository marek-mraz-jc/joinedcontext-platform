//! The egress resolver: a host on the allow-list reaches the public internet and nothing else
//! (AG-65, T-1304).
//!
//! The allow-list names hosts, and the name is all `checked` can see. What a name resolves to is
//! decided by whoever runs its DNS, so a listed host that answers with `127.0.0.1`, `10.x`, or
//! `169.254.169.254` would hand the run the cluster's own services and the cloud's metadata. The
//! addresses are therefore judged where they are used: `reqwest` connects only to what this
//! resolver returns, so there is no gap between the check and the connection for a second DNS
//! answer (a rebinding) to slip into, and every redirect hop is resolved through it again.

use std::error::Error;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// A name whose every address is one the egress must not reach.
#[derive(Debug)]
pub struct PrivateAddress {
    pub host: String,
}

impl fmt::Display for PrivateAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "host '{}' resolves only to private, loopback or link-local addresses; the fetch \
             route reaches public hosts only (AG-65)",
            self.host
        )
    }
}

impl Error for PrivateAddress {}

/// The system resolver with every non-public address removed; a name left with none fails.
#[derive(Debug, Default, Clone, Copy)]
pub struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = found.filter(|addr| is_public(addr.ip())).collect();
            if public.is_empty() {
                return Err(Box::new(PrivateAddress { host }) as Box<dyn Error + Send + Sync>);
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

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
    let [a, b, ..] = ip.octets();
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
        || (a == 192 && b == 0 && ip.octets()[2] == 0)
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

/// The resolver's refusal, when it is what failed a request: the fetch route answers it as a
/// refusal of the run's request rather than as an upstream that could not be reached.
pub fn refused<'a>(err: &'a (dyn Error + 'static)) -> Option<&'a PrivateAddress> {
    let mut cause: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(current) = cause {
        if let Some(found) = current.downcast_ref::<PrivateAddress>() {
            return Some(found);
        }
        cause = current.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_addresses_an_attacker_would_rebind_to_are_not_public() {
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
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a00:1",
            "::127.0.0.1",
        ] {
            assert!(!is_public(ip.parse().expect("an address")), "{ip}");
        }
    }

    #[test]
    fn a_public_address_is_public() {
        for ip in [
            "1.1.1.1",
            "151.101.1.69",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
            "64:ff9b::101:101",
        ] {
            assert!(is_public(ip.parse().expect("an address")), "{ip}");
        }
    }

    /// T-1304: `localhost` is the rebinding in miniature, a name whose DNS answer is loopback.
    #[tokio::test]
    async fn a_name_that_resolves_to_loopback_is_refused_by_the_resolver() {
        let err = match PublicOnly
            .resolve("localhost".parse().expect("a name"))
            .await
        {
            Ok(addrs) => panic!("resolved to {:?}", addrs.collect::<Vec<_>>()),
            Err(err) => err,
        };
        let found = refused(err.as_ref()).expect("the resolver's own refusal");
        assert_eq!(found.host, "localhost");
    }
}
