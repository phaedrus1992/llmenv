use super::is_valid_hostname;
use proptest::prelude::*;

#[test]
fn a_name_of_exactly_253_octets_is_accepted() {
    let labels = [
        "a".repeat(63),
        "a".repeat(63),
        "a".repeat(63),
        "a".repeat(61),
    ];
    let name = labels.join(".");
    assert_eq!(name.len(), 253);
    assert!(is_valid_hostname(&name));
}

proptest! {
    /// A name over 253 octets is refused, even when every label is valid.
    #[test]
    fn a_name_over_253_octets_is_refused(labels in 4usize..9) {
        let name = vec!["a".repeat(63); labels].join(".");
        prop_assert!(name.len() > 253);
        prop_assert!(!is_valid_hostname(&name));
    }

    /// A hyphen at the start or the end of any one label refuses the whole name.
    #[test]
    fn a_label_edged_by_a_hyphen_refuses_the_name(
        mut labels in proptest::collection::vec("[a-z0-9]{1,8}", 1..4),
        index in 0usize..4,
        leading in any::<bool>(),
    ) {
        let at = index % labels.len();
        labels[at] = if leading {
            format!("-{}", labels[at])
        } else {
            format!("{}-", labels[at])
        };
        prop_assert!(!is_valid_hostname(&labels.join(".")));
    }
}
