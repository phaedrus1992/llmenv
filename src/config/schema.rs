use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub scope: Scopes,
    #[serde(default)]
    pub bundle: Vec<Bundle>,
    pub icm: Option<Icm>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    pub cache_dir: String,
    pub sync_interval_minutes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cache_dir: "~/.cache/llmenv".into(),
            sync_interval_minutes: 15,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct Scopes {
    #[serde(default)]
    pub network: Vec<NetworkScope>,
    #[serde(default)]
    pub host: Vec<HostScope>,
    #[serde(default)]
    pub user: Vec<UserScope>,
    #[serde(default)]
    pub project: Vec<ProjectScope>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NetworkScope {
    pub id: String,
    pub r#match: NetworkMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NetworkMatch {
    pub gateway_mac: Option<String>,
    pub ssid: Option<String>,
    pub cidr: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostScope {
    pub id: String,
    pub r#match: HostMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostMatch {
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserScope {
    pub id: String,
    pub r#match: UserMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserMatch {
    pub user: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProjectScope {
    pub id: String,
    pub r#match: ProjectMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProjectMatch {
    pub path_prefix: Option<String>,
    pub marker_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Bundle {
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Icm {
    pub server_tag: String,
    pub server_bind: String,
    pub client_url: String,
    #[serde(default)]
    pub default_topics: Vec<String>,
}
