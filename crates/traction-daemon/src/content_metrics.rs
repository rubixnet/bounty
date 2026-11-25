//! Content-free metrics: the only code that reads raw Harness text (a
//! prompt from a Claude Code hook or transcript record). It returns numbers
//! and flags and keeps nothing, so the caller can drop the text at once.
//!
//! Shared by every Adapter that sees raw text (Claude Code hooks now, the
//! Claude transcript tailer next).

/// What a piece of raw text is reduced to before it is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextMetrics {
    /// Length in Unicode scalar values (not bytes).
    pub prompt_char_len: u64,
    pub has_stacktrace: bool,
}

/// Measures `text`. Linear in its length; allocates nothing.
pub fn measure(text: &str) -> TextMetrics {
    TextMetrics {
        prompt_char_len: u64::try_from(text.chars().count()).unwrap_or(u64::MAX),
        has_stacktrace: has_stacktrace(text),
    }
}

/// Whether `text` holds a pasted stack trace: one unambiguous header
/// (Python `Traceback`, Rust `panicked at` / `stack backtrace:`, a Go
/// `goroutine N [...]` dump, Java `Exception in thread`), or at least two
/// stack-frame lines from any common runtime. A single frame-looking line
/// is not enough: prose can resemble one.
pub fn has_stacktrace(text: &str) -> bool {
    let mut frames = 0;
    for line in text.lines() {
        let line = line.trim();
        if is_trace_header(line) {
            return true;
        }
        if is_frame(line) {
            frames += 1;
            if frames >= 2 {
                return true;
            }
        }
    }
    false
}

fn is_trace_header(line: &str) -> bool {
    line.starts_with("Traceback (most recent call last)")
        || line.starts_with("stack backtrace:")
        || line.starts_with("Exception in thread \"")
        || (line.starts_with("thread '") && line.contains("' panicked at "))
        || is_goroutine_header(line)
}

/// `goroutine 1 [running]:`
fn is_goroutine_header(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("goroutine ") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(" [") && line.ends_with("]:")
}

/// One stack frame, trimmed:
/// - Python: `File "app.py", line 3, in main`
/// - JavaScript, Java, Kotlin, Rust backtraces: `at f (a.js:1:2)`,
///   `at com.x.Y.z(Y.java:10)`, `at ./src/main.rs:5:9`
/// - C#: `at N.C.M() in C:\x\C.cs:line 12`
/// - Ruby: `from app.rb:10:in 'main'`
/// - Go: `/src/main.go:12 +0x1d`
fn is_frame(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix("File \"") {
        return rest.contains("\", line ");
    }
    if let Some(rest) = line.strip_prefix("at ") {
        return has_line_col(rest)
            || ends_with_line_paren(rest)
            || (rest.contains(" in ") && rest.contains(":line "));
    }
    if let Some(rest) = line.strip_prefix("from ") {
        return rest.contains(":in ") && has_colon_number(rest);
    }
    is_go_frame(line)
}

/// Contains `:<digits>:<digits>`.
fn has_line_col(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len()).any(|i| {
        let Some(after) = colon_digits(bytes, i) else {
            return false;
        };
        colon_digits(bytes, after).is_some()
    })
}

/// Ends with `:<digits>)`.
fn ends_with_line_paren(text: &str) -> bool {
    let Some(inner) = text.strip_suffix(')') else {
        return false;
    };
    let digits = inner.bytes().rev().take_while(u8::is_ascii_digit).count();
    digits > 0 && inner[..inner.len() - digits].ends_with(':')
}

/// Contains `:<digits>`.
fn has_colon_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len()).any(|i| colon_digits(bytes, i).is_some())
}

/// `.go:<digits>`, then the end of the line or ` +0x...`.
fn is_go_frame(line: &str) -> bool {
    let Some(at) = line.find(".go:") else {
        return false;
    };
    let rest = &line[at + 4..];
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && (rest.len() == digits || rest[digits..].starts_with(" +0x"))
}

/// When `bytes[i]` is `:` followed by at least one digit, the index after
/// those digits.
fn colon_digits(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    let digits = bytes[i + 1..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    (digits > 0).then_some(i + 1 + digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_length_counts_characters_not_bytes() {
        assert_eq!(measure("").prompt_char_len, 0);
        assert_eq!(measure("hello").prompt_char_len, 5);
        assert_eq!(measure("café ✓").prompt_char_len, 6);
        assert_eq!(measure("a\nb").prompt_char_len, 3);
    }

    #[test]
    fn traces_from_common_runtimes_are_detected() {
        let traces = [
            // Python
            "Traceback (most recent call last):\n  File \"app.py\", line 3, in <module>\n    main()\nValueError: boom",
            // Node
            "TypeError: x is undefined\n    at f (/srv/app.js:10:5)\n    at Object.<anonymous> (/srv/app.js:20:1)",
            // Java
            "java.lang.NullPointerException\n\tat com.acme.App.run(App.java:42)\n\tat com.acme.App.main(App.java:7)",
            "Exception in thread \"main\" java.lang.IllegalStateException: bad",
            // Rust
            "thread 'main' panicked at src/main.rs:4:5:\nboom",
            "stack backtrace:\n   0: rust_begin_unwind",
            "   0: app::run\n             at ./src/main.rs:5:9\n   1: app::main\n             at ./src/main.rs:9:5",
            // Go
            "panic: runtime error\n\ngoroutine 1 [running]:\nmain.main()",
            "main.f()\n\t/src/main.go:12 +0x1d\nmain.main()\n\t/src/main.go:20 +0x25",
            // C#
            "System.Exception: bad\n   at Acme.App.Run() in C:\\src\\App.cs:line 12\n   at Acme.App.Main() in C:\\src\\App.cs:line 5",
            // Ruby
            "app.rb:3:in 'boom': bad (RuntimeError)\n\tfrom app.rb:7:in 'run'\n\tfrom app.rb:10:in '<main>'",
        ];
        for trace in traces {
            assert!(has_stacktrace(trace), "missed: {trace}");
        }
    }

    #[test]
    fn prose_and_lone_frame_lookalikes_are_not_traces() {
        let prose = [
            "",
            "please refactor the login form",
            "at the end of the day, it works",
            "I got Error: connection refused when I ran it",
            "meet at 10:30:15 tomorrow",
            "at f (a.js:1:2)",
            "File \"notes.txt\" is missing",
            "from here to there: 3 steps",
            "the goroutine leaks",
            "see main.go:12 for the bug",
        ];
        for text in prose {
            assert!(!has_stacktrace(text), "false positive: {text}");
        }
    }
}
