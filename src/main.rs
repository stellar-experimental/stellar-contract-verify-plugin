//! `stellar-contract-verify` — a Stellar CLI plugin.
//!
//! Installed on PATH, the stellar CLI's plugin fallback execs this binary when
//! you run `stellar contract verify …`. It can also be run directly.

use clap::Parser;

mod container;
mod engine;
mod error;
mod meta;
mod net;
mod print;
mod source;
mod trust;
mod verify;

fn main() {
    let mut cmd = verify::Cmd::parse();
    if let Err(e) = cmd.run() {
        // The error already reads as a full sentence; the ❌ marks it as the
        // failure line and mirrors the CLI's error styling. Errors can embed
        // untrusted values (a WASM's metadata, a rebuild command), so sanitize
        // per line as a safety net — neutralizing terminal escapes while keeping
        // multi-line layouts (e.g. the verification-mismatch report) intact.
        eprintln!("❌ {}", print::sanitize_lines(&e.to_string()));
        std::process::exit(1);
    }
}
