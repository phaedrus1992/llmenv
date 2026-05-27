mod schema;
mod validate;

pub use schema::*;
pub use validate::ValidateError;

use std::path::Path;

impl Config {
    /// Load and validate a config from an **already-expanded** path.
    ///
    /// `load` does not perform tilde (`~`) expansion — the caller is
    /// responsible for expanding `~`/`~user` (e.g. via [`crate::paths`]) before
    /// calling. Passing a `~`-prefixed path will fail at `read_to_string`
    /// because the literal `~` is not resolved by the OS. A `debug_assert`
    /// guards this contract in debug builds.
    ///
    /// # Errors
    /// Returns an error if the file can't be read, isn't valid YAML, or fails
    /// schema validation.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        debug_assert!(
            !path.starts_with("~"),
            "Config::load expects an expanded path; got tilde-prefixed {}",
            path.display()
        );
        let s = std::fs::read_to_string(path)?;
        let cfg: Self = serde_yaml::from_str(&s)?;
        cfg.validate()?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_accepts_expanded_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(&p, "cache: {}\n").unwrap();
        // An absolute (already-expanded) path must not trip the debug_assert.
        assert!(Config::load(&p).is_ok());
    }

    #[test]
    #[should_panic(expected = "expanded path")]
    fn load_rejects_tilde_path_in_debug() {
        // The contract: callers expand `~` first. In debug builds this trips
        // the guard rather than failing obscurely at read_to_string.
        let _ = Config::load(Path::new("~/.config/llmenv/config.yaml"));
    }
}
