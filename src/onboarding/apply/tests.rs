use std::collections::VecDeque;
use std::path::Path;

use super::{Prompter, apply};

/// Answers from a list, and records each question.
struct Script {
    answers: VecDeque<bool>,
    asked: Vec<String>,
    on_last: Option<Box<dyn FnMut()>>,
}

impl Script {
    fn new(answers: &[bool]) -> Self {
        Self {
            answers: answers.iter().copied().collect(),
            asked: Vec::new(),
            on_last: None,
        }
    }
}

impl Prompter for Script {
    fn confirm(&mut self, question: &str) -> anyhow::Result<bool> {
        self.asked.push(question.to_string());
        let answer = self.answers.pop_front().unwrap_or(false);
        if self.answers.is_empty()
            && let Some(hook) = self.on_last.as_mut()
        {
            hook();
        }
        Ok(answer)
    }
}

const CONFIG: &str = "# my settings\nbundle:\n- name: base\n  when: [me]\n# keep me\nmcp: []\n";

const PLAN: &str = r#"
bundles:
  - name: rust-dev
    when: [lang-rust]
    agents_md: "Use cargo nextest."
    capabilities:
      permissions:
        allow:
          - {tool: Bash, pattern: "cargo *"}
          - {tool: Bash, pattern: "rg *"}
"#;

struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new(config: &str, plan: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.yaml"), config).unwrap();
        std::fs::create_dir_all(dir.path().join("bundles/base")).unwrap();
        std::fs::write(dir.path().join("plan.yaml"), plan).unwrap();
        Self { dir }
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.dir.path().join(rel)
    }

    fn run(&self, script: &mut Script) -> (anyhow::Result<bool>, String) {
        let mut out = Vec::new();
        let result = apply(&self.path("plan.yaml"), self.dir.path(), script, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.path("config.yaml")).unwrap()
    }
}

#[test]
fn yes_to_everything_creates_the_bundle_and_lists_it() {
    let s = Setup::new(CONFIG, PLAN);
    let mut script = Script::new(&[true, true, true, true]);
    let (result, out) = s.run(&mut script);
    assert!(result.unwrap(), "{out}");
    assert_eq!(
        std::fs::read_to_string(s.path("bundles/rust-dev/AGENTS.md")).unwrap(),
        "Use cargo nextest."
    );
    let bundle = std::fs::read_to_string(s.path("bundles/rust-dev/bundle.yaml")).unwrap();
    assert!(
        bundle.contains("cargo *") && bundle.contains("rg *"),
        "{bundle}"
    );
    let config = s.config();
    assert!(
        config.starts_with("# my settings\nbundle:\n- name: base"),
        "{config}"
    );
    assert!(
        config.contains("# keep me") && config.contains("- name: rust-dev"),
        "{config}"
    );
    assert_eq!(
        script.asked.len(),
        4,
        "plan, two allows, config: {:?}",
        script.asked
    );
    assert!(script.asked[1].starts_with("Allow this permission allow in bundle rust-dev"));
    assert!(
        out.contains("+ - name: rust-dev"),
        "the diff is shown: {out}"
    );
}

#[test]
fn a_no_to_the_plan_writes_nothing() {
    let s = Setup::new(CONFIG, PLAN);
    let (result, out) = s.run(&mut Script::new(&[false]));
    assert!(!result.unwrap());
    assert!(out.contains("Nothing was written"), "{out}");
    assert!(!s.path("bundles/rust-dev").exists());
    assert_eq!(s.config(), CONFIG);
}

#[test]
fn a_no_to_one_item_leaves_only_that_item_out() {
    let s = Setup::new(CONFIG, PLAN);
    let (result, out) = s.run(&mut Script::new(&[true, false, true, true]));
    assert!(result.unwrap(), "{out}");
    let bundle = std::fs::read_to_string(s.path("bundles/rust-dev/bundle.yaml")).unwrap();
    assert!(
        !bundle.contains("cargo *") && bundle.contains("rg *"),
        "{bundle}"
    );
    assert!(out.contains("1 entry is left out"), "{out}");
}

#[test]
fn a_no_to_the_config_change_writes_nothing() {
    let s = Setup::new(CONFIG, PLAN);
    let (result, out) = s.run(&mut Script::new(&[true, true, true, false]));
    assert!(!result.unwrap());
    assert!(out.contains("never fires"), "{out}");
    assert!(!s.path("bundles/rust-dev").exists());
    assert_eq!(s.config(), CONFIG);
}

#[test]
fn an_existing_bundle_is_skipped_and_reported() {
    let plan = "bundles: [{name: base, when: [me], agents_md: x}]";
    let s = Setup::new(CONFIG, plan);
    let mut script = Script::new(&[]);
    let (result, out) = s.run(&mut script);
    assert!(!result.unwrap());
    assert!(
        out.contains("Skipped bundle base") && out.contains("adds nothing"),
        "{out}"
    );
    assert!(
        script.asked.is_empty(),
        "nothing to ask: {:?}",
        script.asked
    );
    assert!(!s.path("bundles/base/AGENTS.md").exists());
}

#[test]
fn a_config_edited_during_the_run_is_not_overwritten() {
    let s = Setup::new(CONFIG, PLAN);
    let config_path = s.path("config.yaml");
    let mut script = Script::new(&[true, true, true, true]);
    script.on_last = Some(Box::new(move || {
        std::fs::write(&config_path, format!("{CONFIG}# edited\n")).unwrap();
    }));
    let (result, _) = s.run(&mut script);
    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("changed while the plan ran"), "{err}");
    assert!(s.config().ends_with("# edited\n"));
    assert!(
        !s.path("bundles/rust-dev").exists(),
        "nothing is made before the config check"
    );
}

#[test]
fn a_bundle_that_does_not_load_fails_before_any_question() {
    let plan = "bundles: [{name: two, when: [x], capabilities: {hooks: [{event: Stop}]}}]";
    let s = Setup::new(CONFIG, plan);
    let mut script = Script::new(&[true]);
    let (result, _) = s.run(&mut script);
    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("do not merge"), "{err}");
    assert!(script.asked.is_empty());
}

#[test]
fn a_host_that_exists_is_refused() {
    let config = "bundle:\n- name: base\n  when: [me]\nhost:\n  still:\n    addr: 10.0.0.1\n";
    let s = Setup::new(config, "host: {still: {addr: still.local}}");
    let err = format!("{:#}", s.run(&mut Script::new(&[])).0.unwrap_err());
    assert!(err.contains("already has") && err.contains("Fix:"), "{err}");
}

#[test]
fn a_missing_config_names_the_fix() {
    let dir = tempfile::tempdir().unwrap();
    let plan = dir.path().join("plan.yaml");
    std::fs::write(&plan, PLAN).unwrap();
    let mut out = Vec::new();
    let err = apply(&plan, dir.path(), &mut Script::new(&[]), &mut out).unwrap_err();
    assert!(
        format!("{err:#}").contains("run `llmenv setup` first"),
        "{err:#}"
    );
    assert!(!Path::new(&dir.path().join("bundles")).exists());
}

#[test]
fn a_scope_makes_the_new_bundle_fire_and_needs_no_item_question() {
    let plan = "bundles: [{name: rust-dev, when: [lang-rust], agents_md: x}]\n\
                scope: {content: [{id: rust, match: {glob: Cargo.toml}, tags: [lang-rust]}]}\n";
    let s = Setup::new(CONFIG, plan);
    let mut script = Script::new(&[true, true]);
    let (result, out) = s.run(&mut script);
    assert!(result.unwrap(), "{out}");
    assert_eq!(
        script.asked,
        ["Apply this plan?", "Add these lines to config.yaml?"]
    );
    let config: serde_yaml::Value = serde_yaml::from_str(&s.config()).unwrap();
    assert_eq!(
        config["scope"]["content"][0]["tags"][0],
        serde_yaml::Value::from("lang-rust")
    );
}

#[test]
fn a_failed_write_removes_what_it_made_and_keeps_the_config() {
    let s = Setup::new(CONFIG, PLAN);
    let blocker = s.path("bundles/rust-dev");
    let mut script = Script::new(&[true, true, true, true]);
    // Something takes the bundle path while the user answers.
    script.on_last = Some(Box::new(move || std::fs::write(&blocker, "x").unwrap()));
    let (result, _) = s.run(&mut script);
    let err = format!("{:#}", result.unwrap_err());
    assert!(
        err.contains("never writes into a folder it did not make"),
        "{err}"
    );
    assert!(err.contains("config.yaml is unchanged"), "{err}");
    assert_eq!(s.config(), CONFIG);
    assert_eq!(
        std::fs::read_to_string(s.path("bundles/rust-dev")).unwrap(),
        "x"
    );
}

#[test]
fn a_host_that_a_bundle_declares_is_refused() {
    let s = Setup::new(CONFIG, "host: {still: {addr: evil.example}}");
    std::fs::write(
        s.path("bundles/base/bundle.yaml"),
        "host:\n  still:\n    addr: 10.0.0.1\n",
    )
    .unwrap();
    let err = format!("{:#}", s.run(&mut Script::new(&[])).0.unwrap_err());
    assert!(err.contains("config.yaml or a bundle already has"), "{err}");
}

#[test]
fn a_no_to_every_entry_of_an_entry_only_plan_writes_nothing() {
    let s = Setup::new(CONFIG, "features: {codebase_memory: [{when: [me]}]}");
    let mut script = Script::new(&[true, false]);
    let (result, out) = s.run(&mut script);
    assert!(!result.unwrap());
    assert!(out.contains("Every entry got a no"), "{out}");
    assert_eq!(s.config(), CONFIG);
}

#[test]
fn the_plan_summary_lists_bundles_files_and_the_entry_count() {
    let s = Setup::new(CONFIG, PLAN);
    let (_, out) = s.run(&mut Script::new(&[false]));
    for expected in [
        "The plan adds:\n",
        "  bundle rust-dev (fires on: lang-rust)\n",
        "  file bundles/rust-dev/bundle.yaml\n",
        "  file bundles/rust-dev/AGENTS.md\n",
        "2 entries need a separate yes.\n",
    ] {
        assert!(out.contains(expected), "{expected:?} missing: {out}");
    }
    let s = Setup::new(CONFIG, "host: {still: {addr: still.local}}");
    let (_, out) = s.run(&mut Script::new(&[false]));
    assert!(out.contains("1 entry needs a separate yes.\n"), "{out}");
}

/// The limit is 4 MiB, as for every other read of config.yaml. The config with the plan added
/// must also fit, so the accepted case stays well below the limit.
#[test]
fn a_large_config_is_read_and_one_over_the_limit_is_refused() {
    let pad = |size: usize| format!("{CONFIG}#{}\n", "x".repeat(size - CONFIG.len() - 2));
    let s = Setup::new(&pad(2 * 1024 * 1024), PLAN);
    let (result, out) = s.run(&mut Script::new(&[false]));
    assert!(!result.unwrap(), "a 2 MiB config is read: {out}");
    let s = Setup::new(&pad(4 * 1024 * 1024 + 1), PLAN);
    let (result, out) = s.run(&mut Script::new(&[false]));
    let err = format!("{:#}", result.unwrap_err());
    assert!(
        out.is_empty(),
        "the read fails before the plan is shown: {out}"
    );
    assert!(err.contains("config.yaml"), "{err}");
}

/// objc-dev has no bundle.yaml, so the plan asks two questions only. Its skill script is three
/// folders deep, and the skill runs it directly.
#[test]
fn a_shipped_bundle_is_created_with_its_folders_and_modes() {
    let s = Setup::new(CONFIG, "bundles: [{name: objc-dev, shipped: true}]");
    let mut script = Script::new(&[true, true]);
    let (result, out) = s.run(&mut script);
    assert!(result.unwrap(), "{out}");
    assert_eq!(script.asked.len(), 2, "{:?}", script.asked);
    for file in crate::onboarding::shipped::files("objc-dev") {
        let path = s.path("bundles/objc-dev").join(file.path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file.text);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o100 != 0, file.executable, "{}", path.display());
        }
    }
    let config: serde_yaml::Value = serde_yaml::from_str(&s.config()).unwrap();
    assert_eq!(
        config["bundle"][1]["when"][0],
        serde_yaml::Value::from("lang-objc")
    );
}

mod undo;

mod folders;
