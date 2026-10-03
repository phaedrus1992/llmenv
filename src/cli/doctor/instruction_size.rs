//! `llmenv doctor`: the size of the instruction text Claude Code loads into every session (#2357).
//!
//! Design: docs/design/issue-2357-instruction-size.md

use super::CheckLevel;
use crate::merge::MergedManifest;
use crate::merge::rules::LoadMode;

/// The per-file size at which Claude Code warns about a large instruction file. Claude Code
/// 2.1.287 uses about 5% of the model's context window in characters, with a floor of 40,000.
/// Doctor does not know the model's window, so it uses the floor.
const CLAUDE_INSTRUCTION_FILE_LIMIT_CHARS: usize = 40_000;

/// The size at which the combined instruction text triggers Claude Code's notice. Estimated: the
/// 2.1.288 binary holds no combined constant (searched for the notice text and for the 40,000
/// constant near it), and the 2.1.281 changelog gives no number. The estimate is twice the
/// per-file floor.
const CLAUDE_INSTRUCTION_TOTAL_LIMIT_CHARS: usize = 2 * CLAUDE_INSTRUCTION_FILE_LIMIT_CHARS;

/// How many of the largest contributors doctor prints.
const TOP_CONTRIBUTORS: usize = 5;

/// The separator the Claude Code adapter writes before the slippage fragment.
const SLIPPAGE_SEPARATOR: &str = "<!-- from slippage control: compact_survival -->";

const FIX: &str = "fix it in the source bundle, then run llmenv regenerate; Claude Code's \
                   /doctor prompt-audit reads the generated copies.";

/// One always-loaded piece of instruction text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Contributor {
    pub bundle: String,
    pub file: String,
    pub chars: usize,
}

/// What `measure` finds in a merged manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InstructionSizeReport {
    pub total_chars: usize,
    pub claude_md_chars: usize,
    /// Every always-loaded contributor, largest first.
    pub always_loaded: Vec<Contributor>,
    pub path_filtered_rules: usize,
    pub unparsed_frontmatter: Vec<String>,
    /// The size of each rule file that loads in every session.
    rule_chars: Vec<(String, usize)>,
}

/// Split `CLAUDE.md` into one contributor per bundle, using the separator comments that
/// `agents_md::concat` and the slippage fragment write.
fn claude_md_chunks(text: &str) -> Vec<Contributor> {
    let mut chunks: Vec<(String, usize)> = Vec::new();
    let mut bundle = "(unattributed)".to_string();
    let mut chars = 0usize;
    for line in text.split_inclusive('\n') {
        if let Some(name) = separator_bundle(line) {
            if chars > 0 {
                chunks.push((bundle, chars));
            }
            bundle = name;
            chars = 0;
        }
        chars += line.chars().count();
    }
    if chars > 0 {
        chunks.push((bundle, chars));
    }
    chunks
        .into_iter()
        .map(|(bundle, chars)| Contributor {
            bundle,
            file: "CLAUDE.md".to_string(),
            chars,
        })
        .collect()
}

/// The bundle a separator line names, or `None` for any other line.
fn separator_bundle(line: &str) -> Option<String> {
    let line = line.trim_end();
    if line == SLIPPAGE_SEPARATOR {
        return Some("slippage control".to_string());
    }
    if let Some(rest) = line.strip_prefix("<!-- # from bundle: ") {
        let name = rest.strip_suffix(" -->")?;
        // A rule separator reads `<bundle> rules/<file>`; the bundle is the first word.
        return Some(name.split(' ').next().unwrap_or(name).to_string());
    }
    None
}

/// Measure the always-loaded instruction text of `manifest`.
fn measure(manifest: &MergedManifest) -> InstructionSizeReport {
    let claude_md = crate::adapter::claude_code::claude_md_content(manifest);
    let claude_md_chars = claude_md.chars().count();
    let mut always_loaded = claude_md_chunks(&claude_md);
    let mut rule_chars = Vec::new();
    let mut path_filtered_rules = 0;
    let mut unparsed_frontmatter = Vec::new();
    for rule in &manifest.rules {
        match rule.load_mode() {
            LoadMode::PathFiltered => {
                path_filtered_rules += 1;
                continue;
            }
            LoadMode::UnparsedFrontmatter => {
                unparsed_frontmatter.push(format!("{}/{}", rule.bundle, rule.rel.display()));
            }
            LoadMode::Always => {}
        }
        let chars = rule.raw.chars().count();
        let file = rule.rel.display().to_string();
        rule_chars.push((file.clone(), chars));
        always_loaded.push(Contributor {
            bundle: rule.bundle.clone(),
            file,
            chars,
        });
    }
    always_loaded.sort_by(|a, b| b.chars.cmp(&a.chars).then_with(|| a.file.cmp(&b.file)));
    let total_chars = claude_md_chars + rule_chars.iter().map(|(_, n)| n).sum::<usize>();
    InstructionSizeReport {
        total_chars,
        claude_md_chars,
        always_loaded,
        path_filtered_rules,
        unparsed_frontmatter,
        rule_chars,
    }
}

/// The doctor lines for a report.
fn checks(report: &InstructionSizeReport) -> Vec<(CheckLevel, String)> {
    let mut out = Vec::new();
    let mut files: Vec<(&str, usize)> = report
        .rule_chars
        .iter()
        .map(|(file, chars)| (file.as_str(), *chars))
        .collect();
    files.push(("CLAUDE.md", report.claude_md_chars));
    let mut over = false;
    for (file, chars) in files {
        if chars > CLAUDE_INSTRUCTION_FILE_LIMIT_CHARS {
            over = true;
            out.push((
                CheckLevel::Warn,
                format!(
                    "{file} is {chars} characters, over the {CLAUDE_INSTRUCTION_FILE_LIMIT_CHARS} \
                     that Claude Code accepts for one instruction file (the floor; the limit is higher for \
                     a large context window); {FIX}"
                ),
            ));
        }
    }
    if report.total_chars > CLAUDE_INSTRUCTION_TOTAL_LIMIT_CHARS {
        over = true;
        out.push((
            CheckLevel::Warn,
            format!(
                "always-loaded instructions total {} characters, over {} (estimated; Claude Code \
                 does not document the combined limit); {FIX}",
                report.total_chars, CLAUDE_INSTRUCTION_TOTAL_LIMIT_CHARS
            ),
        ));
    }
    if !over {
        out.push((
            CheckLevel::Pass,
            format!(
                "always-loaded instructions from llmenv bundles total {} characters ({} in CLAUDE.md)",
                report.total_chars, report.claude_md_chars
            ),
        ));
    }
    if report.path_filtered_rules > 0 {
        out.push((
            CheckLevel::Info,
            format!(
                "{} rules load on matching paths and are not counted",
                report.path_filtered_rules
            ),
        ));
    }
    if !report.unparsed_frontmatter.is_empty() {
        out.push((
            CheckLevel::Info,
            format!(
                "{} rules have frontmatter that llmenv cannot read and are counted as always \
                 loaded: {}",
                report.unparsed_frontmatter.len(),
                report.unparsed_frontmatter.join(", ")
            ),
        ));
    }
    out
}

/// The lines that name the largest contributors.
fn top_contributors(report: &InstructionSizeReport) -> Vec<String> {
    report
        .always_loaded
        .iter()
        .take(TOP_CONTRIBUTORS)
        .map(|c| format!("{}  {}  {} chars", c.bundle, c.file, c.chars))
        .collect()
}

/// Print the section.
pub(super) fn run_doctor_instruction_size(use_color: bool, manifest: &MergedManifest) {
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let info = super::super::doctor_info(use_color);
    let report = measure(manifest);
    eprintln!();
    eprintln!("Instruction size (Claude Code):");
    for check in checks(&report) {
        super::print_check(check, &pass, &warn, &info);
    }
    for line in top_contributors(&report) {
        eprintln!("{info} {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::rules::RuleFile;
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn rule(bundle: &str, rel: &str, frontmatter: Option<&str>, body: &str) -> RuleFile {
        let raw = match frontmatter {
            Some(f) => format!("---\n{f}\n---\n{body}"),
            None => body.to_string(),
        };
        RuleFile {
            bundle: bundle.into(),
            rel: PathBuf::from(rel),
            frontmatter: frontmatter.map(str::to_string),
            body: body.into(),
            raw,
        }
    }

    fn manifest(agents_md: &str, rules: Vec<RuleFile>) -> MergedManifest {
        MergedManifest {
            agents_md: agents_md.into(),
            rules,
            ..Default::default()
        }
    }

    #[test]
    fn the_load_mode_follows_the_paths_key() {
        let mode = |f: Option<&str>| rule("b", "r.md", f, "x").load_mode();
        assert_eq!(mode(None), LoadMode::Always);
        assert_eq!(mode(Some("")), LoadMode::Always);
        assert_eq!(mode(Some("paths: []")), LoadMode::Always);
        assert_eq!(mode(Some("description: x")), LoadMode::Always);
        assert_eq!(mode(Some("paths:\n  - src/**")), LoadMode::PathFiltered);
        assert_eq!(
            mode(Some("paths: [unclosed")),
            LoadMode::UnparsedFrontmatter
        );
    }

    #[test]
    fn measure_adds_up_and_excludes_path_filtered_rules() {
        let m = manifest(
            "<!-- # from bundle: base -->\nabc\n<!-- # from bundle: rust -->\nde\n",
            vec![
                rule("base", "rules/a.md", None, "12345"),
                rule("rust", "rules/b.md", Some("paths: [src/**]"), "zzzzzzzzzz"),
                rule("rust", "rules/c.md", Some("paths: [x"), "12"),
            ],
        );
        let r = measure(&m);
        assert_eq!(r.claude_md_chars, m.agents_md.chars().count());
        assert_eq!(r.path_filtered_rules, 1);
        assert_eq!(r.unparsed_frontmatter, ["rust/rules/c.md"]);
        let rules: usize = ["12345", "---\npaths: [x\n---\n12"]
            .iter()
            .map(|s| s.chars().count())
            .sum();
        assert_eq!(r.total_chars, r.claude_md_chars + rules);
        let sizes: Vec<usize> = r.always_loaded.iter().map(|c| c.chars).collect();
        let mut sorted = sizes.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(sizes, sorted);
    }

    #[test]
    fn claude_md_chunks_follow_the_separators() {
        let text = "intro\n<!-- # from bundle: base -->\naaa\n<!-- # from bundle: rust rules/x.md -->\nbb\n\n<!-- from slippage control: compact_survival -->\nc\n<!-- from a user note -->\nd\n";
        let chunks = claude_md_chunks(text);
        let named: Vec<(&str, usize)> = chunks
            .iter()
            .map(|c| (c.bundle.as_str(), c.chars))
            .collect();
        assert_eq!(
            chunks.iter().map(|c| c.chars).sum::<usize>(),
            text.chars().count()
        );
        assert_eq!(named[0].0, "(unattributed)");
        assert_eq!(named[1].0, "base");
        assert_eq!(named[2].0, "rust");
        assert_eq!(named[3].0, "slippage control");
    }

    fn report_of(claude_md: usize, rules: &[usize]) -> InstructionSizeReport {
        InstructionSizeReport {
            total_chars: claude_md + rules.iter().sum::<usize>(),
            claude_md_chars: claude_md,
            always_loaded: vec![],
            path_filtered_rules: 0,
            unparsed_frontmatter: vec![],
            rule_chars: rules
                .iter()
                .enumerate()
                .map(|(i, n)| (format!("rules/r{i}.md"), *n))
                .collect(),
        }
    }

    fn levels(r: &InstructionSizeReport) -> Vec<CheckLevel> {
        checks(r).into_iter().map(|(l, _)| l).collect()
    }

    #[test]
    fn checks_pass_under_both_limits_and_warn_over_either() {
        assert_eq!(levels(&report_of(1000, &[2000])), [CheckLevel::Pass]);
        let one = checks(&report_of(1000, &[50_000]));
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].0, CheckLevel::Warn);
        assert!(one[0].1.contains("rules/r0.md") && one[0].1.contains("source bundle"));
        let total = checks(&report_of(30_000, &[30_000, 30_000]));
        assert_eq!(total.len(), 1);
        assert!(total[0].1.contains("90000") && total[0].1.contains("estimated"));
        let both = checks(&report_of(10_000, &[45_000, 40_000]));
        assert_eq!(
            both.iter().filter(|(l, _)| *l == CheckLevel::Warn).count(),
            2
        );
    }

    #[test]
    fn the_limit_is_in_characters_not_bytes() {
        let at = "é".repeat(CLAUDE_INSTRUCTION_FILE_LIMIT_CHARS);
        let m = manifest("", vec![rule("b", "r.md", None, &at)]);
        assert_eq!(levels(&measure(&m)), [CheckLevel::Pass]);
        let over = "é".repeat(CLAUDE_INSTRUCTION_FILE_LIMIT_CHARS + 1);
        let m = manifest("", vec![rule("b", "r.md", None, &over)]);
        assert_eq!(levels(&measure(&m)), [CheckLevel::Warn]);
    }

    #[test]
    fn a_bundle_with_a_huge_rule_is_named() {
        let m = manifest(
            "",
            vec![rule(
                "big-bundle",
                "rules/huge.md",
                None,
                &"x".repeat(50_000),
            )],
        );
        let r = measure(&m);
        assert_eq!(
            top_contributors(&r)[0],
            "big-bundle  rules/huge.md  50000 chars"
        );
    }

    #[test]
    fn informational_lines_report_skipped_and_unparsed_rules() {
        let mut r = report_of(10, &[]);
        r.path_filtered_rules = 3;
        r.unparsed_frontmatter = vec!["a/x.md".into(), "b/y.md".into()];
        let lines = checks(&r);
        assert!(
            lines
                .iter()
                .any(|(l, t)| *l == CheckLevel::Info && t.starts_with("3 rules load"))
        );
        assert!(lines.iter().any(|(l, t)| *l == CheckLevel::Info
            && t.starts_with("2 rules have")
            && t.contains("a/x.md, b/y.md")));
    }

    #[test]
    fn only_five_contributors_are_printed() {
        let rules: Vec<RuleFile> = (0..8)
            .map(|i| rule("b", &format!("rules/{i}.md"), None, &"x".repeat(10 + i)))
            .collect();
        assert_eq!(
            top_contributors(&measure(&manifest("", rules))).len(),
            TOP_CONTRIBUTORS
        );
    }

    proptest! {
        #[test]
        fn the_total_is_claude_md_plus_the_always_loaded_rules(
            bodies in prop::collection::vec("[a-z é]{0,60}", 0..8),
            filtered in prop::collection::vec(any::<bool>(), 8),
            agents in "[a-z \n]{0,80}",
        ) {
            let rules: Vec<RuleFile> = bodies
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    let fm = filtered[i].then_some("paths: [src/**]");
                    rule("b", &format!("rules/{i}.md"), fm, b)
                })
                .collect();
            let m = manifest(&agents, rules.clone());
            let r = measure(&m);
            let expected: usize = rules
                .iter()
                .filter(|x| x.load_mode() != LoadMode::PathFiltered)
                .map(|x| x.raw.chars().count())
                .sum();
            prop_assert_eq!(r.total_chars, r.claude_md_chars + expected);
            let listed: usize = r.always_loaded.iter().map(|c| c.chars).sum();
            prop_assert_eq!(listed, r.total_chars);
        }
    }
}
