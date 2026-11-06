use super::*;

fn validate(url: &str) -> anyhow::Result<(String, Vec<SocketAddr>)> {
    validate_url_production(url, SsrfPolicy::PublicOnly, Duration::from_secs(10))
}

#[test]
fn validate_url_rejects_loopback() {
    assert!(validate("http://127.0.0.1:8080").is_err());
    assert!(validate("http://[::1]:8080").is_err());
}

#[test]
fn validate_url_rejects_private_ips() {
    assert!(validate("http://10.0.0.1:8080").is_err());
    assert!(validate("http://192.168.1.1:8080").is_err());
    assert!(validate("http://172.16.0.1:8080").is_err());
}

#[test]
fn validate_url_accepts_public_ips() {
    // IP literals only: a host name needs real DNS, which fails offline or on a slow resolver
    // (#3428).
    assert!(validate("http://8.8.8.8:8080").is_ok());
    assert!(validate("https://1.1.1.1:8080").is_ok());
}

#[test]
fn validate_url_rejects_ipv6_ula() {
    // IPv6 Unique Local Addresses (fc00::/7) are private and must be rejected
    // under PublicOnly (#191). Covers both halves of the prefix: fc00::/8 and
    // fd00::/8.
    assert!(validate("http://[fc00::1]:8080").is_err());
    assert!(validate("http://[fd00::1]:8080").is_err());
    assert!(validate("http://[fd12:3456:789a::1]:8080").is_err());
}

#[test]
fn validate_url_accepts_public_ipv6() {
    // Documentation range 2001:db8::/32 and a real public resolver address are
    // not in any blocked range, so they must pass.
    assert!(validate("http://[2001:db8::1]:8080").is_ok());
    assert!(validate("http://[2606:4700:4700::1111]:8080").is_ok());
}

#[test]
fn validate_url_rejects_ipv4_mapped_ipv6_loopback() {
    // An attacker can smuggle a blocked IPv4 through the v6 namespace as a
    // mapped address (::ffff:127.0.0.1). The blocklist must unwrap and reject
    // it rather than treat the v6 wrapper as public (#191).
    assert!(validate("http://[::ffff:127.0.0.1]:8080").is_err());
    assert!(validate("http://[::ffff:169.254.169.254]:8080").is_err());
    assert!(validate("http://[::ffff:10.0.0.1]:8080").is_err());
}

#[test]
fn validate_url_rejects_unspecified_and_metadata() {
    // 0.0.0.0 / :: route to localhost on many stacks; 169.254.169.254 is the
    // cloud metadata endpoint — both are classic SSRF targets (#191). Blocked
    // under both policies since neither has a legitimate memory-backend use.
    assert!(validate("http://0.0.0.0:8080").is_err());
    assert!(validate("http://[::]:8080").is_err());
    assert!(validate("http://169.254.169.254:8080").is_err());
    assert!(
        validate_url_production(
            "http://169.254.169.254:8080",
            SsrfPolicy::AllowPrivateNetwork,
            Duration::from_secs(2)
        )
        .is_err()
    );
}

#[test]
fn validate_url_allow_private_network_accepts_loopback_and_private() {
    // The ICM memory/transcript backend is expected to run on loopback or
    // the operator's LAN (AGENTS.md), so its client must not reject the
    // exact topology it's documented to support.
    for url in [
        "http://127.0.0.1:9092",
        "http://[::1]:9092",
        "http://10.0.0.1:9092",
        "http://192.168.1.1:9092",
        "http://172.16.0.1:9092",
        "http://[fc00::1]:9092",
    ] {
        assert!(
            validate_url_production(url, SsrfPolicy::AllowPrivateNetwork, Duration::from_secs(2))
                .is_ok(),
            "expected {url} to be allowed under AllowPrivateNetwork"
        );
    }
}

#[test]
fn validate_url_allow_private_network_still_rejects_link_local_and_special() {
    // Link-local (incl. the 169.254.169.254 cloud metadata address),
    // unspecified, and broadcast addresses have no legitimate use as a
    // memory backend regardless of trust policy.
    for url in [
        "http://169.254.169.254:9092",
        "http://[fe80::1]:9092",
        "http://0.0.0.0:9092",
        "http://[::]:9092",
    ] {
        assert!(
            validate_url_production(url, SsrfPolicy::AllowPrivateNetwork, Duration::from_secs(2))
                .is_err(),
            "expected {url} to still be rejected under AllowPrivateNetwork"
        );
    }
}

fn sock(ip: &str) -> SocketAddr {
    SocketAddr::new(ip.parse().unwrap(), 9092)
}

#[test]
fn domain_allow_private_drops_link_local_and_keeps_routable() {
    let resolved = vec![
        sock("fe80::c0:d5a8:4dfd:30bd"),
        sock("192.168.211.42"),
        sock("169.254.169.254"),
        sock("fe80::1"),
    ];
    let kept = drop_link_local_for_domain(
        resolved,
        SsrfPolicy::AllowPrivateNetwork,
        "http://still.local:9092/mcp",
    )
    .unwrap();
    assert_eq!(kept, vec![sock("192.168.211.42")]);
}

#[test]
fn domain_allow_private_rejects_when_only_link_local_remains() {
    let err = drop_link_local_for_domain(
        vec![sock("fe80::1"), sock("169.254.169.254")],
        SsrfPolicy::AllowPrivateNetwork,
        "http://still.local:9092/mcp",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("only to link-local"), "{err}");
}

#[test]
fn domain_public_only_keeps_link_local_so_the_caller_rejects() {
    let resolved = vec![sock("93.184.216.34"), sock("fe80::1")];
    let kept =
        drop_link_local_for_domain(resolved.clone(), SsrfPolicy::PublicOnly, "http://x.test/")
            .unwrap();
    assert_eq!(kept, resolved);
}

#[test]
fn domain_allow_private_still_rejects_unspecified_alongside_routable() {
    let kept = drop_link_local_for_domain(
        vec![sock("0.0.0.0"), sock("192.168.1.5")],
        SsrfPolicy::AllowPrivateNetwork,
        "http://x.test/",
    )
    .unwrap();
    // Not link-local, so it survives the drop and the caller's strict check rejects it.
    assert!(
        kept.iter()
            .any(|a| blocked_reason(&a.ip(), SsrfPolicy::AllowPrivateNetwork).is_some())
    );
}

#[test]
fn domain_allow_private_drops_ipv4_mapped_metadata_address() {
    let kept = drop_link_local_for_domain(
        vec![sock("::ffff:169.254.169.254"), sock("10.0.0.7")],
        SsrfPolicy::AllowPrivateNetwork,
        "http://x.test/",
    )
    .unwrap();
    assert_eq!(kept, vec![sock("10.0.0.7")]);
}

#[test]
fn domain_with_no_addresses_is_an_error() {
    assert!(
        drop_link_local_for_domain(vec![], SsrfPolicy::AllowPrivateNetwork, "http://x.test/")
            .is_err()
    );
}

#[test]
fn blocked_reason_flags_private_and_special_ranges() {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    // The blocklist is the single source of truth shared by literal-IP and
    // resolved-domain validation; assert it directly (#191).
    let blocked: &[IpAddr] = &[
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
        IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
    ];
    for ip in blocked {
        assert!(
            blocked_reason(ip, SsrfPolicy::PublicOnly).is_some(),
            "expected {ip} to be blocked under PublicOnly"
        );
    }
    let allowed: &[IpAddr] = &[
        IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
        IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
    ];
    for ip in allowed {
        assert!(
            blocked_reason(ip, SsrfPolicy::PublicOnly).is_none(),
            "expected {ip} to be allowed under PublicOnly"
        );
        assert!(
            blocked_reason(ip, SsrfPolicy::AllowPrivateNetwork).is_none(),
            "expected {ip} to be allowed under AllowPrivateNetwork"
        );
    }
}

#[test]
fn validate_url_returns_pinned_addrs_for_literal_ip() {
    // The TOCTOU fix pins reqwest to the addresses validation already vetted.
    // For a literal IP, the returned set is exactly that address, and the host
    // key matches the literal so the caller can pin without re-parsing (#191).
    let (host, addrs) = validate("http://8.8.8.8:8080").expect("public IP ok");
    assert_eq!(host, "8.8.8.8");
    assert!(
        addrs
            .iter()
            .any(|a| a.ip().to_string() == "8.8.8.8" && a.port() == 8080)
    );
}

#[test]
fn validate_url_returns_unbracketed_host_for_literal_ipv6() {
    // resolve_to_addrs keys on the unbracketed host; a re-parse via host_str
    // would yield the bracketed form and pin the wrong key. Returning the host
    // from the validating parse keeps the two in lockstep (#191).
    let (host, addrs) = validate("http://[2606:4700:4700::1111]:8080").expect("public IPv6 ok");
    assert_eq!(host, "2606:4700:4700::1111");
    assert!(addrs.iter().any(|a| a.port() == 8080));
}

#[test]
fn validate_url_rejects_domain_resolving_to_loopback() {
    // DNS-rebinding TOCTOU: a hostname that resolves to a blocked address must
    // be rejected at validation time, before any request is sent. localhost is
    // the always-available stand-in for an attacker-controlled rebind (#191).
    // localhost resolves from the hosts file, with no DNS server. The error must name the
    // blocked address, so a resolver failure cannot pass this test (#3428).
    let err = validate("http://localhost:8080").unwrap_err().to_string();
    assert!(err.contains("not allowed (SSRF)"), "{err}");
}

#[test]
fn validate_url_rejects_unsupported_schemes() {
    assert!(validate("file:///tmp/socket").is_err());
    assert!(validate("ftp://example.com").is_err());
}
