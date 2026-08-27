//! The `stellar contract verify` command, as a standalone plugin.
//!
//! Orchestration ported from the CLI's `contract verify` (`run` +
//! `rebuild_and_verify`), scoped to the isolated MVP: input is a local `--wasm`
//! file (network `--id`/`--wasm-hash` fetch is deferred), and the rebuild runs
//! through the `docker` CLI.

use std::path::{Path, PathBuf};

use clap::Parser;
use sha2::{Digest, Sha256};

use crate::container;
use crate::engine::{ContainerArgs, RunArgs};
use crate::error::Error;
use crate::meta::{self, ExtractedMetadata};
use crate::net;
use crate::print::{sanitize, Print};
use crate::source;
use crate::trust::{require_trust, TrustKind};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "stellar-contract-verify",
    about = "Verify that a contract's WASM reproduces from the build metadata it records, per SEP-58.",
    version
)]
pub struct Cmd {
    /// Local WASM file to verify, instead of fetching from the network.
    #[arg(long, conflicts_with_all = ["contract_id", "wasm_hash"])]
    pub wasm: Option<PathBuf>,

    /// Contract id (a `C…` strkey) to fetch the WASM from the network via
    /// `stellar contract fetch`.
    #[arg(long = "id", env = "STELLAR_CONTRACT_ID", conflicts_with = "wasm_hash")]
    pub contract_id: Option<String>,

    /// WASM hash (hex) to fetch the WASM from the network via
    /// `stellar contract fetch`.
    #[arg(long = "wasm-hash")]
    pub wasm_hash: Option<String>,

    /// Named network to fetch from (forwarded to `stellar contract fetch`), e.g.
    /// `testnet`. Only used with `--id` / `--wasm-hash`.
    #[arg(long, short = 'n', env = "STELLAR_NETWORK")]
    pub network: Option<String>,

    /// Local source code file or http(s) URL to use as the source when the WASM's
    /// recorded SEP-58 metadata has only `source_sha256` (no `source_uri`), or to
    /// override the recorded `source_uri`. Accepts http(s) URLs or local file paths.
    #[arg(long)]
    pub source_uri: Option<String>,

    /// Bypass interactive confirmation when the WASM's bldimg is not in the
    /// default trust list, or when the source is provided as an archive (source
    /// archives are never default-trusted).
    #[arg(long)]
    pub trust: bool,

    /// Keep the materialized source and rebuild output instead of deleting them
    /// on exit, and print the path. Useful for debugging a byte mismatch.
    #[arg(long)]
    pub keep: bool,

    /// Suppress progress output; only the final verdict is printed.
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Print the container command and stream the rebuild's output.
    #[arg(long, short, global = true)]
    pub verbose: bool,

    #[command(flatten)]
    pub container_args: ContainerArgs,

    #[command(flatten)]
    pub run_args: RunArgs,
}

impl Cmd {
    pub fn run(&mut self) -> Result<(), Error> {
        let print = Print::new(self.quiet);

        // Adopt the CLI's configured default engine when running standalone
        // without an explicit choice (no-op when launched as a plugin, where the
        // env var is already inherited).
        self.container_args.resolve_default_from_cli();

        let wasm_bytes = self.fetch_wasm(&print)?;
        let meta = meta::extract_metadata(&wasm_bytes)?;

        // Every value below comes from the (untrusted) WASM or user input, so it
        // is `sanitize`d before printing to neutralize embedded terminal escapes.
        print.infoln(format!("Build image: {}", sanitize(&meta.bldimg)));
        // Report the source we'll actually fetch from. When `--source-uri`
        // overrides the recorded value, show the override (and the recorded value
        // it replaces) so the line isn't misleading.
        match (&self.source_uri, &meta.source_uri) {
            (Some(override_uri), Some(recorded)) => {
                print.infoln(format!(
                    "Source URI: {} (overrides recorded {})",
                    sanitize(override_uri),
                    sanitize(recorded)
                ));
            }
            (Some(override_uri), None) => {
                print.infoln(format!("Source URI: {} (override)", sanitize(override_uri)));
            }
            (None, Some(recorded)) => {
                print.infoln(format!("Source URI: {}", sanitize(recorded)));
            }
            (None, None) => {}
        }
        if let Some(v) = &meta.source_sha256 {
            print.infoln(format!("Source SHA-256: {}", sanitize(v)));
        }

        if !meta.bldopts.is_empty() {
            print.infoln(format!("Build options ({}):", meta.bldopts.len()));
            for o in &meta.bldopts {
                print.blankln(format!("  • {}", sanitize(o)));
            }
        }

        // Catch the no-retrieval-channel case before any trust prompts so a
        // doomed run errors immediately instead of asking the user to trust an
        // image we won't end up using.
        if self.effective_source_uri(&meta).is_none() {
            return Err(Error::SourceUriRequired);
        }

        // bldimg trust check is always required.
        require_trust(self.trust, TrustKind::Bldimg, &meta.bldimg, &print)?;

        // Source archive: trust the URL we will actually fetch from (either the
        // value the WASM recorded, or the user's `--source-uri` override).
        if let Some(url) = self.effective_source_uri(&meta) {
            require_trust(self.trust, TrustKind::SourceArchive, &url, &print)?;
        }

        // Materialize the recorded source into a tempdir so the rebuild can
        // bind-mount it. Normally the TempDir cleans up on drop; with `--keep`
        // we persist it (below) so a mismatch can be inspected afterwards.
        let workdir = source::materialize_source(&meta, self.source_uri.as_deref(), &print)?;
        // Register it for interrupt cleanup: on Ctrl-C the process exits without
        // running `TempDir`'s `Drop`, so the handler removes it instead (unless
        // `--keep`).
        crate::cleanup::set_tempdir(workdir.path().to_path_buf(), self.keep);
        print.checkln(format!(
            "Source materialized at {}",
            workdir.path().display()
        ));

        let result = self.rebuild_and_verify(workdir.path(), &meta, &wasm_bytes, &print);

        // Persist the build tree when asked — regardless of the outcome, so a
        // byte mismatch (or a rebuild error) can be debugged. Otherwise it cleans
        // up on drop here.
        if self.keep {
            let kept = workdir.keep();
            Print::new(false).infoln(format!("Kept build directory at {}", kept.display()));
        }

        result
    }

    /// Rebuild the contract in the recorded `bldimg` and compare the freshly
    /// built WASM against the original.
    fn rebuild_and_verify(
        &self,
        workdir: &Path,
        meta: &ExtractedMetadata,
        wasm_bytes: &[u8],
        print: &Print,
    ) -> Result<(), Error> {
        self.container_args.warn_if_host_ignored(print);
        container::pull_image(&meta.bldimg, &self.container_args, print)?;

        // `--locked` was only added to `contract build` in cli 25.2.0. The
        // recorded bldimg may be older (and still valid), so probe it before
        // forcing `--locked` — passing an unknown flag would fail the rebuild.
        let supports_locked =
            container::probe_supports_locked(&meta.bldimg, &self.container_args, print);
        let (container_cmd, env) = container::build_container_command(meta, supports_locked);

        // SEP-58 requires the source be wrapped in a single top-level directory,
        // so the build's working tree is that wrapper dir under `workdir`.
        let source_root = source::locate_extracted_source_root(workdir)?;

        // Snapshot any WASM artifacts already present before the rebuild; a
        // conformant source archive ships none, so excluding these stops a
        // planted pre-built binary from spoofing a match.
        let preexisting = container::snapshot_preexisting_wasms(&source_root);
        if !preexisting.is_empty() {
            print.warnln(format!(
                "Ignoring {} pre-existing WASM artifact(s) in the source; only freshly rebuilt output is trusted",
                preexisting.len()
            ));
        }

        container::run_in_container(
            &meta.bldimg,
            &source_root,
            &container_cmd,
            &env,
            &self.container_args,
            &self.run_args,
            print,
            self.verbose,
        )?;

        let rebuilt_path = container::find_rebuilt_wasm(&source_root, meta, &preexisting)?;
        let rebuilt = std::fs::read(&rebuilt_path).map_err(|e| Error::ReadRebuilt {
            path: rebuilt_path.clone(),
            source: e,
        })?;
        if self.keep {
            print.infoln(format!("Rebuilt WASM at {}", rebuilt_path.display()));
        }

        // Compare. The final result is always shown, even under `--quiet`, via a
        // dedicated Print that ignores the quiet flag.
        let result_print = Print::new(false);
        let original_hash = format!("{:x}", Sha256::digest(wasm_bytes));
        let rebuilt_hash = format!("{:x}", Sha256::digest(&rebuilt));
        if original_hash == rebuilt_hash && wasm_bytes.len() == rebuilt.len() {
            result_print.checkln(format!(
                "Verified: {} bytes, sha256={original_hash}",
                wasm_bytes.len()
            ));
            Ok(())
        } else {
            Err(Error::VerificationMismatch {
                original_hash,
                original_size: wasm_bytes.len(),
                rebuilt_hash,
                rebuilt_size: rebuilt.len(),
            })
        }
    }

    /// Obtain the WASM to verify: read a local `--wasm` file, or fetch from the
    /// network by `--id` / `--wasm-hash` (delegated to `stellar contract fetch`).
    /// Clap keeps these mutually exclusive, so at most one is set.
    fn fetch_wasm(&self, print: &Print) -> Result<Vec<u8>, Error> {
        if let Some(path) = &self.wasm {
            return std::fs::read(path).map_err(|e| Error::ReadWasm(path.clone(), e));
        }
        if self.contract_id.is_some() || self.wasm_hash.is_some() {
            print.infoln("Fetching WASM from the network via `stellar contract fetch`");
            return net::fetch_wasm(
                self.contract_id.as_deref(),
                self.wasm_hash.as_deref(),
                self.network.as_deref(),
            );
        }
        Err(Error::MissingInput)
    }

    /// The source archive URL we'll actually retrieve from: the cli override if
    /// set, otherwise the value recorded in the WASM.
    fn effective_source_uri(&self, meta: &ExtractedMetadata) -> Option<String> {
        self.source_uri.clone().or_else(|| meta.source_uri.clone())
    }
}
