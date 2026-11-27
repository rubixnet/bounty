//! Daemon side of one capture: spawn `capture-frame`, kill it after
//! [`CAPTURE_CHILD_TIMEOUT`](super::CAPTURE_CHILD_TIMEOUT), parse its one
//! JSON line. Pixels never enter this process: only the line is read.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::child::{ChildArgs, ChildReport};

/// Longest stdout accepted from the child (one short JSON line).
const MAX_CHILD_STDOUT: u64 = 1024;

/// Runs one child to completion or until `timeout`, then kills it. `None`
/// when it was killed, failed to start, or printed anything but one valid
/// report line.
pub fn run_child(program: &Path, args: &ChildArgs, timeout: Duration) -> Option<ChildReport> {
    let mut child = Command::new(program)
        .args(args.to_args())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child
        .stdout
        .take()?
        .take(MAX_CHILD_STDOUT)
        .read_to_string(&mut out)
        .ok()?;
    parse_report(&out)
}

/// Exactly one line holding one report.
pub fn parse_report(out: &str) -> Option<ChildReport> {
    let line = out.strip_suffix('\n')?;
    if line.contains('\n') {
        return None;
    }
    ChildReport::parse(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_valid_line_is_a_report() {
        assert_eq!(
            parse_report("{\"status\":\"error\"}\n"),
            Some(ChildReport::Error)
        );
        assert_eq!(parse_report("{\"status\":\"error\"}"), None);
        assert_eq!(parse_report("x\n{\"status\":\"error\"}\n"), None);
        assert_eq!(parse_report("{\"status\":\"error\",\"t\":1}\n"), None);
        assert_eq!(parse_report(""), None);
    }
}
