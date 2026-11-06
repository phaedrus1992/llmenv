use super::*;
use proptest::prelude::*;

#[test]
fn new_rejects_plain_http_to_a_public_address() {
    let err = McpHttpClient::new("http://8.8.8.8:9092/mcp".into(), Duration::from_secs(2))
        .expect_err("cleartext to a public address must be refused")
        .to_string();
    assert!(err.contains("http://8.8.8.8:9092/mcp"), "{err}");
    assert!(err.contains("https://"), "{err}");
}

#[test]
fn new_error_for_plain_http_hides_credentials_and_query() {
    let err = McpHttpClient::new(
        "http://user:pw@8.8.8.8:9/mcp?token=secret".into(),
        Duration::from_secs(2),
    )
    .expect_err("public http")
    .to_string();
    assert!(!err.contains("pw") && !err.contains("secret"), "{err}");
}

#[test]
fn new_allows_plain_http_to_loopback_and_private_addresses() {
    for url in [
        "http://127.0.0.1:9092",
        "http://[::1]:9092",
        "http://10.1.2.3:9092",
        "http://192.168.1.5:9092",
        "http://172.16.0.9:9092",
        "http://[fd00::1]:9092",
        "http://100.64.0.7:9092",
        "http://localhost:9092",
    ] {
        McpHttpClient::new(url.into(), Duration::from_secs(2))
            .unwrap_or_else(|e| panic!("{url} must be allowed: {e}"));
    }
}

#[test]
fn new_allows_https_to_a_public_address() {
    McpHttpClient::new("https://8.8.8.8:443/mcp".into(), Duration::from_secs(2))
        .expect("https to a public address is allowed");
}

#[test]
fn new_rejects_a_malformed_url() {
    assert!(McpHttpClient::new("http//not a url".into(), Duration::from_secs(2)).is_err());
}

#[test]
fn invalid_url_errors_do_not_echo_credentials_or_query() {
    for url in [
        "http://user:pw@:9/mcp?token=secret",
        "ftp://user:pw@host/?token=secret",
    ] {
        let err = format!(
            "{:#}",
            McpHttpClient::new(url.into(), Duration::from_secs(2)).expect_err(url)
        );
        assert!(!err.contains("pw") && !err.contains("secret"), "{err}");
    }
}

#[test]
fn plain_http_error_names_the_host() {
    let err = McpHttpClient::new("http://8.8.8.8:9/mcp".into(), Duration::from_secs(2))
        .expect_err("public http")
        .to_string();
    assert!(err.contains("8.8.8.8"), "{err}");
}

#[test]
fn redact_url_strips_userinfo_query_and_fragment() {
    assert_eq!(redact_url("http://u:p@h:1/x?q=1#f"), "http://h:1/x");
    assert_eq!(redact_url("not a url"), "(invalid URL)");
}

#[test]
fn require_https_fails_closed_on_an_unparseable_url() {
    let addr: SocketAddr = "8.8.8.8:80".parse().unwrap();
    assert!(require_https_for_public("not a url", &[addr]).is_err());
}

#[test]
fn cleartext_check_judges_wrapped_ipv4_in_ipv6() {
    use std::net::{IpAddr, Ipv6Addr};
    let ip = |s: &str| IpAddr::V6(s.parse::<Ipv6Addr>().unwrap());
    assert!(!is_cleartext_safe(&ip("::ffff:8.8.8.8")));
    assert!(is_cleartext_safe(&ip("::ffff:10.0.0.1")));
    assert!(!is_cleartext_safe(&ip("64:ff9b::808:808")));
    assert!(!is_cleartext_safe(&ip("2002:808:808::1")));
    assert!(!is_cleartext_safe(&ip("fe80::1")));
}

#[test]
fn embedded_ipv4_extracts_the_exact_wrapped_address() {
    use std::net::{Ipv4Addr, Ipv6Addr};
    let got = |s: &str| embedded_ipv4(&s.parse::<Ipv6Addr>().unwrap());
    let want = Some(Ipv4Addr::new(1, 2, 3, 4));
    assert_eq!(got("::ffff:1.2.3.4"), want);
    assert_eq!(got("::1.2.3.4"), want);
    assert_eq!(got("64:ff9b::102:304"), want);
    assert_eq!(got("2002:102:304::"), want);
}

#[test]
fn embedded_ipv4_ignores_addresses_that_only_look_wrapped() {
    use std::net::Ipv6Addr;
    let got = |s: &str| embedded_ipv4(&s.parse::<Ipv6Addr>().unwrap());
    for s in [
        "::",
        "::1",
        "64:1::1",
        "1:ff9b::1",
        "64:ff9b:0:0:1::1",
        "2001:db8::1",
    ] {
        assert_eq!(got(s), None, "{s}");
    }
}

#[test]
fn blocked_reason_sees_metadata_address_behind_nat64_6to4_and_compat() {
    use std::net::{IpAddr, Ipv6Addr};
    for s in [
        "64:ff9b::a9fe:a9fe",
        "2002:a9fe:a9fe::",
        "::169.254.169.254",
    ] {
        let ip = IpAddr::V6(s.parse::<Ipv6Addr>().unwrap());
        assert!(
            blocked_reason(&ip, SsrfPolicy::AllowPrivateNetwork).is_some(),
            "{s}"
        );
    }
    let public = IpAddr::V6("64:ff9b::808:808".parse::<Ipv6Addr>().unwrap());
    assert!(blocked_reason(&public, SsrfPolicy::AllowPrivateNetwork).is_none());
    let unspecified = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
    assert!(blocked_reason(&unspecified, SsrfPolicy::AllowPrivateNetwork).is_some());
}

proptest::proptest! {
    #[test]
    fn prop_cleartext_safe_matches_std_classification_for_ipv4(octets in proptest::array::uniform4(0u8..)) {
        let ip = std::net::Ipv4Addr::from(octets);
        let cgnat = octets[0] == 100 && (64..=127).contains(&octets[1]);
        let expected = ip.is_loopback() || ip.is_private() || cgnat;
        proptest::prop_assert_eq!(is_cleartext_safe(&IpAddr::V4(ip)), expected);
    }

    #[test]
    fn prop_redact_url_is_idempotent_and_hides_secrets(
        user in "[a-z]{1,8}", pw in "[A-Z]{6,10}", q in "[0-9]{6,10}",
    ) {
        let url = format!("http://{user}:{pw}@example.com/mcp?token={q}#frag");
        let once = redact_url(&url);
        proptest::prop_assert!(!once.contains(&pw) && !once.contains(&q));
        proptest::prop_assert_eq!(redact_url(&once), once);
    }
}

#[test]
fn cleartext_safe_edges_for_cgnat() {
    use std::net::{IpAddr, Ipv4Addr};
    for ok in [
        Ipv4Addr::new(100, 64, 0, 1),
        Ipv4Addr::new(100, 127, 255, 254),
    ] {
        assert!(is_cleartext_safe(&IpAddr::V4(ok)), "{ok}");
    }
    for bad in [
        Ipv4Addr::new(100, 63, 255, 255),
        Ipv4Addr::new(100, 128, 0, 1),
    ] {
        assert!(!is_cleartext_safe(&IpAddr::V4(bad)), "{bad}");
    }
}

/// Test convenience: `SsrfPolicy::PublicOnly` with a generous timeout,
/// matching every pre-existing test's expectations before `SsrfPolicy` and
/// per-call DNS timeouts were introduced. 10s, not 2s (#1236): a real DNS
/// lookup contending
/// with the rest of a `--workspace` parallel test run for CPU/network
/// blew a 2s budget intermittently — this is test-only slack, not a
/// change to production's own timeout policy.
fn any_policy() -> impl Strategy<Value = SsrfPolicy> {
    prop_oneof![
        Just(SsrfPolicy::PublicOnly),
        Just(SsrfPolicy::AllowPrivateNetwork),
    ]
}

proptest! {
    #[test]
    fn prop_blocked_reason_never_panics(octets in any::<[u8; 16]>(), v4 in any::<[u8; 4]>(), policy in any_policy()) {
        // The SSRF gate must total over every possible address, under either
        // policy (#191).
        let _ = blocked_reason(&IpAddr::V6(std::net::Ipv6Addr::from(octets)), policy);
        let _ = blocked_reason(&IpAddr::V4(std::net::Ipv4Addr::from(v4)), policy);
    }

    #[test]
    fn prop_is_unique_local_v6_matches_fc00_slash_7(octets in any::<[u8; 16]>()) {
        // The hand-rolled prefix test must agree with the fc00::/7 definition:
        // first byte 0xfc or 0xfd (the unstable std is_unique_local).
        let v6 = std::net::Ipv6Addr::from(octets);
        let expected = matches!(octets[0], 0xfc | 0xfd);
        prop_assert_eq!(is_unique_local_v6(&v6), expected);
    }

    #[test]
    fn prop_drop_link_local_keeps_exactly_the_non_link_local_addresses(
        raw in proptest::collection::vec(any::<[u8; 16]>(), 1..8),
        v4s in proptest::collection::vec(any::<[u8; 4]>(), 0..4),
    ) {
        let mut addrs: Vec<SocketAddr> = raw
            .into_iter()
            .map(|o| SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::from(o)), 9092))
            .collect();
        addrs.extend(v4s.into_iter().map(|o| SocketAddr::new(IpAddr::from(o), 9092)));
        let expected: Vec<SocketAddr> =
            addrs.iter().copied().filter(|a| !is_link_local(&a.ip())).collect();
        let result = drop_link_local_for_domain(
            addrs,
            SsrfPolicy::AllowPrivateNetwork,
            "http://x.test/",
        );
        match result {
            Ok(kept) => prop_assert_eq!(kept, expected),
            Err(_) => prop_assert!(expected.is_empty()),
        }
    }

    #[test]
    fn prop_ula_v6_always_blocked_under_public_only(first in prop_oneof![Just(0xfcu8), Just(0xfdu8)], rest in any::<[u8; 15]>()) {
        // Every fc00::/7 address is rejected under PublicOnly, regardless of
        // the lower bits. Treated like RFC 1918 private v4 under
        // AllowPrivateNetwork, so not asserted here (see
        // validate_url_allow_private_network_accepts_loopback_and_private).
        let mut octets = [0u8; 16];
        octets[0] = first;
        octets[1..].copy_from_slice(&rest);
        let v6 = std::net::Ipv6Addr::from(octets);
        prop_assert!(blocked_reason(&IpAddr::V6(v6), SsrfPolicy::PublicOnly).is_some());
    }

    #[test]
    fn prop_ipv4_mapped_v6_judged_as_v4(v4 in any::<[u8; 4]>(), policy in any_policy()) {
        // ::ffff:a.b.c.d must yield the same verdict as a.b.c.d — a blocked v4
        // range cannot be smuggled through the v6 namespace — under either
        // policy.
        let v4_addr = std::net::Ipv4Addr::from(v4);
        let mapped = v4_addr.to_ipv6_mapped();
        prop_assert_eq!(
            blocked_reason(&IpAddr::V6(mapped), policy).is_some(),
            blocked_reason(&IpAddr::V4(v4_addr), policy).is_some()
        );
    }

    #[test]
    fn prop_only_400_and_404_are_stale_session_status(code in 100u16..1000) {
        // #1094: exactly 400/404 trigger the clear-and-retry path; every
        // other status (2xx included, though callers only reach this on
        // a non-2xx) must not.
        let Ok(status) = reqwest::StatusCode::from_u16(code) else {
            return Ok(());
        };
        let expected = matches!(code, 400 | 404);
        prop_assert_eq!(is_stale_session_status(status), expected);
    }
}
