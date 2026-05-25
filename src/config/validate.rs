use super::Config;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ValidateError {
    #[error("duplicate scope id: {0}")]
    DuplicateScopeId(String),
    #[error("bundle {0} has no tags")]
    BundleNoTags(String),
}

impl Config {
    pub fn validate(&self) -> Result<(), ValidateError> {
        let mut seen = std::collections::HashSet::new();
        let ids = self
            .scope
            .network
            .iter()
            .map(|s| &s.id)
            .chain(self.scope.host.iter().map(|s| &s.id))
            .chain(self.scope.user.iter().map(|s| &s.id))
            .chain(self.scope.project.iter().map(|s| &s.id));
        for id in ids {
            if !seen.insert(id.clone()) {
                return Err(ValidateError::DuplicateScopeId(id.clone()));
            }
        }
        for b in &self.bundle {
            if b.tags.is_empty() {
                return Err(ValidateError::BundleNoTags(b.name.clone()));
            }
        }
        Ok(())
    }
}
