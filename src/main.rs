//! `stellar-contract-verify` — a Stellar CLI plugin.
//!
//! Installed on PATH, the stellar CLI's plugin fallback execs this binary when
//! you run `stellar contract verify …`. It can also be run directly.

use clap::Parser;

mod container;
mod error;
mod meta;
mod net;
mod print;
mod source;
mod trust;
mod verify;

fn main() {
    let cmd = verify::Cmd::parse();
    if let Err(e) = cmd.run() {
        // The error already reads as a full sentence; the ❌ marks it as the
        // failure line and mirrors the CLI's error styling.
        eprintln!("❌ {e}");
        std::process::exit(1);
    }
}
