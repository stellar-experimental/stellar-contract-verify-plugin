//! SEP-58 build-metadata extraction from a contract WASM.
//!
//! Ported verbatim from the CLI's `contract verify` (`extract_metadata` and its
//! helpers). Reads the WASM's `contractmetav0` custom sections, tells the
//! CLI-injected section from the SDK/compile-emitted one, and pulls out the
//! fields that drive the rebuild while preserving the exact entry order to
//! replay.

use std::io::Cursor;

use regex::Regex;
use stellar_xdr::{Limited, Limits, ReadXdr, ScMetaEntry, ScMetaV0};

use crate::error::Error;

pub fn bldimg_regex() -> Regex {
    Regex::new(BLDIMG_REGEX_STR).unwrap()
}

pub fn source_sha256_regex() -> Regex {
    Regex::new(SOURCE_SHA256_REGEX_STR).unwrap()
}

pub fn source_uri_regex() -> Regex {
    Regex::new(SOURCE_URL_REGEX_STR).unwrap()
}

// These mirror the regex strings used by the CLI's verifiable build. They both
// drive matching and render back to the user in `Error::MetaFormat`.
const BLDIMG_REGEX_STR: &str =
    r"^(?:localhost(?::\d+)?|[^\s@/]*[.:][^\s@/]*)/[^\s@]+@sha256:[0-9a-f]{64}$";
const SOURCE_URL_REGEX_STR: &str = r"^[a-zA-Z][a-zA-Z0-9+.-]*:\S+$";
const SOURCE_SHA256_REGEX_STR: &str = r"^[0-9a-f]{64}$";

/// Meta keys the rebuild regenerates on its own, so verify must never replay
/// them — re-passing one would write it twice and break byte-equality. `cliver`
/// is re-injected by the container's CLI; `rsver`/`rssdkver` are re-embedded by
/// the SDK on recompile. The section split in `extract_metadata` already keeps
/// the SDK's own section out; this filter is applied to the chosen section as a
/// final guard (chiefly for a degenerate single-section WASM). Source-embedded
/// keys with arbitrary names (e.g. a `contractmeta!` `Description`) are handled
/// by the section split, which no fixed list could enumerate.
const REGENERATED_META_KEYS: &[&str] = &["cliver", "rsver", "rssdkver"];

/// The `cliver` entry the CLI stamps into the section it injects; its presence
/// marks that section as the CLI-injected one (see `extract_metadata`).
const CLIVER_KEY: &str = "cliver";

/// Metadata read from a contract's `contractmetav0` custom sections (SEP-46).
///
/// Verify reproduces the section by *replaying* what the WASM records rather
/// than reconstructing it from `build`'s ordering rules: `meta_entries` holds
/// the CLI-injected entries, in the exact order the WASM records them, so the
/// rebuild's metadata matches byte-for-byte no matter how (or by what tool) the
/// original was produced. The entries the rebuild regenerates itself — the
/// SDK/compile-emitted section (`rsver`, `rssdkver`, and any `contractmeta!`
/// keys such as `Description`) and the CLI's own `cliver` — are excluded, so
/// they aren't written twice.
///
/// The typed fields (`bldimg`, `source_uri`, `source_sha256`, `bldopts`) are
/// pulled out of the same entries only to *drive* the rebuild — pick the image,
/// trust-check, fetch the source, and derive the forwarded build flags. They are
/// not re-added to the `--meta` list; the replay of `meta_entries` covers them.
#[derive(Debug, Clone)]
pub struct ExtractedMetadata {
    pub bldimg: String,
    pub source_uri: Option<String>,
    pub source_sha256: Option<String>,
    pub bldopts: Vec<String>,
    pub meta_entries: Vec<(String, String)>,
}

/// Read the WASM's `contractmetav0` custom sections *separately*, preserving
/// both the per-section grouping and the entry order within each. Keeping them
/// apart is what lets verify tell the SDK/compile-emitted metadata (its own
/// section) from the CLI-injected metadata (a separate section appended by
/// `inject_meta`), so it replays only the latter. SEP-46 permits multiple
/// same-named sections and fixes their concatenation order, so this grouping is
/// well-defined.
fn read_meta_sections(wasm: &[u8]) -> Result<Vec<Vec<(String, String)>>, Error> {
    let mut sections = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| Error::MetaParse(e.to_string()))?;
        if let wasmparser::Payload::CustomSection(reader) = payload {
            if reader.name() == "contractmetav0" {
                sections.push(parse_meta_entries(reader.data())?);
            }
        }
    }
    Ok(sections)
}

/// Decode one `contractmetav0` section's XDR into `(key, value)` pairs, in order.
fn parse_meta_entries(data: &[u8]) -> Result<Vec<(String, String)>, Error> {
    let mut read = Limited::new(Cursor::new(data), Limits::none());
    ScMetaEntry::read_xdr_iter(&mut read)
        .map(|entry| {
            entry.map(|ScMetaEntry::ScMetaV0(ScMetaV0 { key, val })| {
                (key.to_string(), val.to_string())
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::MetaParse(e.to_string()))
}

/// Read the WASM's contract metadata and split out what verify must replay from
/// what the rebuild regenerates on its own.
///
/// The rebuild re-creates the SDK/compile-emitted metadata (`rsver`, `rssdkver`,
/// and any `contractmeta!` keys) by recompiling the source, and the container's
/// CLI re-injects `cliver`. Replaying any of those as `--meta` would write them
/// twice and break byte-equality. `inject_meta` puts `cliver` plus the user's
/// `--meta` into its own `contractmetav0` section, so the section containing
/// `cliver` *is* the CLI-injected set — everything verify must replay, and
/// nothing the rebuild produces for free. We therefore replay that section
/// (minus `cliver`) and ignore the rest.
///
/// Fallback: a WASM with no `cliver` (a pre-v23.2.0 CLI never wrote one, and a
/// WASM may be hand-authored per SEP-46) has no marked section, so we take the
/// last non-empty section instead — `inject_meta` always appends after the
/// compile-emitted sections, so the CLI section is always last.
///
/// Errors when `bldimg` or `source_sha256` is absent, since neither has a
/// sensible default; `source_uri` is optional.
pub fn extract_metadata(wasm: &[u8]) -> Result<ExtractedMetadata, Error> {
    let sections = read_meta_sections(wasm)?;
    if sections.iter().all(Vec::is_empty) {
        return Err(Error::NoMeta);
    }

    // Locate the CLI-injected section: the one carrying `cliver`, or — when no
    // section is marked — the last non-empty one, since `inject_meta` always
    // appends after the compile-emitted sections (the linker merges every
    // `#[link_section = "contractmetav0"]` static — `contractmeta!` entries plus
    // the SDK's `rsver`/`rssdkver` — into a single earlier section). Replay it,
    // dropping the keys the rebuild regenerates itself (`REGENERATED_META_KEYS`):
    // a well-formed CLI section holds none of them, but this guards a degenerate
    // single-section WASM where build fields sit alongside `rsver`/`rssdkver`.
    let cli_section = sections
        .iter()
        .position(|s| s.iter().any(|(k, _)| k == CLIVER_KEY))
        .or_else(|| sections.iter().rposition(|s| !s.is_empty()))
        .expect("a non-empty section exists: the all-empty case is rejected as NoMeta above");
    let meta_entries: Vec<(String, String)> = sections[cli_section]
        .iter()
        .filter(|(k, _)| !REGENERATED_META_KEYS.contains(&k.as_str()))
        .cloned()
        .collect();

    let mut bldimg: Option<String> = None;
    let mut source_uri: Option<String> = None;
    let mut source_sha256: Option<String> = None;
    let mut bldopts: Vec<String> = Vec::new();

    // Each of these fields must appear at most once. Reject duplicates rather
    // than silently taking the last: two `bldimg` entries (say a benign one to
    // fool inspection and a second the cli would actually trust and rebuild in)
    // would be a verification-bypass vector, and the same ambiguity applies to
    // the `source_uri`/`source_sha256` that pin what gets rebuilt.
    let set_once =
        |slot: &mut Option<String>, field: &'static str, v: String| -> Result<(), Error> {
            if slot.is_some() {
                return Err(Error::DuplicateMeta { field });
            }
            *slot = Some(v);
            Ok(())
        };

    // The typed fields are pulled out of the very entries we replay, so the
    // rebuild is driven by exactly the metadata that gets re-recorded.
    for (k, v) in &meta_entries {
        match k.as_str() {
            "bldimg" => set_once(&mut bldimg, "bldimg", v.clone())?,
            "source_uri" => set_once(&mut source_uri, "source_uri", v.clone())?,
            "source_sha256" => set_once(&mut source_sha256, "source_sha256", v.clone())?,
            "bldopt" => bldopts.push(v.clone()),
            _ => {} // user meta: carried in meta_entries for replay
        }
    }

    let bldimg = bldimg.ok_or(Error::MissingBldimg)?;
    if !bldimg_regex().is_match(&bldimg) {
        return Err(Error::MetaFormat {
            field: "bldimg",
            value: bldimg,
            regex: BLDIMG_REGEX_STR,
        });
    }

    if let Some(v) = &source_uri {
        if !source_uri_regex().is_match(v) {
            return Err(Error::MetaFormat {
                field: "source_uri",
                value: v.clone(),
                regex: SOURCE_URL_REGEX_STR,
            });
        }
    }
    if let Some(v) = &source_sha256 {
        if !source_sha256_regex().is_match(v) {
            return Err(Error::MetaFormat {
                field: "source_sha256",
                value: v.clone(),
                regex: SOURCE_SHA256_REGEX_STR,
            });
        }
    }

    if source_sha256.is_none() {
        return Err(Error::MissingSourceSha256);
    }

    Ok(ExtractedMetadata {
        bldimg,
        source_uri,
        source_sha256,
        bldopts,
        meta_entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use stellar_xdr::{Limited, Limits, ScMetaEntry, ScMetaV0, WriteXdr};

    fn make_wasm_with_meta(entries: &[(&str, &str)]) -> Vec<u8> {
        make_wasm_with_sections(&[entries])
    }

    /// Build a WASM with one `contractmetav0` custom section per slice, in order
    /// — mirroring how the SDK/compile step and the CLI's `inject_meta` each
    /// append their own section.
    fn make_wasm_with_sections(sections: &[&[(&str, &str)]]) -> Vec<u8> {
        let mut wasm = empty_wasm_module();
        for entries in sections {
            let xdr = encode_meta(entries);
            wasm_gen::write_custom_section(&mut wasm, "contractmetav0", &xdr);
        }
        wasm
    }

    fn empty_wasm_module() -> Vec<u8> {
        // Minimal valid WASM: magic + version, no sections.
        vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]
    }

    fn encode_meta(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut writer = Limited::new(Cursor::new(&mut buf), Limits::none());
        for (k, v) in entries {
            ScMetaEntry::ScMetaV0(ScMetaV0 {
                key: (*k).to_string().try_into().unwrap(),
                val: (*v).to_string().try_into().unwrap(),
            })
            .write_xdr(&mut writer)
            .unwrap();
        }
        buf
    }

    fn good_bldimg() -> String {
        format!("docker.io/stellar/stellar-cli@sha256:{}", "a".repeat(64))
    }

    #[test]
    fn extract_metadata_happy_path_source_pair() {
        let wasm = make_wasm_with_meta(&[
            ("bldimg", &good_bldimg()),
            ("source_uri", "https://example.com/src.tar.gz"),
            ("source_sha256", &"f".repeat(64)),
            ("bldopt", "--locked"),
        ]);
        let meta = extract_metadata(&wasm).unwrap();
        assert_eq!(
            meta.source_uri.as_deref(),
            Some("https://example.com/src.tar.gz")
        );
        assert_eq!(meta.source_sha256.as_deref(), Some("f".repeat(64).as_str()));
    }

    #[test]
    fn extract_metadata_missing_bldimg_errors() {
        let wasm = make_wasm_with_meta(&[("source_sha256", &"b".repeat(64))]);
        let err = extract_metadata(&wasm).unwrap_err();
        assert!(matches!(err, Error::MissingBldimg));
    }

    #[test]
    fn extract_metadata_duplicate_bldimg_errors() {
        let other = format!("docker.io/attacker/evil@sha256:{}", "b".repeat(64));
        let wasm = make_wasm_with_meta(&[
            ("bldimg", &good_bldimg()),
            ("bldimg", &other),
            ("source_sha256", &"f".repeat(64)),
        ]);
        let err = extract_metadata(&wasm).unwrap_err();
        assert!(matches!(err, Error::DuplicateMeta { field: "bldimg" }));
    }

    #[test]
    fn extract_metadata_duplicate_source_ids_error() {
        for field in ["source_uri", "source_sha256"] {
            let wasm = make_wasm_with_meta(&[
                ("bldimg", &good_bldimg()),
                ("source_sha256", &"f".repeat(64)),
                (field, "https://example.com/a.tar.gz"),
                (field, "https://example.com/b.tar.gz"),
            ]);
            let err = extract_metadata(&wasm).unwrap_err();
            assert!(
                matches!(err, Error::DuplicateMeta { field: f } if f == field),
                "expected DuplicateMeta for {field}, got {err:?}"
            );
        }
    }

    #[test]
    fn extract_metadata_missing_source_id_errors() {
        let wasm = make_wasm_with_meta(&[("bldimg", &good_bldimg())]);
        let err = extract_metadata(&wasm).unwrap_err();
        assert!(matches!(err, Error::MissingSourceSha256));
    }

    #[test]
    fn extract_metadata_bad_bldimg_format_errors() {
        let wasm = make_wasm_with_meta(&[
            ("bldimg", "stellar/stellar-cli@sha256:abc"),
            ("source_sha256", &"b".repeat(64)),
        ]);
        let err = extract_metadata(&wasm).unwrap_err();
        assert!(matches!(
            err,
            Error::MetaFormat {
                field: "bldimg",
                ..
            }
        ));
    }

    #[test]
    fn extract_metadata_bad_source_sha256_format_errors() {
        let wasm = make_wasm_with_meta(&[("bldimg", &good_bldimg()), ("source_sha256", "abc")]);
        let err = extract_metadata(&wasm).unwrap_err();
        assert!(matches!(
            err,
            Error::MetaFormat {
                field: "source_sha256",
                ..
            }
        ));
    }

    #[test]
    fn extract_metadata_replays_only_cli_section() {
        let wasm = make_wasm_with_sections(&[
            &[
                ("Description", "A hello world contract"),
                ("key1", "val1"),
                ("key2", "val2"),
                ("rsver", "1.96.0"),
                ("rssdkver", "26.1.0#abcdef"),
            ],
            &[
                ("cliver", "27.0.0#abcdef"),
                ("bldimg", &good_bldimg()),
                ("source_sha256", &"b".repeat(64)),
                ("bldopt", "--locked"),
                ("home_domain", "fnando.com"),
            ],
        ]);
        let meta = extract_metadata(&wasm).unwrap();
        assert_eq!(meta.bldopts, vec!["--locked".to_string()]);
        assert_eq!(
            meta.meta_entries,
            vec![
                ("bldimg".to_string(), good_bldimg()),
                ("source_sha256".to_string(), "b".repeat(64)),
                ("bldopt".to_string(), "--locked".to_string()),
                ("home_domain".to_string(), "fnando.com".to_string()),
            ]
        );
    }

    #[test]
    fn extract_metadata_fallback_picks_last_section_without_cliver() {
        let wasm = make_wasm_with_sections(&[
            &[
                ("key1", "val1"),
                ("key2", "val2"),
                ("rsver", "1.97.0"),
                ("rssdkver", "22.0.11#abcdef"),
            ],
            &[
                ("bldimg", &good_bldimg()),
                ("source_sha256", &"b".repeat(64)),
                ("bldopt", "--locked"),
            ],
        ]);
        let meta = extract_metadata(&wasm).unwrap();
        assert_eq!(
            meta.meta_entries,
            vec![
                ("bldimg".to_string(), good_bldimg()),
                ("source_sha256".to_string(), "b".repeat(64)),
                ("bldopt".to_string(), "--locked".to_string()),
            ]
        );
    }

    #[test]
    fn extract_metadata_single_section_fallback_drops_regenerated_keys() {
        let wasm = make_wasm_with_meta(&[
            ("bldimg", &good_bldimg()),
            ("source_sha256", &"b".repeat(64)),
            ("rsver", "1.97.0"),
            ("rssdkver", "22.0.11#abcdef"),
            ("home_domain", "fnando.com"),
        ]);
        let meta = extract_metadata(&wasm).unwrap();
        assert_eq!(
            meta.meta_entries,
            vec![
                ("bldimg".to_string(), good_bldimg()),
                ("source_sha256".to_string(), "b".repeat(64)),
                ("home_domain".to_string(), "fnando.com".to_string()),
            ]
        );
    }

    #[test]
    fn extract_metadata_ignores_duplicate_key_in_source_embedded_section() {
        let evil = format!("docker.io/attacker/evil@sha256:{}", "e".repeat(64));
        let wasm = make_wasm_with_sections(&[
            &[("bldimg", &evil), ("rsver", "1.96.0")],
            &[
                ("cliver", "27.0.0#abcdef"),
                ("bldimg", &good_bldimg()),
                ("source_sha256", &"b".repeat(64)),
            ],
        ]);
        let meta = extract_metadata(&wasm).unwrap();
        assert_eq!(meta.bldimg, good_bldimg());
    }

    #[test]
    fn extract_metadata_empty_meta_errors() {
        let wasm = empty_wasm_module();
        let err = extract_metadata(&wasm).unwrap_err();
        assert!(matches!(err, Error::NoMeta));
    }
}
