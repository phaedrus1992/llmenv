//! The reason a caller is refused. Each refusal states this text, so every path names the same
//! reason (#3861).

use super::Caller;

/// Why `caller` may not act for a session it does not own.
pub fn caller_reason(caller: &Caller) -> String {
    match caller {
        Caller::Session(id) => format!("your session is '{id}'"),
        Caller::Unidentified(reason) => reason.clone(),
    }
}
