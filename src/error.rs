//! Unified error type for the plugin.
//!
//! The upstream implementation splits these across `verify::Error`,
//! `source_archive::Error`, `container::shared::Error`, and `verifiable::Error`;
//! since the plugin folds those modules together, one enum covers them all.

use std::path::PathBuf;

use crate::trust::TrustKind;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("must pass exactly one of --wasm, --id, or --wasm-hash")]
    MissingInput,

    #[error("could not run `stellar` (is the Stellar CLI installed and on PATH?): {0}")]
    StellarInvoke(std::io::Error),

    #[error("`stellar contract fetch` failed: {stderr}")]
    FetchFailed { stderr: String },

    #[error("reading wasm {0}: {1}")]
    ReadWasm(PathBuf, std::io::Error),

    #[error("parsing the WASM's contract metadata: {0}")]
    MetaParse(String),

    #[error("the WASM has no contractmetav0 custom section")]
    NoMeta,

    #[error("the WASM's contractmetav0 does not record a `bldimg` entry; cannot verify")]
    MissingBldimg,

    #[error("the WASM's contractmetav0 records more than one `{field}` entry; refusing to verify (which value applies is ambiguous)")]
    DuplicateMeta { field: &'static str },

    #[error("the WASM's contractmetav0 does not record a `source_sha256` entry; cannot verify")]
    MissingSourceSha256,

    #[error(
        "the WASM's `{field}` value {value:?} does not match the SEP-58 format regex `{regex}`"
    )]
    MetaFormat {
        field: &'static str,
        value: String,
        regex: &'static str,
    },

    #[error("{kind} {value:?} is not in the default trust list, and stdin is not a terminal so we can't ask. Re-run with --trust to proceed.")]
    TrustRequired { kind: TrustKind, value: String },

    #[error("user declined to trust the {kind}; aborting")]
    TrustDeclined { kind: TrustKind },

    #[error("reading stdin: {0}")]
    Stdin(std::io::Error),

    #[error("source {uri:?} has an unsupported format; accepted formats are {formats}")]
    UnsupportedSourceFormat { uri: String, formats: String },

    #[error("the WASM records only `source_sha256` (no `source_uri`). Pass `--source-uri URL_OR_PATH` to provide retrieval.")]
    SourceUriRequired,

    #[error("downloading {url}: {source}")]
    SourceDownload { url: String, source: reqwest::Error },

    #[error("reading local source code {path}: {source}")]
    SourceRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("source code sha256 mismatch: expected {expected}, got {actual}")]
    SourceHashMismatch { expected: String, actual: String },

    #[error("reading extracted source at {path}: {source}")]
    SourceExtract {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("source archive at {path} does not contain exactly one top-level directory (found {count}); SEP-58 requires the source be wrapped in a single directory")]
    SourceArchiveLayout { path: PathBuf, count: usize },

    #[error("could not extract source archive: {0}")]
    ArchiveExtract(std::io::Error),

    #[error("could not extract source archive: {0}")]
    ZipExtract(zip::result::ZipError),

    #[error("could not locate the stellar-cli data directory")]
    DataDir,

    #[error("could not run `docker` (is it installed and on PATH?): {0}")]
    DockerInvoke(std::io::Error),

    #[error("failed to pull image {image}")]
    DockerPull { image: String },

    #[error("the verifiable build failed (container exited with status {status}).\n  reproduce with: {command}")]
    ContainerExit { status: i64, command: String },

    #[error("could not find a rebuilt WASM under {target}")]
    NoRebuiltWasm { target: PathBuf },

    #[error("multiple rebuilt WASMs under {target}; pass --package=... in the bldopt entries to disambiguate. Found: {found}")]
    AmbiguousRebuiltWasm { target: PathBuf, found: String },

    #[error("reading rebuilt wasm {path}: {source}")]
    ReadRebuilt {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("verification failed: rebuilt bytes do not match the original.\n  original: {original_size} bytes, sha256={original_hash}\n  rebuilt:  {rebuilt_size} bytes, sha256={rebuilt_hash}")]
    VerificationMismatch {
        original_hash: String,
        original_size: usize,
        rebuilt_hash: String,
        rebuilt_size: usize,
    },
}
