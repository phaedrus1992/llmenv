//! Small shared helpers with no better home.

/// Stable dedup preserving first-seen order. Lists here are small (permission
/// rules, hooks, plugin ids), so the quadratic scan is fine and avoids
/// requiring `Hash`/`Ord` on every element type.
pub fn dedup<T: PartialEq>(items: &mut Vec<T>) {
    let mut i = 0;
    while i < items.len() {
        if items[..i].contains(&items[i]) {
            items.remove(i);
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dedup;

    #[test]
    fn removes_later_duplicates_preserving_order() {
        let mut v = vec!["a", "b", "a", "c", "b"];
        dedup(&mut v);
        assert_eq!(v, vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_and_singleton_are_noops() {
        let mut empty: Vec<i32> = Vec::new();
        dedup(&mut empty);
        assert!(empty.is_empty());
        let mut one = vec![1];
        dedup(&mut one);
        assert_eq!(one, vec![1]);
    }

    #[test]
    fn idempotent() {
        let mut v = vec![1, 1, 2, 3, 3, 3];
        dedup(&mut v);
        let once = v.clone();
        dedup(&mut v);
        assert_eq!(v, once);
    }
}
