use super::is_safe_cache_dir;

#[test]
fn a_cache_dir_is_safe_up_to_the_longest_allowed_length_and_not_beyond() {
    assert!(is_safe_cache_dir(&"a".repeat(4096)));
    assert!(!is_safe_cache_dir(&"a".repeat(4097)));
}

#[test]
fn an_empty_cache_dir_is_not_safe() {
    assert!(!is_safe_cache_dir(""));
}
