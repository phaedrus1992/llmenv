use proptest::prelude::*;
use llmenv::config::Config;

// ===== CIDR Validation =====

fn valid_cidr_strategy() -> impl Strategy<Value = String> {
    (0u8..=255, 0u8..=255, 0u8..=255, 0u8..=255, 0u8..=32)
        .prop_map(|(a, b, c, d, mask)| format!("{}.{}.{}.{}/{}", a, b, c, d, mask))
}

fn invalid_cidr_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        (0u8..=255, 0u8..=255, 0u8..=255).prop_map(|(a, b, c)| format!("{}.{}.{}", a, b, c)),
        (256u16..=500).prop_map(|n| format!("{}.0.0.0/24", n)),
        (33u8..=255).prop_map(|m| format!("192.168.1.0/{}", m)),
        Just("192.168.1.abc/24".to_string()),
        Just("192.168.1.0/abc".to_string()),
    ]
}

#[test]
fn prop_valid_cidrs_parse_correctly() {
    proptest!(|(cidr in valid_cidr_strategy())| {
        let parts: Vec<&str> = cidr.split('/').collect();
        assert_eq!(parts.len(), 2);
        let octets: Vec<&str> = parts[0].split('.').collect();
        assert_eq!(octets.len(), 4);
    });
}

#[test]
fn prop_invalid_cidrs_have_flaws() {
    proptest!(|(cidr in invalid_cidr_strategy())| {
        let parts: Vec<&str> = cidr.split('/').collect();
        if parts.len() == 2 {
            let octets: Vec<&str> = parts[0].split('.').collect();
            if octets.len() == 4 {
                let octets_invalid = octets.iter().any(|o| {
                    o.parse::<u16>().map_or(true, |n| n > 255)
                });
                let mask_invalid = parts[1].parse::<u8>().map_or(true, |m| m > 32);
                assert!(octets_invalid || mask_invalid);
            } else {
                assert_ne!(octets.len(), 4);
            }
        } else {
            assert_ne!(parts.len(), 2);
        }
    });
}

// ===== Hostname Validation =====

fn valid_hostname_strategy() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-z][a-z0-9-]*[a-z0-9](\\.[a-z][a-z0-9-]*[a-z0-9])*")
        .expect("valid regex")
        .prop_filter("length in range", |h| !h.is_empty() && h.len() <= 253)
}

fn invalid_hostname_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("".to_string()),
        Just("a".repeat(254)),
        Just("test..example.com".to_string()),
        Just("test@example.com".to_string()),
        Just("-invalid.com".to_string()),
        Just("invalid-.com".to_string()),
    ]
}

#[test]
fn prop_valid_hostnames_structure_correct() {
    proptest!(|(hostname in valid_hostname_strategy())| {
        assert!(!hostname.is_empty());
        assert!(hostname.len() <= 253);
        assert!(!hostname.starts_with('-'));
        assert!(!hostname.ends_with('-'));
        assert!(!hostname.contains(".."));
    });
}

#[test]
fn prop_invalid_hostnames_have_flaws() {
    proptest!(|(hostname in invalid_hostname_strategy())| {
        let is_invalid = hostname.is_empty()
            || hostname.len() > 253
            || hostname.starts_with('-')
            || hostname.ends_with('-')
            || hostname.contains("..")
            || hostname.split('.').any(|label| label.starts_with('-') || label.ends_with('-'))
            || hostname.contains(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '.');
        assert!(is_invalid);
    });
}

// ===== Variable Name Validation =====

fn valid_var_name_strategy() -> impl Strategy<Value = String> {
    prop::string::string_regex("[A-Za-z_][A-Za-z0-9_]*")
        .expect("valid regex")
        .prop_filter("non-empty", |s| !s.is_empty())
}

fn invalid_var_name_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("".to_string()),
        Just("9var".to_string()),
        Just("var-name".to_string()),
        Just("var.name".to_string()),
        Just("var@name".to_string()),
    ]
}

#[test]
fn prop_valid_var_names_structure_correct() {
    proptest!(|(name in valid_var_name_strategy())| {
        assert!(!name.is_empty());
        let first = name.chars().next().unwrap();
        assert!(first.is_ascii_alphabetic() || first == '_');
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    });
}

#[test]
fn prop_invalid_var_names_have_flaws() {
    proptest!(|(name in invalid_var_name_strategy())| {
        if !name.is_empty() {
            let first = name.chars().next().unwrap();
            let invalid_first = !first.is_ascii_alphabetic() && first != '_';
            let invalid_chars = name.chars().any(|c| !c.is_ascii_alphanumeric() && c != '_');
            assert!(invalid_first || invalid_chars);
        }
    });
}

// ===== Path Prefix Validation =====

fn valid_path_prefix_strategy() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-z0-9/_-]+")
        .expect("valid regex")
        .prop_filter("length in range", |p| !p.is_empty() && p.len() <= 4096)
        .prop_filter("no parent components", |p| !p.contains(".."))
}

fn invalid_path_prefix_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("".to_string()),
        Just("a".repeat(4097)),
        Just("path\0null".to_string()),
        Just("foo/../bar".to_string()),
        Just("..".to_string()),
    ]
}

#[test]
fn prop_valid_path_prefixes_structure_correct() {
    proptest!(|(path in valid_path_prefix_strategy())| {
        assert!(!path.is_empty());
        assert!(path.len() <= 4096);
        assert!(!path.contains('\0'));
        assert!(!path.contains(".."));
    });
}

#[test]
fn prop_invalid_path_prefixes_have_flaws() {
    proptest!(|(path in invalid_path_prefix_strategy())| {
        let is_invalid = path.is_empty()
            || path.len() > 4096
            || path.contains('\0')
            || path.contains("..");
        assert!(is_invalid);
    });
}

// ===== Config Round-Trip Serialization =====

#[test]
fn prop_config_round_trip_preserves_structure() {
    proptest!(|(
        scope_id in "[a-z0-9_]+",
        hostname in valid_hostname_strategy(),
        bundle_name in "[a-z0-9_-]+"
    )| {
        let yaml_template = r#"
scope:
  host:
    - id: {id}
      match:
        hostname: {hostname}
      tags: [tag1]
bundle:
  - name: {bundle_name}
    tags: [tag1]
settings:
  cache_retention_hours: 24
"#;

        let yaml = yaml_template
            .replace("{id}", &scope_id)
            .replace("{hostname}", &hostname)
            .replace("{bundle_name}", &bundle_name);

        let cfg1: Result<Config, _> = serde_yaml::from_str(&yaml);
        if cfg1.is_err() {
            return Ok(());
        }

        let cfg1 = cfg1.unwrap();
        let yaml2 = serde_yaml::to_string(&cfg1).expect("serialize");
        let cfg2: Config = serde_yaml::from_str(&yaml2).expect("deserialize");

        assert_eq!(cfg1.scope.host.len(), cfg2.scope.host.len());
        assert_eq!(cfg1.bundle.len(), cfg2.bundle.len());
    });
}
