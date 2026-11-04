//! One open session of the project, as the status line needs it (#3196).

use serde::{Deserialize, Serialize};

use super::data::SessionProgress;

/// One open session of the project, as the status line needs it (#3196).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SessionTasks {
    /// The conversation that owns the session. `None` for a session with no owner.
    pub owner: Option<String>,
    /// Done and total tasks of this session.
    pub progress: SessionProgress,
    /// The title of this session's task in progress, if any.
    pub current: Option<String>,
}
