use std::path::Path;

use super::{reject_native_disabled_tools, render_env, render_options, render_permission_lists};
use crate::config::{PermissionRule, Permissions};

fn rule(tool: &str) -> PermissionRule {
    PermissionRule {
        tool: tool.into(),
        pattern: None,
        paths: vec![],
    }
}

#[test]
fn unscoped_deny_becomes_disabled_tool() {
    let perms = Permissions {
        deny: vec![rule("Bash")],
        ..Default::default()
    };
    let (allowed, disabled) = render_permission_lists(&perms, None);
    assert!(allowed.is_empty());
    assert_eq!(disabled, vec!["bash".to_string()]);
}

#[test]
fn pattern_scoped_deny_is_not_rendered_as_disabled_tool() {
    let mut scoped = rule("Bash");
    scoped.pattern = Some("rm *".into());
    let perms = Permissions {
        deny: vec![scoped],
        ..Default::default()
    };
    let (_, disabled) = render_permission_lists(&perms, None);
    assert!(disabled.is_empty());
}

#[test]
fn deny_withholds_allow_of_the_same_tool() {
    let perms = Permissions {
        allow: vec![rule("Bash"), rule("Read")],
        deny: vec![rule("Bash")],
        ..Default::default()
    };
    let (allowed, disabled) = render_permission_lists(&perms, None);
    assert_eq!(allowed, vec!["view".to_string()]);
    assert_eq!(disabled, vec!["bash".to_string()]);
}

#[test]
fn task_and_ask_user_question_allow_map_to_crush_tools() {
    let perms = Permissions {
        allow: vec![rule("Task"), rule("AskUserQuestion")],
        ..Default::default()
    };
    let (allowed, _) = render_permission_lists(&perms, None);
    assert!(allowed.contains(&"agent".to_string()), "{allowed:?}");
    assert!(allowed.contains(&"question".to_string()), "{allowed:?}");
}

#[test]
fn render_options_is_none_when_nothing_applies() {
    assert!(
        render_options(Path::new("/x"), false, &[])
            .unwrap()
            .is_none()
    );
}

#[test]
fn render_options_carries_skills_and_disabled_tools() {
    let v = render_options(Path::new("/x"), true, &["bash".into()])
        .unwrap()
        .unwrap();
    assert_eq!(v["skills_paths"][0], "/x/skills");
    assert_eq!(v["disabled_tools"][0], "bash");
}

#[test]
fn render_options_omits_skills_key_without_skills() {
    let v = render_options(Path::new("/x"), false, &["bash".into()])
        .unwrap()
        .unwrap();
    assert!(v.get("skills_paths").is_none());
    assert_eq!(v["disabled_tools"][0], "bash");
}

#[test]
fn edit_deny_also_disables_multiedit() {
    let perms = Permissions {
        deny: vec![rule("Edit")],
        ..Default::default()
    };
    let (_, disabled) = render_permission_lists(&perms, None);
    assert!(disabled.contains(&"edit".to_string()), "{disabled:?}");
    assert!(disabled.contains(&"multiedit".to_string()), "{disabled:?}");
}

#[test]
fn webfetch_deny_also_disables_agentic_fetch() {
    let perms = Permissions {
        deny: vec![rule("WebFetch")],
        ..Default::default()
    };
    let (_, disabled) = render_permission_lists(&perms, None);
    assert!(
        disabled.contains(&"agentic_fetch".to_string()),
        "{disabled:?}"
    );
}

#[test]
fn write_deny_withholds_the_file_creating_edit_grants() {
    let perms = Permissions {
        allow: vec![rule("Edit"), rule("Read")],
        deny: vec![rule("Write")],
        ..Default::default()
    };
    let (allowed, disabled) = render_permission_lists(&perms, None);
    assert_eq!(allowed, vec!["view".to_string()]);
    assert_eq!(disabled, vec!["write".to_string()]);
}

#[test]
fn native_deny_and_ask_are_honored() {
    let native = crate::config::NativePermissionRules {
        allow: vec!["grep".into()],
        ask: vec!["ls".into()],
        deny: vec!["download".into()],
    };
    let (allowed, disabled) = render_permission_lists(&Permissions::default(), Some(&native));
    assert_eq!(allowed, vec!["grep".to_string()]);
    assert_eq!(disabled, vec!["download".to_string()]);
    assert!(disabled.contains(&"download".to_string()), "{disabled:?}");

    let ask_perms = Permissions {
        allow: vec![rule("Read")],
        ..Default::default()
    };
    let (allowed, _) = render_permission_lists(&ask_perms, Some(&native));
    assert!(allowed.contains(&"view".to_string()), "{allowed:?}");
    let (withheld_allowed, _) = render_permission_lists(
        &ask_perms,
        Some(&crate::config::NativePermissionRules {
            allow: vec!["view".into()],
            ask: vec!["view".into()],
            deny: vec![],
        }),
    );
    assert!(
        !withheld_allowed.contains(&"view".to_string()),
        "{withheld_allowed:?}"
    );
}

#[test]
fn native_options_disabled_tools_is_rejected() {
    let native: serde_yaml::Value =
        serde_yaml::from_str("options:\n  disabled_tools: []\n").unwrap();
    let err = reject_native_disabled_tools(Some(&native)).unwrap_err();
    assert!(err.to_string().contains("options.disabled_tools"), "{err}");
}

#[test]
fn native_options_without_disabled_tools_is_accepted() {
    let native: serde_yaml::Value =
        serde_yaml::from_str("options:\n  context_paths: []\n").unwrap();
    assert!(reject_native_disabled_tools(Some(&native)).is_ok());
    assert!(reject_native_disabled_tools(None).is_ok());
}

#[test]
fn env_values_are_escaped_so_crush_reads_them_literally() {
    let mut env = std::collections::BTreeMap::new();
    env.insert("CMD".to_string(), "$(id) `id` $HOME \\x".to_string());
    let rendered = render_env(&env);
    assert_eq!(rendered["CMD"], "\\$(id) \\`id\\` \\$HOME \\\\x");
}

#[test]
fn env_value_without_metacharacters_is_unchanged() {
    let mut env = std::collections::BTreeMap::new();
    env.insert("TOKEN_FILE".to_string(), "/run/tok".to_string());
    assert_eq!(render_env(&env)["TOKEN_FILE"], "/run/tok");
}

#[test]
fn lsp_env_values_are_escaped_so_crush_reads_them_literally() {
    let mut env = std::collections::BTreeMap::new();
    env.insert("CMD".to_string(), "$(id)".to_string());
    let srv = llmenv_config::LspServer {
        name: "ra".to_string(),
        command: "rust-analyzer".to_string(),
        env,
        ..Default::default()
    };
    let rendered = super::render_lsp(&[srv]).unwrap();
    assert_eq!(rendered["ra"]["env"]["CMD"], "\\$(id)");
}
