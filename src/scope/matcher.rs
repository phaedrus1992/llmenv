use crate::config::{HostScope, NetworkScope, ProjectScope, UserScope};

#[derive(Debug, Clone)]
pub struct Env {
    pub hostname: String,
    pub user: String,
    pub cwd: String,
    pub gateway_mac: Option<String>,
}

impl Env {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            hostname: String::new(),
            user: String::new(),
            cwd: String::new(),
            gateway_mac: None,
        }
    }

    #[must_use]
    pub fn detect() -> Self {
        Self {
            hostname: detect_hostname().unwrap_or_default(),
            user: std::env::var("USER").unwrap_or_default(),
            cwd: std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
                .unwrap_or_default(),
            gateway_mac: super::network::detect_gateway_mac(),
        }
    }
}

fn detect_hostname() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim().to_string())
}

#[must_use]
pub fn matches_network(s: &NetworkScope, env: &Env) -> bool {
    let Some(want) = s.r#match.gateway_mac.as_deref() else {
        // ssid/cidr are not yet supported for matching; without gateway_mac we cannot match.
        return false;
    };
    env.gateway_mac.as_deref() == Some(want)
}

#[must_use]
pub fn matches_host(s: &HostScope, env: &Env) -> bool {
    s.r#match
        .hostname
        .as_deref()
        .is_some_and(|h| h == env.hostname)
}

#[must_use]
pub fn matches_user(s: &UserScope, env: &Env) -> bool {
    s.r#match.user.as_deref().is_some_and(|u| u == env.user)
}

#[must_use]
pub fn matches_project(s: &ProjectScope, env: &Env) -> bool {
    if let Some(p) = s.r#match.path_prefix.as_deref() {
        let expanded = expand_tilde(p);
        if env.cwd.starts_with(&expanded) {
            return true;
        }
    }
    if let Some(marker) = s.r#match.marker_file.as_deref() {
        let mut cur = std::path::PathBuf::from(&env.cwd);
        loop {
            if cur.join(marker).exists() {
                return true;
            }
            if !cur.pop() {
                break;
            }
        }
    }
    false
}

fn expand_tilde(p: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return p.to_string();
    };
    if let Some(rest) = p.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else if p == "~" {
        home
    } else {
        p.to_string()
    }
}
