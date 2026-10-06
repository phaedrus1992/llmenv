use llmenv_scope::network::{
    parse_linux_gateway_ip, parse_linux_neigh_mac, parse_macos_arp_mac, parse_macos_gateway_ip,
};

const MACOS_ROUTE: &str = "   route to: default
destination: default
       mask: default
    gateway: 192.168.1.1
  interface: en0
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING>
";

const MACOS_ARP: &str = "? (192.168.1.1) at aa:bb:cc:dd:ee:ff on en0 ifscope [ethernet]
";

const LINUX_ROUTE: &str = "default via 10.0.0.1 dev eth0 proto dhcp metric 100
";

const LINUX_NEIGH: &str = "10.0.0.1 dev eth0 lladdr 11:22:33:44:55:66 REACHABLE
";

#[test]
fn parses_macos_default_gateway() {
    assert_eq!(
        parse_macos_gateway_ip(MACOS_ROUTE).as_deref(),
        Some("192.168.1.1")
    );
}

#[test]
fn parses_macos_arp_mac() {
    assert_eq!(
        parse_macos_arp_mac(MACOS_ARP).as_deref(),
        Some("aa:bb:cc:dd:ee:ff")
    );
}

#[test]
fn parses_linux_default_gateway() {
    assert_eq!(
        parse_linux_gateway_ip(LINUX_ROUTE).as_deref(),
        Some("10.0.0.1")
    );
}

#[test]
fn parses_linux_neigh_mac() {
    assert_eq!(
        parse_linux_neigh_mac(LINUX_NEIGH).as_deref(),
        Some("11:22:33:44:55:66")
    );
}

#[test]
fn macos_gateway_handles_missing() {
    assert_eq!(parse_macos_gateway_ip("no gateway here\n"), None);
}

#[test]
fn linux_gateway_handles_malformed() {
    assert_eq!(parse_linux_gateway_ip("garbage line\n"), None);
}

#[test]
fn arp_handles_incomplete_entry() {
    assert_eq!(
        parse_macos_arp_mac("? (192.168.1.1) at (incomplete) on en0\n"),
        None
    );
}

#[test]
fn macos_arp_zero_pads_dropped_leading_zeros() {
    // macOS `arp -n` prints `1c:b:8b:e4:5f:94` for 1c:0b:8b:e4:5f:94 (#2487).
    assert_eq!(
        parse_macos_arp_mac("? (10.0.0.1) at 1c:b:8b:e4:5f:94 on en0 ifscope [ethernet]\n")
            .as_deref(),
        Some("1c:0b:8b:e4:5f:94")
    );
}

#[test]
fn macos_arp_lowercases_the_mac() {
    assert_eq!(
        parse_macos_arp_mac("? (10.0.0.1) at AA:B:CC:D:EE:F on en0\n").as_deref(),
        Some("aa:0b:cc:0d:ee:0f")
    );
}

#[test]
fn arp_rejects_a_mac_with_too_few_octets() {
    assert_eq!(
        parse_macos_arp_mac("? (10.0.0.1) at 1c:b:8b:e4:5f on en0\n"),
        None
    );
}

#[test]
fn arp_rejects_a_three_digit_octet() {
    assert_eq!(parse_macos_arp_mac("at 1c:0bb:8b:e4:5f:94 on en0\n"), None);
}
