//! The bounded tail read of a transcript file.

use std::path::Path;

/// How many trailing bytes to read. `transcript_path` comes from hook stdin, so
/// the read must stay bounded whatever the file is.
pub(super) const TAIL_BYTES: u64 = 1024 * 1024;

/// The last [`TAIL_BYTES`] of a regular file, starting at a line boundary.
/// `None` for anything that is not a readable regular file. A file that exists but cannot be
/// read is logged, because the checks that read the tail then pass with no word.
pub(super) fn read_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let logged = |step: &str, e: std::io::Error| {
        crate::agent_notice::record_logged(format!(
            "llmenv: the transcript {} cannot be {step}, so the checks that read it pass \
             unchecked: {e}. Fix: check its permissions.",
            path.display()
        ));
    };
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            logged("opened", e);
            return None;
        }
    };
    let meta = file.metadata().map_err(|e| logged("examined", e)).ok()?;
    if !meta.is_file() {
        return None;
    }
    let start = meta.len().saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))
        .map_err(|e| logged("read", e))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| logged("read", e))
        .ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if start == 0 {
        return Some(text);
    }
    // The first line after the seek is cut in the middle, so it is dropped.
    Some(
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default(),
    )
}
