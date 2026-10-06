//! Network facts for `network` scopes: gateway MAC, interface addresses, and matching.
//!
//! Detection shells out to platform-specific commands, but all parsing is
//! pure-function so it can be unit-tested with canned output. SSID detection lives in
//! [`super::ssid`].

use std::net::IpAddr;

use crate::config::NetworkScope;

use super::matcher::Env;

/// The network probes that a config needs. Each probe costs a subprocess or a syscall,
/// so detection runs only the ones that a declared scope can use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct NetworkNeeds {
    pub(crate) gateway_mac: bool,
    pub(crate) local_addrs: bool,
    pub(crate) ssid: bool,
}

impl NetworkNeeds {
    pub(crate) const ALL: Self = Self {
        gateway_mac: true,
        local_addrs: true,
        ssid: true,
    };

    pub(crate) fn from_scopes(scopes: &[NetworkScope]) -> Self {
        scopes.iter().fold(Self::default(), |acc, s| Self {
            gateway_mac: acc.gateway_mac || s.r#match.gateway_mac.is_some(),
            local_addrs: acc.local_addrs || s.r#match.cidr.is_some(),
            ssid: acc.ssid || s.r#match.ssid.is_some(),
        })
    }

    /// Whether a result detected with `self` also answers a request for `other`.
    pub(crate) fn covers(self, other: Self) -> bool {
        (self.gateway_mac || !other.gateway_mac)
            && (self.local_addrs || !other.local_addrs)
            && (self.ssid || !other.ssid)
    }
}

/// Whether the network scope matches. Every field that the scope sets must match, and a
/// scope that sets no field never matches.
#[must_use]
pub(crate) fn matches_network(s: &NetworkScope, env: &Env) -> bool {
    let m = &s.r#match;
    if m.gateway_mac.is_none() && m.ssid.is_none() && m.cidr.is_none() {
        return false;
    }
    m.gateway_mac
        .as_deref()
        .is_none_or(|want| gateway_mac_matches(want, env))
        && m.ssid
            .as_deref()
            // SSIDs are case-sensitive, unlike hostnames and MACs.
            .is_none_or(|want| env.ssid.as_deref() == Some(want))
        && m.cidr
            .as_deref()
            .is_none_or(|want| cidr_matches(want, &env.local_addrs))
}

fn gateway_mac_matches(want: &str, env: &Env) -> bool {
    let Some(got) = env.gateway_mac.as_deref() else {
        return false;
    };
    // Normalize both sides so either spelling in config matches (#2487). A value that is not
    // a MAC falls back to a case-insensitive compare.
    match (normalize_mac(want), normalize_mac(got)) {
        (Some(w), Some(g)) => w == g,
        _ => got.eq_ignore_ascii_case(want),
    }
}

/// Whether any of `addrs` lies inside the CIDR block `cidr`. A block that does not parse
/// matches nothing; config validation reports it at load time.
#[must_use]
fn cidr_matches(cidr: &str, addrs: &[IpAddr]) -> bool {
    cidr.parse::<ipnet::IpNet>()
        .is_ok_and(|net| addrs.iter().any(|a| net.contains(a)))
}

/// Whether `ip` says something about which network the machine is on. Loopback and
/// link-local addresses exist on every machine, so a `cidr` scope never matches them.
fn identifies_a_network(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !(v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()),
        IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified() || v6.is_unicast_link_local()),
    }
}

/// Addresses of the local network interfaces, without loopback and link-local addresses.
/// An enumeration failure gives an empty list, so a `cidr` scope does not match.
#[must_use]
pub(crate) fn detect_local_addrs() -> Vec<IpAddr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .iter()
            .map(if_addrs::Interface::ip)
            .filter(identifies_a_network)
            .collect(),
        Err(e) => {
            tracing::debug!("interface enumeration failed: {e}");
            Vec::new()
        }
    }
}

#[must_use]
pub(crate) fn detect_gateway_mac() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        detect_macos()
    }
    #[cfg(target_os = "linux")]
    {
        detect_linux()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn detect_macos() -> Option<String> {
    let route = run(&["route", "-n", "get", "default"])?;
    let gw_ip = parse_macos_gateway_ip(&route)?;
    let arp = run(&["arp", "-n", &gw_ip])?;
    parse_macos_arp_mac(&arp)
}

#[cfg(target_os = "linux")]
fn detect_linux() -> Option<String> {
    let route = run(&["ip", "route", "show", "default"])?;
    let gw_ip = parse_linux_gateway_ip(&route)?;
    let neigh = run(&["ip", "neigh", "show", &gw_ip])?;
    parse_linux_neigh_mac(&neigh)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run(args: &[&str]) -> Option<String> {
    let (cmd, rest) = args.split_first()?;
    super::capture_stdout("gateway-mac detection", cmd, rest)
}

#[must_use]
pub fn parse_macos_gateway_ip(s: &str) -> Option<String> {
    s.lines().find_map(|l| {
        l.trim()
            .strip_prefix("gateway:")
            .map(str::trim)
            .map(String::from)
    })
}

#[must_use]
pub fn parse_macos_arp_mac(s: &str) -> Option<String> {
    // Format: `? (192.168.1.1) at aa:bb:cc:dd:ee:ff on en0 ifscope [ethernet]`
    // (macOS may print an octet without its leading zero).
    s.split_whitespace().find_map(normalize_mac)
}

#[must_use]
pub fn parse_linux_gateway_ip(s: &str) -> Option<String> {
    // Format: `default via 10.0.0.1 dev eth0 ...`
    for line in s.lines() {
        let mut it = line.split_whitespace();
        if it.next() == Some("default") && it.next() == Some("via") {
            return it.next().map(String::from);
        }
    }
    None
}

#[must_use]
pub fn parse_linux_neigh_mac(s: &str) -> Option<String> {
    // Format: `10.0.0.1 dev eth0 lladdr 11:22:33:44:55:66 REACHABLE`
    let mut tokens = s.split_whitespace();
    while let Some(t) = tokens.next() {
        if t == "lladdr" {
            return tokens.next().and_then(normalize_mac);
        }
    }
    None
}

/// Normalize a MAC to canonical lowercase `xx:xx:xx:xx:xx:xx`.
///
/// macOS `arp -n` drops the leading zero of each octet (`1c:b:8b:e4:5f:94`),
/// so each of the six octets may carry one or two hex digits (#2487).
/// Returns `None` when the text is not a MAC.
#[must_use]
fn normalize_mac(s: &str) -> Option<String> {
    let mut octets = Vec::with_capacity(6);
    for part in s.split(':') {
        if !(1..=2).contains(&part.len()) || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        octets.push(format!("{:0>2}", part.to_ascii_lowercase()));
    }
    (octets.len() == 6).then(|| octets.join(":"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses_exclude_loopback_and_link_local() {
        assert!(detect_local_addrs().iter().all(identifies_a_network));
    }

    #[test]
    fn only_an_address_that_names_a_network_identifies_one() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        for no in [
            "127.0.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "::1",
            "::",
            "fe80::1",
        ] {
            assert!(!identifies_a_network(&ip(no)), "{no}");
        }
        for yes in [
            "192.168.1.7",
            "10.0.0.1",
            "100.64.0.9",
            "fd00::1",
            "2001:db8::1",
        ] {
            assert!(identifies_a_network(&ip(yes)), "{yes}");
        }
    }

    proptest::proptest! {
        #[test]
        fn normalize_mac_pads_any_octet_spelling_to_the_canonical_form(
            octets in proptest::prelude::any::<[u8; 6]>(),
            drop_zero in proptest::prelude::any::<[bool; 6]>(),
            upper in proptest::prelude::any::<bool>(),
        ) {
            let spelled: Vec<String> = octets
                .iter()
                .zip(drop_zero)
                .map(|(o, drop)| {
                    let s = if drop { format!("{o:x}") } else { format!("{o:02x}") };
                    if upper { s.to_ascii_uppercase() } else { s }
                })
                .collect();
            let canonical = octets.iter().map(|o| format!("{o:02x}")).collect::<Vec<_>>().join(":");
            let got = normalize_mac(&spelled.join(":"));
            proptest::prop_assert_eq!(got.as_deref(), Some(canonical.as_str()));
            // Normalizing a canonical MAC changes nothing.
            proptest::prop_assert_eq!(normalize_mac(&canonical), Some(canonical));
        }

        #[test]
        fn normalize_mac_and_the_arp_parsers_never_panic(s in "\\PC{0,120}") {
            let _ = normalize_mac(&s);
            let _ = parse_macos_arp_mac(&s);
            let _ = parse_linux_neigh_mac(&s);
        }
    }

    #[test]
    fn needs_cover_only_the_probes_they_include() {
        let mac = NetworkNeeds {
            gateway_mac: true,
            ..NetworkNeeds::default()
        };
        assert!(NetworkNeeds::ALL.covers(mac));
        assert!(mac.covers(mac));
        assert!(!mac.covers(NetworkNeeds::ALL));
        assert!(NetworkNeeds::default().covers(NetworkNeeds::default()));
        assert!(!NetworkNeeds::default().covers(mac));
    }
}
