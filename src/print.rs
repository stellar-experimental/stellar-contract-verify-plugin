//! Minimal stderr printer, a lean stand-in for `soroban_cli::print::Print`.
//!
//! Messages go to stderr (so stdout stays clean) and are suppressed under
//! `--quiet`, except where a caller deliberately constructs `Print::new(false)`
//! for output that must always be shown (the final verdict, trust prompts).

use std::fmt::{Display, Write as _};
use std::io::Write;

/// Escape control characters (ESC, CR, …) as `\xNN` so attacker-controlled
/// values — a WASM's `bldimg`/`source_uri`/`bldopt`/replayed metadata — can't
/// inject terminal escape sequences (color, cursor moves, line rewrites) when
/// printed. Mirrors the CLI's `escape_control_characters`. Escapes newlines too,
/// so a value can't forge extra output lines; apply it to individual values, not
/// whole (possibly multi-line) messages.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            let mut buf = [0u8; 4];
            for &b in c.encode_utf8(&mut buf).as_bytes() {
                let _ = write!(out, "\\x{b:02x}");
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Per-line [`sanitize`]: neutralizes control characters within each line while
/// preserving newlines, keeping multi-line layouts intact. A final safety net
/// for messages (e.g. error text) that embed values not sanitized at the source.
pub fn sanitize_lines(s: &str) -> String {
    s.split('\n').map(sanitize).collect::<Vec<_>>().join("\n")
}

#[derive(Clone, Copy)]
pub struct Print {
    pub quiet: bool,
}

impl Print {
    pub fn new(quiet: bool) -> Print {
        Print { quiet }
    }

    fn emit(&self, icon: &str, message: impl Display) {
        if !self.quiet {
            eprintln!("{icon} {message}");
        }
    }

    pub fn infoln(&self, message: impl Display) {
        self.emit("ℹ️", message);
    }

    pub fn checkln(&self, message: impl Display) {
        self.emit("✅", message);
    }

    pub fn warnln(&self, message: impl Display) {
        self.emit("⚠️", message);
    }

    /// Indented continuation line (no leading icon), matching the CLI's `blankln`.
    pub fn blankln(&self, message: impl Display) {
        self.emit("  ", message);
    }

    /// A prompt written without a trailing newline; the caller flushes stderr and
    /// reads the answer from stdin.
    pub fn question(&self, message: impl Display) {
        if !self.quiet {
            eprint!("❓ {message}");
            let _ = std::io::stderr().flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_escapes_control_chars_including_esc_and_newline() {
        // An ANSI CSI sequence and a newline must not survive verbatim.
        assert_eq!(sanitize("a\x1b[31mred\nb"), r"a\x1b[31mred\x0ab");
        assert_eq!(sanitize("\r\t"), r"\x0d\x09");
    }

    #[test]
    fn sanitize_passes_through_printable_and_unicode() {
        assert_eq!(
            sanitize("docker.io/stellar/stellar-cli"),
            "docker.io/stellar/stellar-cli"
        );
        assert_eq!(sanitize("café • 世界"), "café • 世界");
    }

    #[test]
    fn sanitize_lines_preserves_newlines_but_neutralizes_escapes() {
        assert_eq!(
            sanitize_lines("line1\n\x1b[2Jline2"),
            "line1\n\\x1b[2Jline2"
        );
    }
}
