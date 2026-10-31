//! Folder planning for a new bundle: the folders between the bundle and each made file.

use std::path::Path;

use crate::onboarding::apply::new_folders;

proptest::proptest! {
    /// The folders to make are each folder below `bundles/<name>` and above the file, outermost
    /// first.
    #[test]
    fn new_folders_are_the_folders_between_the_bundle_and_the_file(
        name in "[a-z]{1,8}",
        dirs in proptest::collection::vec("[a-z]{1,6}", 0..4),
    ) {
        let bundle = Path::new("bundles").join(&name);
        let mut expected = Vec::new();
        let mut folder = bundle.clone();
        for dir in &dirs {
            folder = folder.join(dir);
            expected.push(folder.clone());
        }
        let rel = folder.join("file.md");
        let got: Vec<std::path::PathBuf> =
            new_folders(&rel).into_iter().map(Path::to_path_buf).collect();
        proptest::prop_assert_eq!(got, expected);
    }
}
