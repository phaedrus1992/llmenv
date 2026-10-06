use llmenv_scope::ssid::{
    SsidReading, parse_iw_ssid, parse_macos_ipconfig_ssid, parse_macos_wifi_devices,
    parse_netsh_ssid, parse_nmcli_active_wifi_connection, parse_nmcli_ssid_value,
};

const MACOS_PORTS: &str = "Hardware Port: Ethernet Adapter (en14)
Device: en14
Ethernet Address: aa:bb:cc:00:00:01

Hardware Port: Wi-Fi
Device: en0
Ethernet Address: aa:bb:cc:00:00:02

Hardware Port: Thunderbolt Bridge
Device: bridge0
";

proptest::proptest! {
    #[test]
    fn no_ssid_parser_panics_on_any_text(s in "\\PC{0,300}") {
        let _ = parse_iw_ssid(&s);
        let _ = parse_netsh_ssid(&s);
        let _ = parse_nmcli_active_wifi_connection(&s);
        let _ = parse_nmcli_ssid_value(&s);
        let _ = parse_macos_ipconfig_ssid(&s);
        let _ = parse_macos_wifi_devices(&s);
    }

    #[test]
    fn an_ssid_that_the_parsers_return_is_never_empty(s in "\\PC{0,300}") {
        for got in [
            parse_iw_ssid(&s),
            parse_netsh_ssid(&s),
            parse_nmcli_active_wifi_connection(&s),
            parse_nmcli_ssid_value(&s),
        ] {
            proptest::prop_assert!(got.is_none_or(|name| !name.is_empty()));
        }
    }

    #[test]
    fn iw_returns_the_name_after_the_ssid_keyword(name in "[A-Za-z0-9 _-]{1,32}") {
        let out = format!("phy#0\n\tInterface wlan0\n\t\tssid {name}\n");
        // Spaces inside an SSID are part of the name, so nothing is trimmed.
        proptest::prop_assert_eq!(parse_iw_ssid(&out), Some(name));
    }
}

#[test]
fn macos_wifi_devices_are_read_by_port_name() {
    assert_eq!(
        parse_macos_wifi_devices(MACOS_PORTS),
        vec!["en0".to_string()]
    );
    assert!(parse_macos_wifi_devices("Hardware Port: Ethernet\nDevice: en5\n").is_empty());
    assert!(parse_macos_wifi_devices("").is_empty());
}

#[test]
fn macos_ipconfig_reads_the_ssid() {
    let out =
        "<dictionary> {\n  BSSID : aa:bb:cc:dd:ee:ff\n  SSID : Home Wifi\n  Security : WPA2\n}\n";
    assert_eq!(
        parse_macos_ipconfig_ssid(out),
        SsidReading::Ssid("Home Wifi".into())
    );
}

#[test]
fn macos_redacted_ssid_is_undetermined_not_a_network_name() {
    // macOS 15 without a Location Services grant (#1051).
    let reading = parse_macos_ipconfig_ssid("  SSID : <redacted>\n");
    assert!(
        matches!(reading, SsidReading::Undetermined(_)),
        "{reading:?}"
    );
}

#[test]
fn macos_ssid_keeps_leading_and_trailing_spaces() {
    assert_eq!(
        parse_macos_ipconfig_ssid("  SSID :  Home \n"),
        SsidReading::Ssid(" Home ".into())
    );
    assert_eq!(
        parse_netsh_ssid("    SSID : Home \r\n").as_deref(),
        Some("Home ")
    );
}

#[test]
fn macos_without_an_ssid_line_is_not_associated() {
    assert_eq!(
        parse_macos_ipconfig_ssid("<dictionary> {\n  IPv4 : x\n}\n"),
        SsidReading::NotAssociated
    );
}

#[test]
fn nmcli_reads_the_active_wifi_connection_name() {
    let out = "ethernet:Wired connection 1\n802-11-wireless:Home\\: 5G\nvpn:Work\n";
    assert_eq!(
        parse_nmcli_active_wifi_connection(out).as_deref(),
        Some("Home: 5G")
    );
    assert_eq!(
        parse_nmcli_active_wifi_connection("wifi:Cafe\n").as_deref(),
        Some("Cafe")
    );
    assert_eq!(parse_nmcli_active_wifi_connection("ethernet:Wired\n"), None);
    assert_eq!(
        parse_nmcli_active_wifi_connection("802-11-wireless:\n"),
        None
    );
    assert_eq!(parse_nmcli_active_wifi_connection(""), None);
}

#[test]
fn nmcli_ssid_value_is_one_unescaped_line() {
    assert_eq!(
        parse_nmcli_ssid_value("Home Wifi\n").as_deref(),
        Some("Home Wifi")
    );
    assert_eq!(
        parse_nmcli_ssid_value("Cafe\\: 5G\n").as_deref(),
        Some("Cafe: 5G")
    );
    // Only the first line counts, so trailing output cannot add a name.
    assert_eq!(
        parse_nmcli_ssid_value("Home\nOther\n").as_deref(),
        Some("Home")
    );
    assert_eq!(parse_nmcli_ssid_value("\n"), None);
    assert_eq!(parse_nmcli_ssid_value(""), None);
}

#[test]
fn iw_reads_the_ssid_of_an_associated_interface() {
    let out = "phy#0\n\tInterface wlan0\n\t\tifindex 3\n\t\ttype managed\n\t\tssid Home Wifi\n";
    assert_eq!(parse_iw_ssid(out).as_deref(), Some("Home Wifi"));
    assert_eq!(
        parse_iw_ssid("phy#0\n\tInterface wlan0\n\t\ttype managed\n"),
        None
    );
}

#[test]
fn netsh_reads_ssid_and_ignores_bssid() {
    let out = "    Name                   : Wi-Fi\n    BSSID                  : aa:bb:cc:dd:ee:ff\n    SSID                   : Home Wifi\n    Signal                 : 90%\n";
    assert_eq!(parse_netsh_ssid(out).as_deref(), Some("Home Wifi"));
    assert_eq!(parse_netsh_ssid("    BSSID : aa:bb:cc:dd:ee:ff\n"), None);
    assert_eq!(parse_netsh_ssid("    State : disconnected\n"), None);
}
