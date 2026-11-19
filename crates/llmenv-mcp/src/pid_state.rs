//! Who a pid names: a process of this user, a process of another user, or nothing.
//!
//! The proxy stop path asks this question before it sends a signal.

use anyhow::Context as _;

/// What a pid names (#3835).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PidState {
    /// A running process of this user. This includes one that a sandbox does not let this user
    /// signal, because the process is still this user's.
    Own,
    /// A running process of another user. It is not this user's to stop.
    OtherUser,
    /// No process has this pid.
    Gone,
}

/// What `pid` names. A signal check answers for most pids. EPERM means the pid exists, but it does
/// not say whose it is: a process of another user gives EPERM, and so does this user's own
/// process under a sandbox that blocks signals. The owner of the pid settles it.
///
/// # Errors
/// Errors if the signal check fails for a reason other than "no such process" and EPERM, or when
/// `ps` cannot say who owns a pid that gave EPERM.
pub(crate) fn pid_state(pid: rustix::process::Pid) -> anyhow::Result<PidState> {
    match rustix::process::test_kill_process(pid) {
        Ok(()) => Ok(PidState::Own),
        Err(rustix::io::Errno::SRCH) => Ok(PidState::Gone),
        Err(rustix::io::Errno::PERM) => owner_state(pid.as_raw_nonzero().get().cast_unsigned()),
        Err(e) => Err(anyhow::anyhow!(
            "cannot check pid {}: {e}. Fix: check that this process may signal its own user's \
             processes",
            pid.as_raw_nonzero()
        )),
    }
}

/// Settle a pid that gave EPERM by the uid of its owner.
fn owner_state(pid: u32) -> anyhow::Result<PidState> {
    let uid = crate::proxy_command::uid_of(pid).with_context(|| {
        format!("pid {pid} gave EPERM, and its owner cannot be read. Fix: check that /bin/ps works")
    })?;
    Ok(state_for_owner(uid, rustix::process::geteuid().as_raw()))
}

/// The state of a pid that gave EPERM, from the uid of its owner and the uid of this user.
/// `None` means the process ended between the two checks.
fn state_for_owner(owner: Option<u32>, own: u32) -> PidState {
    match owner {
        None => PidState::Gone,
        Some(uid) if uid == own => PidState::Own,
        Some(_) => PidState::OtherUser,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_of_an_eperm_pid_decides_who_it_belongs_to() {
        assert_eq!(state_for_owner(Some(501), 501), PidState::Own);
        assert_eq!(state_for_owner(Some(0), 501), PidState::OtherUser);
        assert_eq!(state_for_owner(None, 501), PidState::Gone);
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "test code")]
    fn this_process_is_own_and_pid_1_is_not_this_users() {
        let me = rustix::process::Pid::from_raw(std::process::id().cast_signed()).unwrap();
        assert_eq!(pid_state(me).unwrap(), PidState::Own);
        if !rustix::process::geteuid().is_root() {
            // Pid 1 belongs to root on macOS and Linux, so the signal check gives EPERM.
            let init = rustix::process::Pid::from_raw(1).unwrap();
            assert_eq!(pid_state(init).unwrap(), PidState::OtherUser);
        }
    }
}
