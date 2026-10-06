//! Gateway-MAC detection.
//!
//! Detection shells out to platform-specific commands, but all parsing is
//! pure-function so it can be unit-tested with canned output.

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
pub(crate) fn normalize_mac(s: &str) -> Option<String> {
    let mut octets = Vec::with_capacity(6);
    for part in s.split(':') {
        if !(1..=2).contains(&part.len()) || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        octets.push(format!("{:0>2}", part.to_ascii_lowercase()));
    }
    (octets.len() == 6).then(|| octets.join(":"))
}
