use super::write_hook_line;
use std::io::{Error, ErrorKind, Write};

/// A writer that fails every write with the given kind.
struct Failing(ErrorKind);

impl Write for Failing {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(Error::from(self.0))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn empty_output_writes_nothing_and_reports_nothing() {
    let mut buf = Vec::new();
    assert!(write_hook_line(&mut buf, "").is_none());
    assert!(buf.is_empty());
}

#[test]
fn output_is_written_as_one_line() {
    let mut buf = Vec::new();
    assert!(write_hook_line(&mut buf, "context").is_none());
    assert_eq!(buf, b"context\n");
}

#[test]
fn a_closed_pipe_is_not_reported() {
    assert!(write_hook_line(&mut Failing(ErrorKind::BrokenPipe), "x").is_none());
}

#[test]
fn any_other_write_error_is_reported() {
    let kind = write_hook_line(&mut Failing(ErrorKind::PermissionDenied), "x").map(|e| e.kind());
    assert_eq!(kind, Some(ErrorKind::PermissionDenied));
}
