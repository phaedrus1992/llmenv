//! Property tests of the parsers behind the `/tmp` guard (#4045).

use proptest::prelude::*;

use super::writers::{option_values, written_by_option};
use super::{all_operands, destination, is_tmp_path, redirect_target};

/// One path segment: a name, a `.`, or a `..`, so a path can hold every form that the guard
/// resolves.
fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[a-zA-Z0-9_-]{1,6}",
        1 => Just(".".to_string()),
        1 => Just("..".to_string()),
        1 => prop_oneof![Just("tmp"), Just("private"), Just("var"), Just("folders")]
            .prop_map(str::to_string),
    ]
}

fn absolute_path() -> impl Strategy<Value = String> {
    prop::collection::vec(segment(), 0..6).prop_map(|segments| format!("/{}", segments.join("/")))
}

/// An operand that no parser reads as an option.
fn operand() -> impl Strategy<Value = String> {
    "[a-z0-9][a-z0-9/._]{0,8}"
}

proptest! {
    #[test]
    fn a_path_without_a_leading_slash_is_never_a_tmp_path(path in "[^/][ -~]{0,20}") {
        prop_assert!(!is_tmp_path(&path));
    }

    #[test]
    fn repeated_slashes_and_dot_segments_do_not_change_the_verdict(path in absolute_path()) {
        let noisy = path.replace('/', "//./");
        prop_assert_eq!(is_tmp_path(&path), is_tmp_path(&noisy), "{} vs {}", path, noisy);
    }

    #[test]
    fn a_child_that_is_left_again_does_not_change_the_verdict(path in absolute_path()) {
        let walked = format!("{path}/child/..");
        prop_assert_eq!(is_tmp_path(&path), is_tmp_path(&walked));
    }

    #[test]
    fn a_name_that_is_left_before_the_path_starts_does_not_change_the_verdict(
        path in absolute_path(),
        detour in "[a-z0-9]{1,6}",
    ) {
        let detoured = format!("/{detour}/..{path}");
        prop_assert_eq!(is_tmp_path(&path), is_tmp_path(&detoured), "{} vs {}", path, detoured);
    }

    #[test]
    fn the_case_of_a_name_does_not_change_the_verdict(path in absolute_path()) {
        prop_assert_eq!(is_tmp_path(&path), is_tmp_path(&path.to_ascii_uppercase()));
    }

    #[test]
    fn every_path_under_a_temp_root_is_denied_whatever_follows(
        root in prop_oneof![
            Just("/tmp"),
            Just("/private/tmp"),
            Just("/var/folders"),
            Just("/private/var/folders"),
            Just("/var/tmp"),
            Just("/private/var/tmp"),
            Just("/dev/shm")
        ],
        rest in prop::collection::vec("[a-z0-9]{1,6}", 0..4),
    ) {
        let path = format!("{root}/{}", rest.join("/"));
        prop_assert!(is_tmp_path(&path), "{}", path);
    }

    #[test]
    fn a_redirect_gives_its_target_joined_or_in_the_next_word(
        fd in "[0-9]{0,2}",
        op in prop_oneof![Just(">"), Just(">>"), Just(">|"), Just(">&")],
        target in "[a-z/][a-z/._]{0,10}",
    ) {
        let joined = [format!("{fd}{op}{target}")];
        prop_assert_eq!(redirect_target(&joined, 0, None), Some(target.as_str()));
        let split = [format!("{fd}{op}"), target.clone()];
        prop_assert_eq!(redirect_target(&split, 0, None), Some(target.as_str()));
        let next_segment = [format!("{fd}{op}")];
        prop_assert_eq!(
            redirect_target(&next_segment, 0, Some(target.as_str())),
            Some(target.as_str())
        );
    }

    #[test]
    fn a_word_that_does_not_start_with_a_redirect_gives_no_target(
        word in "[a-zA-Z_./-][ -~]{0,20}",
    ) {
        let words = [word];
        prop_assert_eq!(redirect_target(&words, 0, Some("/tmp/x")), None);
    }

    #[test]
    fn the_redirect_parser_never_panics(word in "\\PC{0,24}", next in "\\PC{0,8}") {
        let words = [word];
        let _ = redirect_target(&words, 0, Some(next.as_str()));
    }

    #[test]
    fn every_operand_of_a_writer_comes_back_in_order(
        name in prop_oneof![Just("tee"), Just("mkdir"), Just("mktemp")],
        args in prop::collection::vec(operand(), 0..5),
    ) {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        prop_assert_eq!(all_operands(name, &words), words);
    }

    #[test]
    fn no_option_comes_back_as_an_operand_and_an_attached_value_does(
        value in operand(),
        flags in prop::collection::vec("-[a-z]{1,3}", 0..4),
    ) {
        let attached = format!("--tmpdir={value}");
        let mut words: Vec<&str> = flags.iter().map(String::as_str).collect();
        words.push(&attached);
        let operands = all_operands("mktemp", &words);
        prop_assert_eq!(operands, vec![value.as_str()]);
    }

    #[test]
    fn touch_skips_the_reference_file_and_keeps_the_rest(
        reference in operand(),
        targets in prop::collection::vec(operand(), 1..4),
    ) {
        let mut words: Vec<&str> = vec!["-r", reference.as_str()];
        words.extend(targets.iter().map(String::as_str));
        prop_assert_eq!(
            all_operands("touch", &words),
            targets.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_destination_is_the_last_operand_when_no_target_option_is_given(
        sources in prop::collection::vec(operand(), 1..5),
    ) {
        let words: Vec<&str> = sources.iter().map(String::as_str).collect();
        prop_assert_eq!(destination(&words), words.last().copied());
    }

    #[test]
    fn a_target_option_names_the_destination_wherever_it_stands(
        sources in prop::collection::vec(operand(), 1..4),
        target in operand(),
        at in 0usize..4,
        long in any::<bool>(),
    ) {
        let option = if long {
            vec![format!("--target-directory={target}")]
        } else {
            vec!["-t".to_string(), target.clone()]
        };
        let mut words: Vec<String> = sources.clone();
        let at = at.min(words.len());
        for (offset, word) in option.into_iter().enumerate() {
            words.insert(at + offset, word);
        }
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        // A source that stands between `-t` and its value cannot occur: the insert keeps them
        // together, so the value of `-t` is the next word.
        prop_assert_eq!(destination(&words), Some(target.as_str()));
    }

    #[test]
    fn options_are_never_the_destination(
        flags in prop::collection::vec("-[a-z]{1,2}", 0..4),
        dest in operand(),
    ) {
        prop_assume!(flags.iter().all(|f| f != "-t"));
        let mut words: Vec<&str> = flags.iter().map(String::as_str).collect();
        words.push(dest.as_str());
        prop_assert_eq!(destination(&words), Some(dest.as_str()));
    }

    #[test]
    fn an_option_value_is_found_in_each_form_and_wherever_the_option_stands(
        value in "[a-z/][a-z/._]{0,10}",
        before in prop::collection::vec(operand(), 0..3),
        after in prop::collection::vec(operand(), 0..3),
        form in 0usize..5,
    ) {
        let option: Vec<String> = match form {
            0 => vec!["-o".into(), value.clone()],
            1 => vec![format!("-o{value}")],
            2 => vec!["--output".into(), value.clone()],
            3 => vec![format!("--output={value}")],
            _ => vec![format!("-sSo{value}")],
        };
        let mut words: Vec<String> = before;
        words.extend(option);
        words.extend(after);
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        prop_assert_eq!(
            option_values(&words, &['o'], &["--output"]),
            vec![value.as_str()]
        );
    }

    #[test]
    fn operands_alone_give_no_option_value(words in prop::collection::vec(operand(), 0..6)) {
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        prop_assert!(option_values(&words, &['o'], &["--output"]).is_empty());
    }

    #[test]
    fn the_option_parsers_never_panic(
        name in prop_oneof![Just("git"), Just("curl"), Just("wget"), Just("dd"), Just("tar")],
        words in prop::collection::vec("\\PC{0,12}", 0..6),
    ) {
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        let _ = written_by_option(name, &words);
    }
}
