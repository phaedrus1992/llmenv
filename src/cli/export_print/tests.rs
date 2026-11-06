//! Tests for the export output: value quoting and the all-or-nothing export.

#![expect(
    clippy::expect_used,
    reason = "test code: a failed fixture stops the test"
)]

use super::export_lines;
use proptest::prelude::*;
use std::collections::BTreeMap;

#[cfg(unix)]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// A shell that reads the quoted value gets the same bytes back.
    #[test]
    fn a_quoted_value_reads_back_unchanged_in_sh(value in "[^\\x00]{0,64}") {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", llmenv_util::shell::single_quote(&value)))
            .output()
            .expect("run sh");
        prop_assert!(output.status.success());
        prop_assert_eq!(output.stdout, value.as_bytes().to_vec());
    }
}

#[test]
fn one_bad_value_fails_the_whole_export() {
    let mut vars = BTreeMap::new();
    vars.insert("LLMENV_GOOD".to_string(), "fine".to_string());
    vars.insert("LLMENV_BAD".to_string(), "a\u{0}b".to_string());
    assert!(export_lines(&vars, None).is_err());
}

#[test]
fn an_explained_line_names_its_source_before_the_export() {
    let mut vars = BTreeMap::new();
    vars.insert("LLMENV_ACTIVE_TAGS".to_string(), "rust".to_string());
    let lines = export_lines(&vars, Some("rust-dev")).expect("a valid export");
    assert_eq!(lines[0], "# source: llmenv introspection");
    assert!(
        lines[1].starts_with("export LLMENV_ACTIVE_TAGS="),
        "{lines:?}"
    );
}

mod scope_selection {
    use super::super::select_firing;
    use crate::config::{Bundle, Config};
    use crate::scope::matcher::Env;
    use crate::scope::{ActiveScope, ActiveScopes};
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    const TAGS: [&str; 5] = ["t0", "t1", "t2", "lang-x", "macos"];
    const BUNDLES: [&str; 4] = ["b0", "b1", "b2", "b3"];

    fn pick(pool: &[&str], mask: u8) -> Vec<String> {
        pool.iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, s)| (*s).to_string())
            .collect()
    }

    fn scope(id: usize, tags: u8, enable: u8, disable: u8) -> ActiveScope {
        ActiveScope {
            id: format!("s{id}"),
            kind: "project",
            tags: pick(&TAGS, tags),
            enable_bundles: pick(&BUNDLES, enable),
            disable_bundles: pick(&BUNDLES, disable),
            ..ActiveScope::default()
        }
    }

    /// Active scopes as the detection builds them: every scope tag, the OS tag, and the extra tags.
    fn active(scopes: Vec<ActiveScope>, extra: u8, env: &Env) -> ActiveScopes {
        let extra_tags: BTreeSet<String> = pick(&TAGS, extra).into_iter().collect();
        let mut tags: BTreeSet<String> = scopes.iter().flat_map(|s| s.tags.clone()).collect();
        tags.insert(env.os.clone());
        tags.extend(extra_tags.iter().cloned());
        ActiveScopes {
            scopes,
            tags,
            extra_tags,
        }
    }

    fn names(bundles: &[&Bundle]) -> BTreeSet<String> {
        bundles.iter().map(|b| b.name.clone()).collect()
    }

    proptest! {
        /// With `--scope`, a known scope fires a subset of the bundles that fire without it, and
        /// an unknown scope has no scope or tag left to fire from (#3269, #1399).
        #[test]
        fn a_scope_narrows_the_firing_set_and_an_unknown_one_fires_nothing(
            raw_scopes in prop::collection::vec((0u8..32, 0u8..16, 0u8..16), 1..4),
            whens in prop::collection::vec(0u8..32, BUNDLES.len()),
            extra in 0u8..32,
            chosen in 0usize..4,
            tag in prop::option::of(prop::sample::select(TAGS.to_vec())),
        ) {
            let env = Env { os: "macos".to_string(), ..Env::default() };
            let scopes = raw_scopes
                .iter()
                .enumerate()
                .map(|(i, (t, e, d))| scope(i, *t, *e, *d))
                .collect();
            let active = active(scopes, extra, &env);
            let config = Config {
                bundle: BUNDLES
                    .iter()
                    .zip(&whens)
                    .map(|(name, mask)| Bundle { name: (*name).to_string(), when: pick(&TAGS, *mask) })
                    .collect(),
                ..Config::default()
            };
            let (all, _) = select_firing(&config, &active, &env, None, tag);
            let all = names(&all);
            let id = format!("s{chosen}");
            let (narrowed, scopes) = select_firing(&config, &active, &env, Some(&id), tag);
            if chosen < raw_scopes.len() {
                prop_assert!(names(&narrowed).is_subset(&all));
                prop_assert!(scopes.scopes.iter().all(|s| s.id == id));
                let disabled = crate::cli::marker_disabled_bundle_names(&active);
                prop_assert!(names(&narrowed).is_disjoint(&disabled.iter().cloned().collect()));
                prop_assert_eq!(crate::cli::marker_disabled_bundle_names(&scopes), disabled);
            } else {
                prop_assert!(names(&narrowed).is_subset(&all));
                prop_assert!(scopes.scopes.is_empty());
            }
        }
    }
}
