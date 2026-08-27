//! Network WASM fetch for `--id` / `--wasm-hash`.
//!
//! Rather than reimplement RPC access and network config, this delegates to the
//! `stellar` CLI itself: `stellar contract fetch` already resolves the named
//! network, talks to the RPC, and writes the WASM to stdout. Shelling out keeps
//! the plugin isolated *and* keeps network behavior in lockstep with the CLI
//! that invoked us.

use std::process::Command;

use crate::error::Error;

/// Fetch a contract's WASM by delegating to `stellar contract fetch`.
///
/// Exactly one of `id` / `wasm_hash` is set. `network` is the `--network` name to
/// forward; when `None`, the CLI's configured default network (if any) applies.
pub fn fetch_wasm(
    id: Option<&str>,
    wasm_hash: Option<&str>,
    network: Option<&str>,
) -> Result<Vec<u8>, Error> {
    let mut cmd = Command::new("stellar");
    cmd.args(["contract", "fetch"]);
    if let Some(id) = id {
        cmd.args(["--id", id]);
    }
    if let Some(hash) = wasm_hash {
        cmd.args(["--wasm-hash", hash]);
    }
    if let Some(network) = network {
        cmd.args(["--network", network]);
    }

    // The WASM is written to stdout (binary); progress/warnings go to stderr.
    let output = cmd.output().map_err(Error::StellarInvoke)?;
    if !output.status.success() {
        return Err(Error::FetchFailed {
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}
