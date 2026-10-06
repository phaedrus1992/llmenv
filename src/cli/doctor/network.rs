//! Doctor check: can this machine read the network facts that `network` scopes match on (#1051).
//!
//! A scope never fires when its probe fails, for example when macOS hides the SSID. Doctor
//! says so, because a silent no-match is the failure that #1051 reports.

use std::net::IpAddr;

use crate::config::Config;
use crate::scope::ssid::{SsidReading, detect_ssid};

use super::CheckLevel;

/// Doctor's verdict on one SSID reading, for a config that has `ssid` scopes.
fn ssid_check(reading: &SsidReading) -> (CheckLevel, String) {
    match reading {
        SsidReading::Ssid(name) => (CheckLevel::Pass, format!("Wi-Fi SSID is {name:?}")),
        SsidReading::NotAssociated => (
            CheckLevel::Info,
            "no Wi-Fi network is associated, so `match.ssid` scopes do not match now".into(),
        ),
        SsidReading::Undetermined(reason) => (
            CheckLevel::Warn,
            format!(
                "cannot read the Wi-Fi SSID: {reason}. Every `match.ssid` scope stays inactive \
                 on this machine. Match on `gateway_mac` or `cidr` instead"
            ),
        ),
    }
}

/// Doctor's verdict on the gateway MAC probe, for a config that has `gateway_mac` scopes.
fn gateway_check(mac: Option<&str>) -> (CheckLevel, String) {
    match mac {
        Some(mac) => (CheckLevel::Pass, format!("gateway MAC is {mac}")),
        None => (
            CheckLevel::Warn,
            "cannot read the gateway MAC (no default route, or `arp`/`ip neigh` failed). Every \
             `match.gateway_mac` scope stays inactive until it can"
                .into(),
        ),
    }
}

/// Doctor's verdict on the interface address probe, for a config that has `cidr` scopes.
fn cidr_check(addrs: &[IpAddr]) -> (CheckLevel, String) {
    if addrs.is_empty() {
        (
            CheckLevel::Warn,
            "found no local interface address (loopback and link-local do not count). Every \
             `match.cidr` scope stays inactive until the machine has one"
                .into(),
        )
    } else {
        let list: Vec<String> = addrs.iter().map(IpAddr::to_string).collect();
        (
            CheckLevel::Pass,
            format!("local addresses for `match.cidr`: {}", list.join(", ")),
        )
    }
}

fn uses(config: &Config, field: impl Fn(&crate::config::NetworkMatch) -> bool) -> bool {
    config.scope.network.iter().any(|s| field(&s.r#match))
}

/// Report what this machine can read for each field that a network scope matches on. Prints
/// nothing for a field that no scope uses, so a config pays only for the probes it needs.
pub(super) fn run_doctor_network(use_color: bool, config: &Config) {
    let gateway = uses(config, |m| m.gateway_mac.is_some());
    let cidr = uses(config, |m| m.cidr.is_some());
    let ssid = uses(config, |m| m.ssid.is_some());
    if !(gateway || cidr || ssid) {
        return;
    }
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let info = super::super::doctor_info(use_color);
    eprintln!();
    eprintln!("Network scopes:");
    if gateway {
        let mac = crate::scope::network::detect_gateway_mac();
        super::print_check(gateway_check(mac.as_deref()), &pass, &warn, &info);
    }
    if cidr {
        let addrs = crate::scope::network::detect_local_addrs();
        super::print_check(cidr_check(&addrs), &pass, &warn, &info);
    }
    if ssid {
        super::print_check(ssid_check(&detect_ssid()), &pass, &warn, &info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_readable_ssid_passes() {
        let (level, text) = ssid_check(&SsidReading::Ssid("Home".into()));
        assert!(matches!(level, CheckLevel::Pass));
        assert!(text.contains("Home"), "{text}");
    }

    #[test]
    fn no_association_is_informational() {
        let (level, _) = ssid_check(&SsidReading::NotAssociated);
        assert!(matches!(level, CheckLevel::Info));
    }

    #[test]
    fn an_unreadable_ssid_warns_with_the_reason_and_the_fix() {
        let (level, text) = ssid_check(&SsidReading::Undetermined("macOS says <redacted>".into()));
        assert!(matches!(level, CheckLevel::Warn));
        assert!(text.contains("<redacted>"), "{text}");
        assert!(
            text.contains("gateway_mac") && text.contains("cidr"),
            "{text}"
        );
    }

    #[test]
    fn an_unreadable_gateway_mac_warns() {
        let (level, text) = gateway_check(None);
        assert!(matches!(level, CheckLevel::Warn));
        assert!(text.contains("gateway_mac"), "{text}");
        let (level, text) = gateway_check(Some("aa:bb:cc:dd:ee:ff"));
        assert!(matches!(level, CheckLevel::Pass));
        assert!(text.contains("aa:bb:cc:dd:ee:ff"), "{text}");
    }

    #[test]
    fn no_local_address_warns_and_addresses_are_listed() {
        let (level, text) = cidr_check(&[]);
        assert!(matches!(level, CheckLevel::Warn));
        assert!(text.contains("match.cidr"), "{text}");
        let addrs: Vec<IpAddr> = vec!["192.168.1.7".parse().unwrap(), "fd00::1".parse().unwrap()];
        let (level, text) = cidr_check(&addrs);
        assert!(matches!(level, CheckLevel::Pass));
        assert!(
            text.contains("192.168.1.7") && text.contains("fd00::1"),
            "{text}"
        );
    }
}
