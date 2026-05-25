mod schema;
mod validate;

pub use schema::*;
pub use validate::ValidateError;

use std::path::Path;

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&s)?;
        cfg.validate()?;
        Ok(cfg)
    }
}
