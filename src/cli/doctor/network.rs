//! Doctor check: can this machine read the Wi-Fi SSID that `match.ssid` scopes need (#1051).
//!
//! A scope with `match.ssid` never fires when the platform hides the SSID. Doctor says so,
//! because a silent no-match is the failure that #1051 reports.

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

fn has_ssid_scope(config: &Config) -> bool {
    config
        .scope
        .network
        .iter()
        .any(|s| s.r#match.ssid.is_some())
}

/// Report the SSID reading when a network scope matches on `ssid`. Prints nothing otherwise,
/// so a config with no `ssid` scope pays no subprocess.
pub(super) fn run_doctor_network(use_color: bool, config: &Config) {
    if !has_ssid_scope(config) {
        return;
    }
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let info = super::super::doctor_info(use_color);
    eprintln!();
    eprintln!("Network scopes:");
    super::print_check(ssid_check(&detect_ssid()), &pass, &warn, &info);
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
}
