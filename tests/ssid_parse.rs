use llmenv::scope::ssid::{
    SsidReading, parse_iw_ssid, parse_macos_ipconfig_ssid, parse_macos_wifi_devices,
    parse_netsh_ssid, parse_nmcli_ssid,
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
fn macos_without_an_ssid_line_is_not_associated() {
    assert_eq!(
        parse_macos_ipconfig_ssid("<dictionary> {\n  IPv4 : x\n}\n"),
        SsidReading::NotAssociated
    );
}

#[test]
fn nmcli_reads_the_active_ssid_and_unescapes_colons() {
    assert_eq!(
        parse_nmcli_ssid("no:Other\nyes:Home\n").as_deref(),
        Some("Home")
    );
    assert_eq!(
        parse_nmcli_ssid("yes:Cafe\\: 5G\n").as_deref(),
        Some("Cafe: 5G")
    );
    assert_eq!(parse_nmcli_ssid("no:Other\n"), None);
    assert_eq!(parse_nmcli_ssid("yes:\n"), None);
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
