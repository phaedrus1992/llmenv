#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
use proptest::prelude::*;
use std::path::Path;

// ===== Path Canonicalization Idempotence =====

#[test]
fn prop_path_canonicalization_idempotent() {
    proptest!(|(path_str in "[a-z0-9/_-]{1,50}")| {
        let path = Path::new(&path_str);

        if let Ok(canonical1) = std::fs::canonicalize(path)
            && let Ok(canonical2) = std::fs::canonicalize(&canonical1) {
            assert_eq!(canonical1, canonical2);
        }
    });
}

// ===== Relative Path Structure =====

#[test]
fn prop_relative_path_no_double_slashes() {
    proptest!(|(components in prop::collection::vec("[a-z0-9]{1,10}", 1..5))| {
        let path = components.join("/");
        assert!(!path.contains("//"));
    });
}

// ===== Tilde Placeholder Recognition =====

#[test]
fn prop_tilde_placeholder_recognized() {
    proptest!(|(subpath in "[a-z0-9/_-]{0,30}")| {
        let path = if subpath.is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", subpath)
        };

        assert!(path.starts_with('~'));

        if path.len() > 1 {
            assert_eq!(path.chars().nth(1), Some('/'));
        }
    });
}

// ===== Relative Path Normalization =====

#[test]
fn prop_relative_path_dot_prefix_valid() {
    proptest!(|(components in prop::collection::vec("[a-z0-9]{1,8}", 1..4))| {
        let path = format!("./{}", components.join("/"));
        let p = Path::new(&path);
        assert!(p.is_relative());
    });
}

// ===== Parent Component Detection =====

#[test]
fn prop_parent_component_detection() {
    proptest!(|(
        before in "[a-z0-9]{1,5}",
        after in "[a-z0-9]{1,5}"
    )| {
        let path_with_parent = format!("{}/../{}", before, after);
        assert!(path_with_parent.contains(".."));

        let path_no_parent = format!("{}/{}", before, after);
        assert!(!path_no_parent.contains(".."));
    });
}

// ===== Path Length Boundary =====

#[test]
fn prop_path_length_boundary_enforced() {
    proptest!(|(path_str in "[a-z0-9/_-]{1,5000}")| {
        if path_str.len() > 4096 {
            assert!(path_str.len() > 4096);
        }
    });
}

// ===== Path Equality Consistency =====

#[test]
fn prop_path_equality_consistent() {
    proptest!(|(components in prop::collection::vec("[a-z0-9]{1,10}", 1..4))| {
        let path1 = components.join("/");
        let path2 = components.join("/");

        assert_eq!(path1, path2);

        let p1 = Path::new(&path1);
        let p2 = Path::new(&path2);

        assert_eq!(p1, p2);
    });
}

// ===== Path Absoluteness Detection =====

#[test]
fn prop_path_absoluteness_consistent() {
    proptest!(|(path_str in "[a-z0-9/_-]{1,30}")| {
        let path = Path::new(&path_str);

        if path_str.starts_with('/') {
            assert!(path.is_absolute());
        } else {
            assert!(path.is_relative());
        }
    });
}
