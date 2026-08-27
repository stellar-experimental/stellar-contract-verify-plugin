//! Source-archive retrieval and extraction.
//!
//! Reproduces the parts of the CLI's `source_archive` module that `verify` uses
//! (`ArchiveFormat`, `extract_into_hardened_tempdir`, unpack helpers) plus
//! verify's own `materialize_source` and friends. Retrieval is synchronous
//! (`reqwest::blocking`); the upstream is async but the flow is otherwise
//! identical.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use url::Url;

use crate::error::Error;
use crate::meta::ExtractedMetadata;
use crate::print::Print;

/// Prefix for the materialized-source tempdir. `tempfile` appends a random
/// component, and the build container reuses that whole dir name as its own name
/// suffix so a running container maps back to its source tree at a glance.
pub const SOURCE_TEMPDIR_PREFIX: &str = "verify-src-";

/// Container formats we can extract a source tree from. This only concerns how
/// the tree is packed for transport; the tree itself is always wrapped in a
/// single top-level directory (SEP-58), which callers check after extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// Gzipped tarball — what `build --verifiable` produces.
    TarGz,
    /// Zip archive.
    Zip,
}

/// Recognized archive extensions and the format each maps to, matched
/// case-insensitively as a suffix of the archive's filename. Single source of
/// truth for both format detection and the "accepted formats" error text.
const ARCHIVE_EXTENSIONS: &[(&str, ArchiveFormat)] = &[
    (".tar.gz", ArchiveFormat::TarGz),
    (".tgz", ArchiveFormat::TarGz),
    (".zip", ArchiveFormat::Zip),
];

impl ArchiveFormat {
    /// The format named by `filename`'s extension, or `None` if unrecognized.
    pub fn from_filename(filename: &str) -> Option<Self> {
        let lower = filename.to_ascii_lowercase();
        ARCHIVE_EXTENSIONS
            .iter()
            .find(|(ext, _)| lower.ends_with(ext))
            .map(|(_, format)| *format)
    }

    /// Comma-separated list of accepted extensions, for error messages.
    pub fn recognized_extensions() -> String {
        ARCHIVE_EXTENSIONS
            .iter()
            .map(|(ext, _)| *ext)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The stellar-cli local data directory, obtained from the CLI itself via
/// `stellar cache path` rather than reimplementing its resolution (which honors
/// `STELLAR_DATA_HOME`/`XDG_DATA_HOME` and the platform default). Extractions go
/// under its `tmp/`, NOT the OS temp dir: on macOS `$TMPDIR` lives under
/// /var/folders, which container VMs (Docker Desktop, Colima, …) don't share by
/// default, so a bind mount of it would be empty inside the container. The data
/// dir lives under the user's home, which is shared.
fn data_local_dir() -> Result<PathBuf, Error> {
    let output = Command::new("stellar")
        .args(["cache", "path"])
        .output()
        .map_err(Error::StellarInvoke)?;
    if !output.status.success() {
        return Err(Error::DataDir);
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(Error::DataDir);
    }
    Ok(PathBuf::from(path))
}

/// Decompress gzip and unpack the tar into `dest`. Entries are `source/…`, so
/// they land at `<dest>/source/…`.
fn unpack_targz(bytes: &[u8], dest: &Path) -> Result<(), Error> {
    let dec = flate2::read::GzDecoder::new(bytes);
    tar::Archive::new(dec)
        .unpack(dest)
        .map_err(Error::ArchiveExtract)
}

/// Unpack a zip archive into `dest`. `ZipArchive::extract` sanitizes each
/// entry's path (dropping anything that would escape `dest`), so a hostile
/// archive can't write outside the tempdir.
fn unpack_zip(bytes: &[u8], dest: &Path) -> Result<(), Error> {
    zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .and_then(|mut archive| archive.extract(dest))
        .map_err(Error::ZipExtract)
}

/// Recursively set every dir to `0700` and every file to `0600` under `root`,
/// skipping symlinks. No-op on non-Unix. Mirrors the CLI's `enforce_hardened_tree`.
fn enforce_hardened_tree(root: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut stack = vec![root.to_path_buf()];
        while let Some(p) = stack.pop() {
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let current = meta.permissions().mode() & 0o777;
            if meta.is_dir() {
                if current != 0o700 {
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700))?;
                }
                if let Ok(entries) = std::fs::read_dir(&p) {
                    for entry in entries.filter_map(Result::ok) {
                        stack.push(entry.path());
                    }
                }
            } else if current != 0o600 {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = root;
    }
    Ok(())
}

/// Create a fresh temp directory under the data dir's `tmp/`, unpack the source
/// archive `bytes` (in the given `format`) into it, harden its permissions, and
/// return the guard (the tree lives at its `path()`).
pub fn extract_into_hardened_tempdir(
    bytes: &[u8],
    prefix: &str,
    format: ArchiveFormat,
) -> Result<tempfile::TempDir, Error> {
    let base = data_local_dir()?.join("tmp");
    std::fs::create_dir_all(&base).map_err(Error::ArchiveExtract)?;
    let tmp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&base)
        .map_err(Error::ArchiveExtract)?;
    match format {
        ArchiveFormat::TarGz => unpack_targz(bytes, tmp.path())?,
        ArchiveFormat::Zip => unpack_zip(bytes, tmp.path())?,
    }
    enforce_hardened_tree(tmp.path()).map_err(Error::ArchiveExtract)?;
    Ok(tmp)
}

/// Materialize the recorded source tree into a fresh, permission-hardened
/// tempdir and return the guard. The retrieval channel is the cli's
/// `--source-uri` flag (when set) or the WASM's recorded `source_uri`; either
/// may be an http(s) URL or a local file path. When the bytes are present, the
/// optional `source_sha256` is checked before extraction.
pub fn materialize_source(
    meta: &ExtractedMetadata,
    source_uri_override: Option<&str>,
    print: &Print,
) -> Result<tempfile::TempDir, Error> {
    let resolved_source = source_uri_override
        .map(str::to_string)
        .or_else(|| meta.source_uri.clone());
    let Some(source) = resolved_source else {
        // No source_uri anywhere — only source_sha256 is set.
        return Err(Error::SourceUriRequired);
    };

    let format = resolve_source_format(&source)?;

    print.infoln(format!("Fetching source code from {source}"));
    let bytes = fetch_source_bytes(&source)?;
    if let Some(expected) = &meta.source_sha256 {
        verify_source_sha256(&bytes, expected)?;
        print.checkln("Source SHA-256 matches");
    }
    extract_into_hardened_tempdir(&bytes, SOURCE_TEMPDIR_PREFIX, format)
}

/// The last path segment of `source`, whether it's a URL or a local path. Try
/// parsing as a URL first (so query strings and fragments are dropped); if that
/// fails, `source` is a local path, so fall back to `Path::file_name`.
fn source_basename(source: &str) -> String {
    if let Ok(url) = Url::parse(source) {
        return url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or_default()
            .to_string();
    }
    Path::new(source)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Determine the archive format from a `--source-uri` (or recorded `source_uri`)
/// by its basename, rejecting sources whose extension we don't recognize before
/// we bother fetching them, naming the formats we accept.
fn resolve_source_format(source: &str) -> Result<ArchiveFormat, Error> {
    let basename = source_basename(source);
    ArchiveFormat::from_filename(&basename).ok_or_else(|| Error::UnsupportedSourceFormat {
        uri: source.to_string(),
        formats: ArchiveFormat::recognized_extensions(),
    })
}

/// Retrieve the source archive bytes. `source` is either an `http(s)://` URL or
/// a local file path. The split is by prefix, not by attempting both — keeps
/// behavior predictable.
fn fetch_source_bytes(source: &str) -> Result<Vec<u8>, Error> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let resp = reqwest::blocking::get(source).map_err(|e| Error::SourceDownload {
            url: source.to_string(),
            source: e,
        })?;
        let bytes = resp
            .error_for_status()
            .map_err(|e| Error::SourceDownload {
                url: source.to_string(),
                source: e,
            })?
            .bytes()
            .map_err(|e| Error::SourceDownload {
                url: source.to_string(),
                source: e,
            })?;
        Ok(bytes.to_vec())
    } else {
        std::fs::read(source).map_err(|e| Error::SourceRead {
            path: PathBuf::from(source),
            source: e,
        })
    }
}

fn verify_source_sha256(bytes: &[u8], expected: &str) -> Result<(), Error> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(Error::SourceHashMismatch {
            expected: expected.to_string(),
            actual,
        })
    }
}

/// SEP-58 requires the source archive wrap everything in a single top-level
/// directory (the cli names it `source/`, but the spec leaves the name open),
/// so after extraction the build tree is that lone directory under `workdir`.
/// Return it, erroring if the archive doesn't have exactly one top-level dir.
pub fn locate_extracted_source_root(workdir: &Path) -> Result<PathBuf, Error> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(workdir)
        .map_err(|source| Error::SourceExtract {
            path: workdir.to_path_buf(),
            source,
        })?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();

    match dirs.len() {
        1 => Ok(dirs.remove(0)),
        count => Err(Error::SourceArchiveLayout {
            path: workdir.to_path_buf(),
            count,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_format_from_filename() {
        assert_eq!(
            ArchiveFormat::from_filename("src.tar.gz"),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(
            ArchiveFormat::from_filename("SRC.TGZ"),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(
            ArchiveFormat::from_filename("src.zip"),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(ArchiveFormat::from_filename("src.rar"), None);
        assert_eq!(ArchiveFormat::from_filename("src"), None);
        assert_eq!(
            ArchiveFormat::recognized_extensions(),
            ".tar.gz, .tgz, .zip"
        );
    }

    #[test]
    fn verify_source_sha256_matches() {
        let bytes = b"hello, sep-58";
        let digest = format!("{:x}", Sha256::digest(bytes));
        verify_source_sha256(bytes, &digest).unwrap();
        verify_source_sha256(bytes, &digest.to_ascii_uppercase()).unwrap();
    }

    #[test]
    fn verify_source_sha256_mismatch_errors() {
        let bytes = b"hello, sep-58";
        let bogus = "0".repeat(64);
        let err = verify_source_sha256(bytes, &bogus).unwrap_err();
        assert!(matches!(err, Error::SourceHashMismatch { .. }));
    }

    #[test]
    fn materialize_source_errors_when_only_source_sha256() {
        let meta = ExtractedMetadata {
            bldimg: format!("docker.io/stellar/stellar-cli@sha256:{}", "a".repeat(64)),
            source_uri: None,
            source_sha256: Some("f".repeat(64)),
            bldargs: Vec::new(),
            bldopts: Vec::new(),
            meta_entries: Vec::new(),
        };
        let print = Print::new(true);
        let err = materialize_source(&meta, None, &print).unwrap_err();
        assert!(matches!(err, Error::SourceUriRequired));
    }

    #[test]
    fn source_basename_strips_url_query_and_fragment() {
        assert_eq!(
            source_basename("https://example.com/path/src.tar.gz?token=abc#frag"),
            "src.tar.gz"
        );
        assert_eq!(source_basename("https://example.com/a/b/x.tgz"), "x.tgz");
    }

    #[test]
    fn source_basename_handles_local_paths() {
        assert_eq!(source_basename("/tmp/foo/src.tar.gz"), "src.tar.gz");
        assert_eq!(source_basename("./relative/src.tgz"), "src.tgz");
        assert_eq!(source_basename("src.tar.gz"), "src.tar.gz");
    }

    #[test]
    fn resolve_source_format_accepts_recognized_extensions() {
        assert_eq!(
            resolve_source_format("https://example.com/src.tar.gz").unwrap(),
            ArchiveFormat::TarGz
        );
        assert_eq!(
            resolve_source_format("/tmp/src.tgz").unwrap(),
            ArchiveFormat::TarGz
        );
        assert_eq!(
            resolve_source_format("https://example.com/src.zip?token=abc").unwrap(),
            ArchiveFormat::Zip
        );
        assert_eq!(
            resolve_source_format("SRC.TAR.GZ").unwrap(),
            ArchiveFormat::TarGz
        );
    }

    #[test]
    fn resolve_source_format_rejects_unknown_formats() {
        for source in [
            "https://example.com/src.rar",
            "/tmp/src.7z",
            "src",
            "src.gz",
        ] {
            let err = resolve_source_format(source).unwrap_err();
            assert!(matches!(err, Error::UnsupportedSourceFormat { .. }));
        }
    }
}
