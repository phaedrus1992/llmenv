//! Wi-Fi SSID detection for `match.ssid` network scopes (#1051).
//!
//! Detection shells out to a platform tool. All parsing is pure so tests feed it canned
//! output, which needs no Wi-Fi association.

/// What the platform reports about the current Wi-Fi association.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsidReading {
    /// Associated to a network with this name.
    Ssid(String),
    /// The platform answered, and no Wi-Fi network is associated.
    NotAssociated,
    /// The platform cannot say. The text names the reason, for `llmenv doctor`.
    Undetermined(String),
}

/// Read the SSID that this machine is associated with.
#[must_use]
pub(crate) fn detect_ssid() -> SsidReading {
    #[cfg(target_os = "macos")]
    {
        detect_macos()
    }
    #[cfg(target_os = "linux")]
    {
        detect_linux()
    }
    #[cfg(windows)]
    {
        detect_windows()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        SsidReading::Undetermined("this platform has no SSID reader".into())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn run(program: &str, args: &[&str]) -> Option<String> {
    super::capture_stdout("ssid detection", program, args)
}

#[cfg(target_os = "macos")]
fn detect_macos() -> SsidReading {
    let Some(ports) = run("networksetup", &["-listallhardwareports"]) else {
        return SsidReading::Undetermined("`networksetup -listallhardwareports` failed".into());
    };
    // The default route can leave over Ethernet while Wi-Fi is also associated, so ask each
    // Wi-Fi device instead of the default-route interface.
    for device in parse_macos_wifi_devices(&ports) {
        match run("ipconfig", &["getsummary", &device]) {
            Some(summary) => match parse_macos_ipconfig_ssid(&summary) {
                SsidReading::NotAssociated => {}
                reading => return reading,
            },
            None => {
                return SsidReading::Undetermined(format!("`ipconfig getsummary {device}` failed"));
            }
        }
    }
    SsidReading::NotAssociated
}

#[cfg(target_os = "linux")]
fn detect_linux() -> SsidReading {
    if let Some(reading) = detect_via_nmcli() {
        return reading;
    }
    match run("iw", &["dev"]) {
        Some(out) => parse_iw_ssid(&out).map_or(SsidReading::NotAssociated, SsidReading::Ssid),
        None => {
            SsidReading::Undetermined("neither `nmcli` nor `iw` could report Wi-Fi state".into())
        }
    }
}

/// Read the SSID of the active Wi-Fi connection profile. The scan list
/// (`nmcli dev wifi`) carries SSIDs that a nearby attacker chooses, and an SSID can hold a
/// newline, so that list can forge a line. The active profile comes from saved settings.
#[cfg(target_os = "linux")]
fn detect_via_nmcli() -> Option<SsidReading> {
    let active = run(
        "nmcli",
        &["-t", "-f", "TYPE,NAME", "connection", "show", "--active"],
    )?;
    let Some(name) = parse_nmcli_active_wifi_connection(&active) else {
        return Some(SsidReading::NotAssociated);
    };
    let value = run(
        "nmcli",
        &[
            "-g",
            "802-11-wireless.ssid",
            "connection",
            "show",
            "id",
            &name,
        ],
    )?;
    Some(parse_nmcli_ssid_value(&value).map_or(SsidReading::NotAssociated, SsidReading::Ssid))
}

#[cfg(windows)]
fn detect_windows() -> SsidReading {
    match run("netsh", &["wlan", "show", "interfaces"]) {
        Some(out) => parse_netsh_ssid(&out).map_or(SsidReading::NotAssociated, SsidReading::Ssid),
        None => SsidReading::Undetermined("`netsh wlan show interfaces` failed".into()),
    }
}

/// The device names of the Wi-Fi ports in `networksetup -listallhardwareports`, whose
/// entries read `Hardware Port: Wi-Fi` then `Device: en0`.
#[must_use]
pub fn parse_macos_wifi_devices(s: &str) -> Vec<String> {
    let mut devices = Vec::new();
    let mut in_wifi = false;
    for line in s.lines() {
        if let Some(port) = line.trim().strip_prefix("Hardware Port:") {
            in_wifi = port.trim() == "Wi-Fi";
        } else if in_wifi && let Some(dev) = line.trim().strip_prefix("Device:") {
            devices.push(dev.trim().to_string());
            in_wifi = false;
        }
    }
    devices
}

/// The SSID from `ipconfig getsummary <iface>`, a line of the form `SSID : MyNet`.
///
/// macOS 15 prints `SSID : <redacted>` to a process that has no Location Services grant,
/// which is "cannot determine", not a network named `<redacted>`.
#[must_use]
pub fn parse_macos_ipconfig_ssid(s: &str) -> SsidReading {
    // An SSID can start or end with a space, so only the ` : ` separator is removed.
    let Some(value) = s.lines().find_map(|l| {
        let (key, value) = l.split_once(" : ")?;
        (key.trim() == "SSID").then_some(value)
    }) else {
        return SsidReading::NotAssociated;
    };
    match value {
        "" => SsidReading::NotAssociated,
        "<redacted>" => SsidReading::Undetermined(
            "macOS hides the SSID from command-line tools (it prints <redacted>) unless the \
             process has a Location Services grant"
                .into(),
        ),
        name => SsidReading::Ssid(name.to_string()),
    }
}

/// The name of the active Wi-Fi connection from `nmcli -t -f TYPE,NAME connection show
/// --active`, lines of the form `802-11-wireless:Home`.
///
/// Terse mode escapes `:` and `\` in a value with a backslash.
#[must_use]
pub fn parse_nmcli_active_wifi_connection(s: &str) -> Option<String> {
    s.lines().find_map(|l| {
        let (kind, name) = l.split_once(':')?;
        let is_wifi = matches!(kind, "802-11-wireless" | "wifi");
        let name = unescape_nmcli(name);
        (is_wifi && !name.is_empty()).then_some(name)
    })
}

/// The SSID from `nmcli -g 802-11-wireless.ssid connection show id <name>`: one line.
#[must_use]
pub fn parse_nmcli_ssid_value(s: &str) -> Option<String> {
    let name = unescape_nmcli(s.lines().next()?);
    (!name.is_empty()).then_some(name)
}

fn unescape_nmcli(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

/// The SSID from `iw dev`, an indented `ssid MyNet` line under an associated interface.
#[must_use]
pub fn parse_iw_ssid(s: &str) -> Option<String> {
    s.lines().find_map(|l| {
        let name = l.trim_start().strip_prefix("ssid ")?;
        (!name.is_empty()).then(|| name.to_string())
    })
}

/// The SSID from `netsh wlan show interfaces`, a line of the form `SSID : MyNet`.
///
/// The `BSSID` line shares the suffix, so the key must equal `SSID` exactly.
#[must_use]
pub fn parse_netsh_ssid(s: &str) -> Option<String> {
    s.lines().find_map(|l| {
        let (key, value) = l.split_once(" : ")?;
        (key.trim() == "SSID" && !value.is_empty()).then(|| value.to_string())
    })
}
