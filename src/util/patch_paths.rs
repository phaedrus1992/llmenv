//! The file paths of an opencode `patch` call, which edits files through a patch text instead of
//! a `file_path` argument (#2840).
//!
//! The headers follow opencode's patch parser (`packages/util/src/patch.ts` at v2.0.31).

/// The headers that name a file the patch adds, deletes, updates, or moves to.
const HEADERS: &[&str] = &[
    "*** Add File: ",
    "*** Delete File: ",
    "*** Update File: ",
    "*** Move to: ",
];

/// Each file path that `patch_text` names, in order.
pub(crate) fn patch_paths(patch_text: &str) -> Vec<&str> {
    patch_text
        .lines()
        .filter_map(|line| {
            HEADERS
                .iter()
                .find_map(|header| line.trim_start().strip_prefix(header))
        })
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_header_names_its_path() {
        let text = "*** Begin Patch\n*** Add File: new.txt\n+x\n*** Update File: /a/b.rs\n\
                    *** Move to: c.rs\n@@\n-x\n+y\n*** Delete File: old.txt\n*** End Patch\n";
        assert_eq!(patch_paths(text), ["new.txt", "/a/b.rs", "c.rs", "old.txt"]);
    }

    #[test]
    fn a_body_line_is_not_a_header() {
        assert!(patch_paths("+*** Add File x\n-*** Delete File: y\n").is_empty());
        assert!(patch_paths("*** Add File: \n").is_empty());
    }
}
