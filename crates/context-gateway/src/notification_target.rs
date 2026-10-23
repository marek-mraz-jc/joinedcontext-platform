//! Where a notification may be sent (T-1302, G11, T-3653).
//!
//! A subscription names its subscriber, so the address is the caller's choice. One read off the
//! URL alone may not point at this machine or inside the platform's networks, unless the
//! installation named the host (`JC_GATEWAY_EGRESS_PRIVATE_HOSTS`). The URL is read by the parser
//! the delivering client uses (WHATWG, `reqwest::Url`), so `http://127.0.0.1?x`, `http://0x7f.1/`
//! and `http://2130706433/` are the loopback address they dial, not a name that passes. A name is
//! resolved where the notification leaves.

use jc_core::outbound::is_public;
use jc_core::ProblemDetails;
use reqwest::Url;
use std::net::IpAddr;

/// Why a notification endpoint may not be stored, or `None` when it may.
pub(crate) fn refusal(uri: &str, private_hosts: &[String]) -> Option<ProblemDetails> {
    let refused = |why: &str| Some(ProblemDetails::bad_request().with_detail(why.to_owned()));
    let Ok(url) = Url::parse(uri) else {
        return refused("the notification endpoint is not a URL");
    };
    if !matches!(url.scheme(), "http" | "https") {
        return refused("a notification endpoint has to be an HTTP or HTTPS URL");
    }
    let Some((host, _)) = host_of(&url) else {
        return refused("the notification endpoint names no host");
    };
    if private_hosts.contains(&host) {
        return None;
    }
    let internal = host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| !is_public(ip));
    match internal {
        true => refused("a notification endpoint may not point inside the platform's own networks"),
        false => None,
    }
}

/// The host and port a URL dials: lower case, no trailing dot, an IPv6 address without its
/// brackets, the scheme's port when none is written.
pub(crate) fn host_of(url: &Url) -> Option<(String, u16)> {
    let host = url.host_str()?;
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host)
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let port = url.port_or_known_default()?;
    (!host.is_empty()).then_some((host, port))
}

#[cfg(test)]
mod tests {
    use super::refusal;

    /// T-3653: every spelling of an internal address is the address it dials. The first three
    /// passed the old string split (`?` kept in the host, hex and decimal IPv4 read as names).
    #[test]
    fn every_spelling_of_an_internal_address_is_refused() {
        for uri in [
            "http://127.0.0.1?x",
            "http://0x7f.1/",
            "http://2130706433/",
            "http://127.0.0.1#frag",
            "http://user@127.0.0.1:9090/q/admin",
            "http://[::1]/",
            "http://[::ffff:10.0.0.7]/",
            "http://[fe80::1%25eth0]/",
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1/",
            "http://localhost./",
            "http://api.localhost/",
            "HTTP://LOCALHOST/",
        ] {
            assert!(refusal(uri, &[]).is_some(), "{uri} passed");
        }
    }

    /// T-5335: the platform's one guard ([`jc_core::outbound::is_public`]) judges the address, so
    /// the ranges the gateway's own list missed are refused here too.
    #[test]
    fn what_the_platform_guard_refuses_is_refused_here() {
        for uri in [
            "http://[64:ff9b::7f00:1]/",
            "http://[::127.0.0.1]/",
            "http://192.0.0.8/",
            "http://198.18.0.1/",
            "http://240.0.0.1/",
            "http://224.0.0.1/",
            "http://203.0.113.9:8080/in",
            "https://[2001:db8::1]/in",
        ] {
            assert!(refusal(uri, &[]).is_some(), "{uri} passed");
        }
    }

    #[test]
    fn a_public_address_or_a_named_private_host_passes() {
        for uri in [
            "https://hooks.example.com/notify",
            "http://93.184.216.34:8080/in",
            "https://[2606:4700:4700::1111]/in",
        ] {
            assert!(refusal(uri, &[]).is_none(), "{uri} refused");
        }
        let named = ["receiver.internal".to_owned()];
        assert!(refusal("http://receiver.internal/in", &named).is_none());
    }

    #[test]
    fn what_is_no_http_url_is_refused_with_why() {
        for (uri, why) in [
            ("mqtt://broker.example/topic", "HTTP or HTTPS"),
            ("not a url", "not a URL"),
            ("file:///etc/passwd", "HTTP or HTTPS"),
        ] {
            let problem = refusal(uri, &[]).expect("refused");
            assert!(
                problem.detail.as_deref().unwrap_or_default().contains(why),
                "{uri}"
            );
        }
    }
}
