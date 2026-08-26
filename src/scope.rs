pub use llmenv_scope::{ActiveScope, ActiveScopes, evaluate};

pub mod matcher {
    pub use llmenv_scope::matcher::{
        Env, ResolvedProject, discover_project, extra_tags_from_env, is_valid_tag_charset,
    };
}
