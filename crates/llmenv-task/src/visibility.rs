//! Which tasks sessions a caller may see. One rule serves the session reminders and the
//! status line, so the two cannot disagree (#3196).

use super::session::Session;

impl Session {
    /// Whether an end-of-turn reminder for `caller` may name this session. A caller with a
    /// conversation id does not see a session that another conversation owns (#2511). A session
    /// with no recorded owner stays visible: it was started outside an engine or before
    /// ownership existed, and hiding it would drop the reminder for work that nobody else owns.
    /// No caller id means no way to tell, so every session is visible.
    #[must_use]
    pub(crate) fn visible_to(&self, caller: Option<&str>) -> bool {
        owner_visible(self.owner_session(), caller)
    }

    /// Whether a new task from `caller` may join this session (#3480). A session with no owner
    /// takes any task. An owned session takes a task only from its owner, so a caller with no
    /// conversation id is refused. [`Session::visible_to`] allows that caller, which suits a
    /// reminder but not a write.
    #[must_use]
    pub(crate) fn may_take_tasks(&self, caller: Option<&str>) -> bool {
        match self.owner_session() {
            None => true,
            Some(owner) => caller == Some(owner),
        }
    }
}

/// Whether a session with `owner` is visible to `caller`. A session with no owner is visible to
/// every caller, and a caller with no id sees every session (#3185).
#[must_use]
pub fn owner_visible(owner: Option<&str>, caller: Option<&str>) -> bool {
    match (caller, owner) {
        (Some(caller), Some(owner)) => caller == owner,
        _ => true,
    }
}
