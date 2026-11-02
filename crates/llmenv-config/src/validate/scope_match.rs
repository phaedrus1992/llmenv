//! Validators for the network and host scope matches in `config.yaml`.

/// Whether `cidr` is an IPv4 or IPv6 network in `address/prefix` form. Uses the `ipnet`
/// parser that the scope matcher uses, so validation and matching cannot disagree (#1051).
pub(super) fn is_valid_cidr(cidr: &str) -> bool {
    let Some((addr, _prefix)) = cidr.split_once('/') else {
        return false;
    };
    // `ipnet` accepts a leading zero in a dotted-quad octet. RFC 4632 forbids it, and it
    // invites octal confusion, so reject it here.
    let ipv4_leading_zero = !addr.contains(':')
        && addr
            .split('.')
            .any(|octet| octet.len() > 1 && octet.starts_with('0'));
    !ipv4_leading_zero && cidr.parse::<ipnet::IpNet>().is_ok()
}

pub(super) fn is_valid_mac_address(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return false;
    }
    parts
        .iter()
        // macOS `arp` drops the leading zero of an octet (`1c:b:8b:e4:5f:94`), and the
        // matcher accepts that form, so the validator must too (#2487).
        .all(|part| (1..=2).contains(&part.len()) && u8::from_str_radix(part, 16).is_ok())
}

pub(super) fn is_valid_hostname(hostname: &str) -> bool {
    // RFC 1123 §2.1 / RFC 952: total length <= 253 octets, each label
    // 1..=63 octets, labels are alphanumeric plus interior hyphens.
    if hostname.is_empty() || hostname.len() > 253 {
        return false;
    }
    hostname.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}
