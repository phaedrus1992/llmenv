//! Cleartext (`http://`) address policy shared by the MCP resolver and the MCP client.
//!
//! One copy of the "is this address the operator's own network" rule keeps the agent's
//! rendered MCP config and llmenv's own client from disagreeing (#2476, #2483).

use std::net::IpAddr;

use url::{Host, Url};

/// The IPv4 address that an IPv6 address wraps, if any: IPv4-mapped (`::ffff:a.b.c.d`),
/// IPv4-compatible (`::a.b.c.d`), NAT64 (`64:ff9b::/96`), and 6to4 (`2002::/16`).
///
/// A NAT64 gateway or a 6to4 relay forwards such an address to the wrapped IPv4 host, so
/// the SSRF gate must judge the wrapped address, not the IPv6 wrapper (#2476).
pub(crate) fn embedded_ipv4(v6: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let seg = v6.segments();
    let low = |hi: u16, lo: u16| {
        std::net::Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
    };
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return Some(mapped);
    }
    // `::` and `::1` have an all-zero prefix too, but they are the unspecified and loopback
    // addresses, not wrapped IPv4 addresses.
    if seg[..6].iter().all(|&s| s == 0) && !v6.is_unspecified() && !v6.is_loopback() {
        return Some(low(seg[6], seg[7]));
    }
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6].iter().all(|&s| s == 0) {
        return Some(low(seg[6], seg[7]));
    }
    if seg[0] == 0x2002 {
        return Some(low(seg[1], seg[2]));
    }
    None
}

/// Whether `v6` is in the unique-local range `fc00::/7`.
///
/// `Ipv6Addr::is_unique_local` is unstable, so test the prefix directly: the
/// top seven bits are `1111110`, i.e. the first byte is `0xfc` or `0xfd` (#191).
pub(crate) fn is_unique_local_v6(v6: &std::net::Ipv6Addr) -> bool {
    (v6.octets()[0] & 0xfe) == 0xfc
}

/// Whether a bearer token or memory payload may cross the network in clear text to `ip`.
///
/// Loopback, private, unique-local, and CGNAT (100.64.0.0/10, used by Tailscale) addresses
/// count as the operator's own network. Everything else is public.
///
/// Only an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) is judged by the IPv4 address it wraps.
/// A 6to4 or NAT64 address leaves the host through a public relay, so a private IPv4 address
/// inside one is still public for a cleartext decision (RFC 3056, RFC 6052).
pub fn is_cleartext_safe(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let cgnat = v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40;
            v4.is_loopback() || v4.is_private() || cgnat
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(mapped) => is_cleartext_safe(&IpAddr::V4(mapped)),
            None => v6.is_loopback() || is_unique_local_v6(v6),
        },
    }
}

/// The literal IP that `url` would send in clear text to a public address, if any.
///
/// Returns `Some` only for an `http://` URL whose host is an IP literal outside the
/// operator's own network. A hostname is not resolved here: resolution is slow and
/// runs on every config render, so `llmenv doctor` checks hostnames instead. An
/// unparseable URL returns `None`; the resolver passes URLs through unchanged (#1017).
#[must_use]
pub(crate) fn public_cleartext_literal(url: &str) -> Option<IpAddr> {
    let parsed = Url::parse(url).ok()?;
    if parsed.scheme() != "http" {
        return None;
    }
    let ip = match parsed.host()? {
        Host::Ipv4(v4) => IpAddr::V4(v4),
        Host::Ipv6(v6) => IpAddr::V6(v6),
        Host::Domain(_) => return None,
    };
    (!is_cleartext_safe(&ip)).then_some(ip)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn flags_only_http_to_a_public_ip_literal() {
        let ip = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        assert_eq!(
            public_cleartext_literal("http://8.8.8.8:80/mcp"),
            ip("8.8.8.8")
        );
        assert_eq!(
            public_cleartext_literal("http://[2001:db8::1]/mcp"),
            ip("2001:db8::1")
        );
        assert_eq!(
            public_cleartext_literal("http://[::ffff:8.8.8.8]/"),
            ip("::ffff:8.8.8.8")
        );
    }

    #[test]
    fn allows_the_operators_own_network_https_and_hostnames() {
        for url in [
            "http://10.0.0.4/mcp",
            "http://127.0.0.1:9092/mcp",
            "http://100.64.0.1/mcp",
            "http://[fd00::1]/mcp",
            "http://[::1]/mcp",
            "https://8.8.8.8/mcp",
            "http://example.com/mcp",
            "not a url",
        ] {
            assert_eq!(public_cleartext_literal(url), None, "{url}");
        }
    }

    #[test]
    fn a_private_ipv4_inside_a_relay_prefix_is_still_public() {
        // 6to4 (2002::/16), NAT64 (64:ff9b::/96), and IPv4-compatible forms cross a public relay.
        for url in [
            "http://[2002:c0a8:105::1]/mcp",
            "http://[64:ff9b::a00:4]/mcp",
            "http://[::10.0.0.4]/mcp",
        ] {
            assert!(public_cleartext_literal(url).is_some(), "{url}");
        }
        // An IPv4-mapped address stays on the host's own IPv4 stack.
        assert_eq!(
            public_cleartext_literal("http://[::ffff:10.0.0.4]/mcp"),
            None
        );
        assert!(public_cleartext_literal("http://[::ffff:8.8.8.8]/mcp").is_some());
    }

    proptest::proptest! {
        #[test]
        fn an_http_ip_literal_is_flagged_iff_it_is_not_cleartext_safe(octets in proptest::prelude::any::<[u8; 16]>(), v4 in proptest::prelude::any::<u32>(), use_v6 in proptest::prelude::any::<bool>()) {
            let (ip, host) = if use_v6 {
                let ip = IpAddr::V6(std::net::Ipv6Addr::from(octets));
                (ip, format!("[{ip}]"))
            } else {
                let ip = IpAddr::V4(std::net::Ipv4Addr::from(v4));
                (ip, ip.to_string())
            };
            let flagged = public_cleartext_literal(&format!("http://{host}:8080/mcp"));
            proptest::prop_assert_eq!(flagged, (!is_cleartext_safe(&ip)).then_some(ip));
            // TLS protects the payload, so https is never flagged.
            proptest::prop_assert_eq!(public_cleartext_literal(&format!("https://{host}/mcp")), None);
        }

        #[test]
        fn the_url_check_never_panics(url in "\\PC{0,200}") {
            let _ = public_cleartext_literal(&url);
        }
    }

    #[test]
    fn scheme_case_does_not_bypass_the_check() {
        assert!(public_cleartext_literal("HTTP://8.8.8.8/mcp").is_some());
    }
}
