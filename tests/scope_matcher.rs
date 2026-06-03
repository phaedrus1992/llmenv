#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use llmenv::config::{
    Config, HostMatch, HostScope, NetworkMatch, NetworkScope, Scopes, UserMatch, UserScope,
};
use llmenv::scope::{Env, evaluate};

fn cfg() -> Config {
    Config {
        scope: Scopes {
            host: vec![HostScope {
                id: "h".into(),
                r#match: HostMatch {
                    hostname: Some("fixed".into()),
                },
                tags: vec!["icm-server".into()],
                env: Default::default(),
            }],
            user: vec![UserScope {
                id: "u".into(),
                r#match: UserMatch {
                    user: Some("breed".into()),
                },
                tags: vec!["base".into()],
                env: Default::default(),
            }],
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn matches_user_and_host() {
    let env = Env {
        hostname: "fixed".into(),
        user: "breed".into(),
        cwd: "/tmp".into(),
        ..Env::empty()
    };
    let active = evaluate(&cfg(), &env);
    let ids: Vec<&str> = active.scopes.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["h", "u"]);
    assert!(active.tags.contains("icm-server"));
    assert!(active.tags.contains("base"));
}

#[test]
fn matches_project_from_llmenv_yaml() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let yaml_path = tmp.path().join(".llmenv.yaml");
    std::fs::write(&yaml_path, "id: myproj\nname: MyProject\ntags: [x]\n").expect("write yaml");

    let env = Env {
        hostname: "x".into(),
        user: "y".into(),
        cwd: tmp.path().to_string_lossy().into_owned(),
        ..Env::empty()
    };
    let active = evaluate(&cfg(), &env);
    assert!(active.tags.contains("x"));
    let project_scope = active.scopes.iter().find(|s| s.kind == "project");
    assert!(project_scope.is_some());
    assert_eq!(project_scope.unwrap().id, "myproj");
    assert_eq!(project_scope.unwrap().name, Some("MyProject".to_string()));
}

#[test]
fn precedence_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(".llmenv.yaml"), "").expect("write yaml");

    let env = Env {
        hostname: "fixed".into(),
        user: "breed".into(),
        cwd: tmp.path().to_string_lossy().into_owned(),
        ..Env::empty()
    };
    let active = evaluate(&cfg(), &env);
    let kinds: Vec<&str> = active.scopes.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, vec!["host", "user", "project"]);
}

#[test]
fn no_match_returns_empty() {
    let env = Env {
        hostname: "other".into(),
        user: "nobody".into(),
        cwd: "/tmp".into(),
        ..Env::empty()
    };
    let active = evaluate(&cfg(), &env);
    assert!(active.scopes.is_empty());
    assert!(active.tags.is_empty());
}

#[test]
fn network_matcher_uses_gateway_mac() {
    let cfg = Config {
        scope: Scopes {
            network: vec![NetworkScope {
                id: "home".into(),
                r#match: NetworkMatch {
                    gateway_mac: Some("aa:bb:cc:dd:ee:ff".into()),
                    ssid: None,
                    cidr: None,
                },
                tags: vec!["home".into()],
                env: Default::default(),
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let env = Env {
        gateway_mac: Some("aa:bb:cc:dd:ee:ff".into()),
        ..Env::empty()
    };
    assert!(evaluate(&cfg, &env).tags.contains("home"));
}

#[test]
fn network_matcher_rejects_wrong_mac() {
    let cfg = Config {
        scope: Scopes {
            network: vec![NetworkScope {
                id: "home".into(),
                r#match: NetworkMatch {
                    gateway_mac: Some("aa:bb:cc:dd:ee:ff".into()),
                    ssid: None,
                    cidr: None,
                },
                tags: vec!["home".into()],
                env: Default::default(),
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let env = Env {
        gateway_mac: Some("11:22:33:44:55:66".into()),
        ..Env::empty()
    };
    assert!(!evaluate(&cfg, &env).tags.contains("home"));
}

#[test]
fn host_matcher_is_case_insensitive() {
    let cfg = Config {
        scope: Scopes {
            host: vec![HostScope {
                id: "h".into(),
                r#match: HostMatch {
                    hostname: Some("Fixed".into()),
                },
                tags: vec!["t".into()],
                env: Default::default(),
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let env = Env {
        hostname: "fixed".into(),
        ..Env::empty()
    };
    assert!(evaluate(&cfg, &env).tags.contains("t"));
}

#[test]
fn network_matcher_is_case_insensitive() {
    let cfg = Config {
        scope: Scopes {
            network: vec![NetworkScope {
                id: "home".into(),
                r#match: NetworkMatch {
                    gateway_mac: Some("AA:BB:CC:DD:EE:FF".into()),
                    ssid: None,
                    cidr: None,
                },
                tags: vec!["home".into()],
                env: Default::default(),
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let env = Env {
        gateway_mac: Some("aa:bb:cc:dd:ee:ff".into()),
        ..Env::empty()
    };
    assert!(evaluate(&cfg, &env).tags.contains("home"));
}

#[test]
fn project_marker_walks_upward() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let nested = tmp.path().join("a/b/c");
    std::fs::create_dir_all(&nested).expect("mkdir");
    std::fs::write(tmp.path().join(".llmenv.yaml"), "id: found\n").expect("write");

    let env = Env {
        cwd: nested.to_string_lossy().into_owned(),
        home: Some(tmp.path().to_path_buf()),
        ..Env::empty()
    };
    let active = evaluate(&cfg(), &env);
    let project = active.scopes.iter().find(|s| s.kind == "project");
    assert!(project.is_some());
    assert_eq!(project.unwrap().id, "found");
}

#[test]
fn project_marker_includes_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join(".llmenv.yaml"),
        "id: proj\ntags: [a, b, c]\n",
    )
    .expect("write");

    let env = Env {
        cwd: tmp.path().to_string_lossy().into_owned(),
        home: Some(tmp.path().to_path_buf()),
        ..Env::empty()
    };
    let active = evaluate(&cfg(), &env);
    let project = active.scopes.iter().find(|s| s.kind == "project").unwrap();
    assert_eq!(project.tags, vec!["a", "b", "c"]);
}

#[test]
fn project_marker_includes_bundles() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join(".llmenv.yaml"),
        "id: proj\nenable_bundles: [base, dev]\n",
    )
    .expect("write");

    let env = Env {
        cwd: tmp.path().to_string_lossy().into_owned(),
        home: Some(tmp.path().to_path_buf()),
        ..Env::empty()
    };
    let active = evaluate(&cfg(), &env);
    let project = active.scopes.iter().find(|s| s.kind == "project").unwrap();
    assert_eq!(project.enable_bundles, vec!["base", "dev"]);
}

#[test]
fn project_marker_malformed_yaml_uses_defaults() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(".llmenv.yaml"), "not: [valid: yaml").expect("write");

    let env = Env {
        cwd: tmp.path().to_string_lossy().into_owned(),
        home: Some(tmp.path().to_path_buf()),
        ..Env::empty()
    };
    let active = evaluate(&cfg(), &env);
    let project = active.scopes.iter().find(|s| s.kind == "project");
    assert!(project.is_some());
    // Should default to folder basename
    let basename = tmp
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(project.unwrap().id, basename);
}
