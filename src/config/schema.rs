use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub scope: Scopes,
    #[serde(default)]
    pub bundle: Vec<Bundle>,
    pub icm: Option<Icm>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    #[serde(default = "default_sync_interval")]
    pub sync_interval_minutes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cache_dir: default_cache_dir(),
            sync_interval_minutes: default_sync_interval(),
        }
    }
}

fn default_cache_dir() -> String {
    "~/.cache/llmenv".into()
}

fn default_sync_interval() -> u64 {
    15
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkScope {
    pub id: String,
    pub r#match: NetworkMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkMatch {
    pub gateway_mac: Option<String>,
    pub ssid: Option<String>,
    pub cidr: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HostScope {
    pub id: String,
    pub r#match: HostMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HostMatch {
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserScope {
    pub id: String,
    pub r#match: UserMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserMatch {
    pub user: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectScope {
    pub id: String,
    pub r#match: ProjectMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectMatch {
    pub path_prefix: Option<String>,
    pub marker_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Bundle {
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Icm {
    pub server_tag: String,
    pub server_bind: String,
    pub client_url: String,
    #[serde(default)]
    pub default_topics: Vec<String>,
}
