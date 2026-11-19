//! Inspect the command line of a process, to confirm that it is the memory proxy (#2923, #2948).

use crate::util::proc_start::ps_value;

/// A command line that starts the memory proxy: `mcp-proxy … -- <icm> serve`, where `<icm>` is
/// `icm` or an absolute path to it (#2948). A script runs through its interpreter, and `uvx` runs
/// the tool by name, so `mcp-proxy` is the first or the second word.
pub(crate) fn is_proxy_command(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    let names_proxy = words
        .iter()
        .take(2)
        .any(|w| w.rsplit('/').next() == Some("mcp-proxy"));
    let serves_icm = matches!(
        words.as_slice(),
        [.., "--", icm, "serve"] if icm.rsplit('/').next() == Some("icm")
    );
    names_proxy && serves_icm
}

/// The command line of `pid`, or `None` when the process is gone. `-ww` stops `ps` from cutting
/// a long line, which would hide the `-- icm serve` tail.
pub(crate) fn command_of(pid: u32) -> anyhow::Result<Option<String>> {
    ps_value("proxy:ps", pid, &["-ww", "-o", "command="], "command")
}

/// The uid of the user that owns `pid`, or `None` when the process is gone (#3835). The real uid
/// is the one that `kill(2)` compares with the uid of the caller.
pub(crate) fn uid_of(pid: u32) -> anyhow::Result<Option<u32>> {
    ps_value("proxy:ps-uid", pid, &["-o", "ruid="], "owner")?
        .map(|text| parse_uid(pid, &text))
        .transpose()
}

/// The uid that `ps` printed for `pid`.
fn parse_uid(pid: u32, text: &str) -> anyhow::Result<u32> {
    text.parse().map_err(|e| {
        anyhow::anyhow!("/bin/ps printed {text:?} for the owner of pid {pid}, not a uid: {e}")
    })
}

#[cfg(test)]
mod uid_tests {
    use super::parse_uid;
    use proptest::prelude::*;

    proptest! {
        /// A uid that `ps` printed reads back as the same number.
        #[test]
        fn a_printed_uid_reads_back(uid in any::<u32>()) {
            prop_assert_eq!(parse_uid(7, &uid.to_string()).ok(), Some(uid));
        }

        /// Text that is not an unsigned number is an error that names the pid.
        #[test]
        fn text_that_is_not_a_uid_is_an_error(text in "[a-z+ -]{1,8}") {
            let err = parse_uid(7, &text).err().map(|e| e.to_string());
            prop_assert!(err.is_some_and(|e| e.contains("pid 7")));
        }
    }
}
