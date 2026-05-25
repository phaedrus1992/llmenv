use llme::config::{
    Config, HostMatch, HostScope, NetworkMatch, NetworkScope, ProjectMatch, ProjectScope, Scopes,
    UserMatch, UserScope,
};
use llme::scope::{Env, evaluate};

fn cfg() -> Config {
    Config {
        scope: Scopes {
            host: vec![HostScope {
                id: "h".into(),
                r#match: HostMatch {
                    hostname: Some("fixed".into()),
                },
                tags: vec!["icm-server".into()],
            }],
            user: vec![UserScope {
                id: "u".into(),
                r#match: UserMatch {
                    user: Some("breed".into()),
                },
                tags: vec!["base".into()],
            }],
            project: vec![ProjectScope {
                id: "p".into(),
                r#match: ProjectMatch {
                    path_prefix: Some("/home/breed/git/x".into()),
                    marker_file: None,
                },
                tags: vec!["x".into()],
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
fn matches_project_by_prefix() {
    let env = Env {
        hostname: "x".into(),
        user: "y".into(),
        cwd: "/home/breed/git/x/sub".into(),
        ..Env::empty()
    };
    let active = evaluate(&cfg(), &env);
    assert!(active.tags.contains("x"));
}

#[test]
fn precedence_order() {
    let env = Env {
        hostname: "fixed".into(),
        user: "breed".into(),
        cwd: "/home/breed/git/x".into(),
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
fn project_path_prefix_respects_component_boundary() {
    // `/home/breed/git/xyz` must NOT match prefix `/home/breed/git/x`.
    let cfg = Config {
        scope: Scopes {
            project: vec![ProjectScope {
                id: "p".into(),
                r#match: ProjectMatch {
                    path_prefix: Some("/home/breed/git/x".into()),
                    marker_file: None,
                },
                tags: vec!["x".into()],
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let env = Env {
        cwd: "/home/breed/git/xyz".into(),
        ..Env::empty()
    };
    assert!(!evaluate(&cfg, &env).tags.contains("x"));
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
fn project_matcher_uses_marker_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let nested = tmp.path().join("a/b/c");
    std::fs::create_dir_all(&nested).expect("mkdir");
    std::fs::write(tmp.path().join("a/.llme-marker"), "").expect("write");

    let cfg = Config {
        scope: Scopes {
            project: vec![ProjectScope {
                id: "p".into(),
                r#match: ProjectMatch {
                    path_prefix: None,
                    marker_file: Some(".llme-marker".into()),
                },
                tags: vec!["marked".into()],
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let env = Env {
        cwd: nested.to_string_lossy().into_owned(),
        ..Env::empty()
    };
    assert!(evaluate(&cfg, &env).tags.contains("marked"));
}
