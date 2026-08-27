//! Minimal stderr printer, a lean stand-in for `soroban_cli::print::Print`.
//!
//! Messages go to stderr (so stdout stays clean) and are suppressed under
//! `--quiet`, except where a caller deliberately constructs `Print::new(false)`
//! for output that must always be shown (the final verdict, trust prompts).

use std::fmt::Display;
use std::io::Write;

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
