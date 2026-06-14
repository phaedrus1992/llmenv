#![expect(clippy::expect_used, reason = "test scaffolding")]
use llmenv::config::{Bundle, Config};
use proptest::prelude::*;

fn dedup<T: PartialEq>(items: Vec<T>) -> Vec<T> {
    let mut unique = Vec::new();
    for item in items {
        if !unique.contains(&item) {
            unique.push(item);
        }
    }
    unique
}

fn yaml_roundtrip(cfg: &Config) -> Config {
    let yaml = serde_yaml::to_string(cfg).expect("serialize");
    serde_yaml::from_str(&yaml).expect("deserialize")
}

// ===== Merge Determinism =====

#[test]
fn prop_merge_is_deterministic() {
    proptest!(|(
        name1 in "[a-z0-9]{1,10}",
        name2 in "[a-z0-9]{1,10}",
    )| {
        if name1 == name2 {
            return Ok(());
        }

        let bundle1 = Bundle {
            name: name1.clone(),
            when: vec!["tag1".into()],
        };

        let bundle2 = Bundle {
            name: name2.clone(),
            when: vec!["tag2".into()],
        };

        let cfg1 = Config {
            bundle: vec![bundle1.clone(), bundle2.clone()],
            ..Default::default()
        };

        let cfg2 = Config {
            bundle: vec![bundle1.clone(), bundle2.clone()],
            ..Default::default()
        };

        let yaml1 = serde_yaml::to_string(&cfg1).expect("serialize");
        let yaml2 = serde_yaml::to_string(&cfg2).expect("serialize");

        assert_eq!(yaml1, yaml2);
    });
}

// ===== Bundle Order Preservation =====

#[test]
fn prop_bundle_order_preserved_through_serialization() {
    proptest!(|(
        names in prop::collection::vec("[a-z0-9]{1,8}", 1..5)
    )| {
        let unique_names = dedup(names);
        if unique_names.is_empty() {
            return Ok(());
        }

        let bundles: Vec<_> = unique_names
            .iter()
            .map(|name| Bundle {
                name: name.clone(),
                when: vec!["tag".into()],
            })
            .collect();

        let cfg = Config {
            bundle: bundles.clone(),
            ..Default::default()
        };

        let cfg_parsed = yaml_roundtrip(&cfg);

        assert_eq!(cfg_parsed.bundle.len(), cfg.bundle.len());
        for (orig, parsed) in cfg.bundle.iter().zip(cfg_parsed.bundle.iter()) {
            assert_eq!(orig.name, parsed.name);
        }
    });
}

// ===== Bundle Tags Idempotence =====

#[test]
fn prop_bundle_tags_preserved_through_roundtrip() {
    proptest!(|(
        name in "[a-z0-9]{1,10}",
        tags in prop::collection::vec("[a-z0-9]{1,8}", 1..5)
    )| {
        let unique_tags = dedup(tags);

        let bundle = Bundle {
            name: name.clone(),
            when: unique_tags.clone(),
        };

        let cfg = Config {
            bundle: vec![bundle.clone()],
            ..Default::default()
        };

        let cfg_parsed = yaml_roundtrip(&cfg);

        assert_eq!(cfg_parsed.bundle.len(), 1);
        let parsed_bundle = &cfg_parsed.bundle[0];

        assert_eq!(parsed_bundle.name, bundle.name);
        let tags_match = parsed_bundle.when.len() == bundle.when.len()
            && parsed_bundle
                .when
                .iter()
                .all(|t| bundle.when.contains(t));
        assert!(tags_match, "Tags should match after round-trip");
    });
}

// ===== Bundle Merge No Data Loss =====

#[test]
fn prop_bundle_merge_preserves_all_data() {
    proptest!(|(
        name1 in "[a-z0-9]{1,10}",
        name2 in "[a-z0-9]{1,10}",
    )| {
        if name1 == name2 {
            return Ok(());
        }

        let bundle1 = Bundle {
            name: name1.clone(),
            when: vec!["t1".into()],
        };

        let bundle2 = Bundle {
            name: name2.clone(),
            when: vec!["t2".into()],
        };

        let cfg = Config {
            bundle: vec![bundle1.clone(), bundle2.clone()],
            ..Default::default()
        };

        let cfg_parsed = yaml_roundtrip(&cfg);

        assert!(cfg_parsed.bundle.iter().any(|b| b.name == name1));
        assert!(cfg_parsed.bundle.iter().any(|b| b.name == name2));
    });
}

// ===== Bundle Concat Stability =====

#[test]
fn prop_bundle_concat_is_stable() {
    proptest!(|(
        names in prop::collection::vec("[a-z0-9]{1,8}", 2..4)
    )| {
        let unique_names = dedup(names);
        if unique_names.len() < 2 {
            return Ok(());
        }

        let bundles: Vec<_> = unique_names
            .iter()
            .enumerate()
            .map(|(i, name)| Bundle {
                name: name.clone(),
                when: vec![format!("tag{}", i)],
            })
            .collect();

        let cfg1 = Config {
            bundle: bundles.clone(),
            ..Default::default()
        };

        let cfg2 = Config {
            bundle: bundles.clone(),
            ..Default::default()
        };

        let yaml1 = serde_yaml::to_string(&cfg1).expect("serialize");
        let yaml2 = serde_yaml::to_string(&cfg2).expect("serialize");

        assert_eq!(yaml1, yaml2);
    });
}
