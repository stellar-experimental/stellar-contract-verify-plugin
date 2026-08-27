//! Container rebuild and the pure logic around it.
//!
//! A lean, synchronous stand-in for the CLI's `container::shared` +
//! `verifiable::run_in_container`. The engine (docker or Apple's `container`) is
//! selected via [`crate::engine::ContainerArgs`]. `build_container_command`,
//! `collect_release_wasms`, and `find_rebuilt_wasm` are ported verbatim from
//! `contract verify` — they're pure and carry the security-relevant behavior
//! (metadata replay, pre-built-artifact exclusion).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use walkdir::WalkDir;

use crate::engine::{ContainerArgs, RunArgs};
use crate::error::Error;
use crate::meta::ExtractedMetadata;
use crate::print::{sanitize, Print};

/// Pull the recorded build image so the rebuild runs against exactly the pinned
/// digest. Output is streamed to the terminal (unless quiet) so the user sees
/// pull progress.
pub fn pull_image(image_ref: &str, args: &ContainerArgs, print: &Print) -> Result<(), Error> {
    print.infoln(format!("Pulling image {}", sanitize(image_ref)));
    let (stdout, stderr) = if print.quiet {
        (Stdio::null(), Stdio::null())
    } else {
        (Stdio::inherit(), Stdio::inherit())
    };
    let status = args
        .pull_command(image_ref)
        .stdout(stdout)
        .stderr(stderr)
        .status()
        .map_err(|e| args.invoke_error(e))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::PullFailed {
            image: image_ref.to_string(),
        })
    }
}

/// Run `cmd` in a throwaway `run --rm` container (optionally overriding the
/// entrypoint) and return its captured stdout. Only stdout is collected; stderr
/// and the exit status are ignored, matching how every probe treats a missing
/// subcommand or unexpected output as "unsupported".
fn run_probe(
    image_ref: &str,
    args: &ContainerArgs,
    entrypoint: Option<&str>,
    cmd: &[&str],
) -> Result<String, Error> {
    let mut command = args.base_command();
    command.args(["run", "--rm"]);
    if let Some(entrypoint) = entrypoint {
        command.args(["--entrypoint", entrypoint]);
    }
    command.arg(image_ref);
    command.args(cmd);
    let output = command.output().map_err(|e| args.invoke_error(e))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Probe whether the container's `stellar contract build` accepts `--locked`.
/// The flag was added in cli 25.2.0; older images reject it outright, which
/// would fail the build. Rather than map versions, ask the container's own
/// `contract build --help` whether the flag exists. On any probe failure returns
/// false — the conservative assumption that the flag is absent.
pub fn probe_supports_locked(image_ref: &str, args: &ContainerArgs, print: &Print) -> bool {
    match run_probe(image_ref, args, None, &["contract", "build", "--help"]) {
        Ok(help) => help.contains("--locked"),
        Err(e) => {
            print.warnln(format!(
                "Could not probe whether the container's `contract build` supports --locked ({e}); building without it"
            ));
            false
        }
    }
}

/// Probe the image for the toolchain rustup uses by default, so it can be pinned
/// via `RUSTUP_TOOLCHAIN`. Without this pin, a `rust-toolchain.toml` in the
/// source could make rustup switch toolchains mid-build, defeating the
/// digest-pinned image. Returns `None` on any failure (e.g. an image without
/// rustup), so the build proceeds without the pin rather than failing.
fn probe_active_toolchain(image_ref: &str, args: &ContainerArgs) -> Option<String> {
    let stdout = run_probe(
        image_ref,
        args,
        Some("rustup"),
        &["show", "active-toolchain"],
    )
    .ok()?;
    stdout.split_whitespace().next().map(str::to_string)
}

/// Rebuild `container_cmd` (a `contract build …` argv) in `image_ref`, with the
/// materialized source bind-mounted at `/source`. `env` entries become `-e
/// KEY=VALUE` flags; `run_args` adds resource limits. Also pins
/// `RUSTUP_TOOLCHAIN` to the image's active toolchain unless already set.
#[allow(clippy::too_many_arguments)]
pub fn run_in_container(
    image_ref: &str,
    source_root: &Path,
    container_cmd: &[String],
    env: &[String],
    args: &ContainerArgs,
    run_args: &RunArgs,
    print: &Print,
    verbose: bool,
) -> Result<(), Error> {
    let bind = format!("{}:/source", source_root.display());

    let mut env = env.to_vec();
    if !env.iter().any(|e| e.starts_with("RUSTUP_TOOLCHAIN=")) {
        if let Some(toolchain) = probe_active_toolchain(image_ref, args) {
            env.push(format!("RUSTUP_TOOLCHAIN={toolchain}"));
        }
    }

    let run_flags = run_args.flags();

    // A copy-pasteable reproduce line, rendered against the same engine binary
    // and flags we actually run.
    let mut extra = String::new();
    for f in &run_flags {
        extra.push(' ');
        extra.push_str(&shell_escape(f));
    }
    for e in &env {
        extra.push_str(" -e ");
        extra.push_str(&shell_escape(e));
    }
    // The reproduce line aggregates untrusted values (image ref, replayed meta,
    // env); `sanitize` it once so both the verbose print and the `ContainerExit`
    // error are free of terminal escapes. Shell-escaping stays intact — quotes
    // aren't control characters.
    let reproduce = sanitize(&format!(
        "{} run --rm{extra} -v {bind} {image_ref} {}",
        args.reproduce_prefix(),
        container_cmd
            .iter()
            .map(|t| shell_escape(t))
            .collect::<Vec<_>>()
            .join(" ")
    ));

    print.infoln(format!(
        "Running verifiable build in {} (mount {bind})",
        sanitize(image_ref)
    ));
    if verbose {
        print.infoln(format!("Running: {reproduce}"));
    }

    // Name the container so an interrupt can target it: killing the CLI process
    // alone leaves the engine still building. A random UUID keeps concurrent
    // verifies from colliding, and it's kept out of the reproduce line (a fixed
    // name there would clash on re-run).
    let container_name = format!("stellar-contract-verify-{}", uuid::Uuid::new_v4());
    crate::cleanup::set_container(args.kill_argv(&container_name));

    let mut command = args.base_command();
    command.args(["run", "--rm", "--name", &container_name]);
    command.args(&run_flags);
    command.args(["-v", &bind, "-w", "/source"]);
    for e in &env {
        command.args(["-e", e]);
    }
    command.arg(image_ref);
    command.args(container_cmd);

    // Stream the build's cargo output when verbose; otherwise discard it.
    // `quiet` overrides verbose.
    let (stdout, stderr) = if verbose && !print.quiet {
        (Stdio::inherit(), Stdio::inherit())
    } else {
        (Stdio::null(), Stdio::null())
    };
    command.stdout(stdout).stderr(stderr);

    let status = command.status().map_err(|e| args.invoke_error(e))?;
    // The container has exited (`--rm` removed it); drop the interrupt handle so
    // a later signal doesn't try to kill a container that's already gone.
    crate::cleanup::clear_container();
    if !status.success() {
        return Err(Error::ContainerExit {
            status: status.code().unwrap_or(-1).into(),
            command: reproduce,
        });
    }
    Ok(())
}

/// Minimal POSIX shell single-quote escaping for the reproduce line.
fn shell_escape(token: &str) -> String {
    if !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b))
    {
        token.to_string()
    } else {
        format!("'{}'", token.replace('\'', r"'\''"))
    }
}

/// Compose the argv we hand to the container's `stellar contract build`, plus
/// the env vars to apply via docker `-e`.
///
/// The metadata is *replayed*, not reconstructed: every entry the WASM records
/// (`meta.meta_entries`, already stripped of the keys the rebuild regenerates)
/// is re-emitted as a `--meta key=value` in its original order, so the rebuilt
/// `contractmetav0` mirrors the source WASM regardless of how it was produced.
///
/// The `bldopt` entries additionally drive the *build flags*: each is forwarded
/// as a flag to the inner `contract build`, with two exceptions —
///   - `--env=` bldopts are applied via docker `-e` (as the original build did),
///     not forwarded. They're shell-escaped at the source, so we unescape back
///     to a raw `NAME=VALUE`.
///   - `--meta=` bldopts are NOT forwarded: the metadata they produced is
///     already a standalone entry in `meta_entries` and replayed above, so
///     forwarding them too would write the value twice.
///
/// `supports_locked`: whether the recorded bldimg's `contract build` accepts
/// `--locked`. When false the flag is never injected, so a rebuild against an
/// older image doesn't fail on an unknown argument.
pub fn build_container_command(
    meta: &ExtractedMetadata,
    supports_locked: bool,
) -> (Vec<String>, Vec<String>) {
    let mut forwarded: Vec<String> = Vec::new();
    let mut env: Vec<String> = Vec::new();
    for o in &meta.bldopts {
        // Every recorded bldopt is shell-escaped at the source so it's valid
        // shell on its own — e.g. `--meta=source_repo='github:foo'` or
        // `--env=B='a b'`. The single-package rebuild hands argv straight to
        // `stellar` with no shell, so unescape each bldopt back to the one raw
        // argv token the original build used; otherwise the quoting leaks into
        // the value.
        let token = shlex::split(o)
            .and_then(|mut v| (v.len() == 1).then(|| v.remove(0)))
            .unwrap_or_else(|| o.clone());
        if let Some(kv) = token.strip_prefix("--env=") {
            env.push(kv.to_string());
        } else if token.starts_with("--meta=") {
            // The metadata this produced is replayed from `meta_entries`;
            // forwarding it as a flag too would write the value twice.
        } else {
            forwarded.push(token);
        }
    }

    // When the image supports it, `--locked` is forced — even if the original
    // somehow lacked it — so the verifier insists on a locked rebuild and
    // dependency drift can't move bytes underneath us. Older images reject the
    // flag, so it's omitted there.
    if supports_locked && !forwarded.iter().any(|a| a == "--locked") {
        forwarded.insert(0, "--locked".to_string());
    }

    // Replay every recorded meta entry verbatim, in the WASM's own order, so the
    // rebuilt section matches the original byte-for-byte.
    let mut metadata: Vec<String> = Vec::new();
    for (k, v) in &meta.meta_entries {
        metadata.push("--meta".to_string());
        metadata.push(format!("{k}={v}"));
    }

    let mut args = vec!["contract".to_string(), "build".to_string()];
    args.extend(forwarded);
    args.extend(metadata);
    (args, env)
}

/// The two wasm release-output suffixes cargo may write to, newest first. The
/// match is the 2-component `<triple>/release` tail rather than
/// `target/<triple>/release`: cargo's target dir is not fixed at `target/`, but
/// the `<triple>/release/` layout beneath it is stable. Matching the tail also
/// excludes intermediate artifacts under `release/deps/`.
const WASM_RELEASE_SUFFIXES: [&str; 2] =
    ["wasm32v1-none/release", "wasm32-unknown-unknown/release"];

/// Walk `root` and return every `*.wasm` sitting directly in a `<triple>/release`
/// output directory. The target dir's location is not fixed relative to the
/// crate manifest — in a Cargo workspace it lives at the workspace root — so we
/// search the whole tree rather than guess where it is.
fn collect_release_wasms(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .map(walkdir::DirEntry::into_path)
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wasm"))
        .filter(|p| {
            p.parent()
                .is_some_and(|parent| WASM_RELEASE_SUFFIXES.iter().any(|s| parent.ends_with(s)))
        })
        .collect()
}

/// Snapshot the WASM artifacts already present under `source_root` *before* the
/// rebuild. A conformant source archive ships no build output, so anything here
/// was planted; excluding these from the post-build search stops an attacker
/// from smuggling a pre-built binary into the archive to spoof a match.
pub fn snapshot_preexisting_wasms(source_root: &Path) -> HashSet<PathBuf> {
    collect_release_wasms(source_root).into_iter().collect()
}

/// Locate the WASM produced by the container's rebuild under `source_root`.
///
/// Only artifacts *created by this rebuild* are eligible: any `*.wasm` present
/// before the build (captured in `preexisting`) is excluded, so a pre-built
/// binary planted in the source archive can't masquerade as the rebuild output.
/// If a `--package=<name>` bldopt was recorded, prefer that file.
pub fn find_rebuilt_wasm(
    source_root: &Path,
    meta: &ExtractedMetadata,
    preexisting: &HashSet<PathBuf>,
) -> Result<PathBuf, Error> {
    let preferred_pkg = meta
        .bldopts
        .iter()
        .find_map(|opt| opt.strip_prefix("--package=").map(|s| s.replace('-', "_")));

    let found: Vec<PathBuf> = collect_release_wasms(source_root)
        .into_iter()
        .filter(|p| !preexisting.contains(p))
        .collect();

    if let Some(pkg) = &preferred_pkg {
        let want = format!("{pkg}.wasm");
        if let Some(p) = found.iter().find(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n == want)
        }) {
            return Ok(p.clone());
        }
    }

    match found.len() {
        0 => Err(Error::NoRebuiltWasm {
            target: source_root.to_path_buf(),
        }),
        1 => Ok(found.into_iter().next().unwrap()),
        _ => Err(Error::AmbiguousRebuiltWasm {
            target: source_root.to_path_buf(),
            found: found
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_bldimg() -> String {
        format!("docker.io/stellar/stellar-cli@sha256:{}", "a".repeat(64))
    }

    #[test]
    fn build_container_command_replays_meta_in_order_and_forwards_build_flags() {
        let meta_entries = vec![
            ("bldimg".to_string(), good_bldimg()),
            (
                "source_uri".to_string(),
                "https://github.com/foo/bar".to_string(),
            ),
            ("source_sha256".to_string(), "b".repeat(64)),
            ("home_domain".to_string(), "fnando.com".to_string()),
            ("bldopt".to_string(), "--locked".to_string()),
            (
                "bldopt".to_string(),
                "--meta=home_domain=fnando.com".to_string(),
            ),
            ("bldopt".to_string(), "--optimize".to_string()),
            ("bldopt".to_string(), "--env=A=1".to_string()),
            (
                "bldopt".to_string(),
                "--env=B='this is very nice'".to_string(),
            ),
        ];
        let meta = ExtractedMetadata {
            bldimg: good_bldimg(),
            source_uri: Some("https://github.com/foo/bar".to_string()),
            source_sha256: Some("b".repeat(64)),
            bldopts: vec![
                "--locked".to_string(),
                "--meta=home_domain=fnando.com".to_string(),
                "--optimize".to_string(),
                "--env=A=1".to_string(),
                "--env=B='this is very nice'".to_string(),
            ],
            meta_entries: meta_entries.clone(),
        };
        let (cmd, env) = build_container_command(&meta, true);

        assert_eq!(&cmd[..2], &["contract".to_string(), "build".to_string()]);
        assert!(cmd.contains(&"--locked".to_string()));
        assert!(cmd.contains(&"--optimize".to_string()));
        assert!(!cmd.iter().any(|a| a.starts_with("--meta=")));
        assert!(!cmd.iter().any(|a| a.starts_with("--env=")));
        assert_eq!(
            env,
            vec!["A=1".to_string(), "B=this is very nice".to_string()]
        );

        let replayed: Vec<(String, String)> = cmd
            .windows(2)
            .filter(|w| w[0] == "--meta")
            .map(|w| {
                let (k, v) = w[1].split_once('=').unwrap();
                (k.to_string(), v.to_string())
            })
            .collect();
        assert_eq!(replayed, meta_entries);
    }

    #[test]
    fn build_container_command_replays_meta_bldopt_verbatim_without_forwarding() {
        let meta = ExtractedMetadata {
            bldimg: good_bldimg(),
            source_uri: Some("https://github.com/foo/bar".to_string()),
            source_sha256: Some("b".repeat(64)),
            bldopts: vec![
                "--meta=source_repo='github:LayerZero-Labs/monorepo-external'".to_string(),
            ],
            meta_entries: vec![
                (
                    "source_repo".to_string(),
                    "github:LayerZero-Labs/monorepo-external".to_string(),
                ),
                (
                    "bldopt".to_string(),
                    "--meta=source_repo='github:LayerZero-Labs/monorepo-external'".to_string(),
                ),
            ],
        };
        let (cmd, _env) = build_container_command(&meta, true);

        assert!(
            !cmd.iter().any(|a| a.starts_with("--meta=")),
            "--meta bldopts must not be forwarded, got {cmd:?}"
        );
        assert!(
            cmd.windows(2).any(|w| w[0] == "--meta"
                && w[1] == "source_repo=github:LayerZero-Labs/monorepo-external"),
            "the standalone meta entry must be replayed, got {cmd:?}"
        );
        assert!(
            cmd.windows(2).any(|w| w[0] == "--meta"
                && w[1] == "bldopt=--meta=source_repo='github:LayerZero-Labs/monorepo-external'"),
            "the bldopt meta must round-trip the escaped original, got {cmd:?}"
        );
    }

    #[test]
    fn build_container_command_injects_locked_when_missing() {
        let meta = ExtractedMetadata {
            bldimg: good_bldimg(),
            source_uri: Some("https://github.com/foo/bar".to_string()),
            source_sha256: Some("b".repeat(64)),
            bldopts: vec!["--meta=author=alice".to_string()],
            meta_entries: vec![
                ("author".to_string(), "alice".to_string()),
                ("bldopt".to_string(), "--meta=author=alice".to_string()),
            ],
        };
        let (cmd, _env) = build_container_command(&meta, true);
        let locked_count = cmd.iter().filter(|s| *s == "--locked").count();
        assert_eq!(
            locked_count, 1,
            "expected exactly one --locked, got {locked_count} in {cmd:?}"
        );
    }

    #[test]
    fn build_container_command_omits_locked_when_unsupported() {
        let meta = ExtractedMetadata {
            bldimg: good_bldimg(),
            source_uri: Some("https://github.com/foo/bar".to_string()),
            source_sha256: Some("b".repeat(64)),
            bldopts: vec!["--optimize".to_string()],
            meta_entries: vec![("bldopt".to_string(), "--optimize".to_string())],
        };
        let (cmd, _env) = build_container_command(&meta, false);
        assert!(
            !cmd.iter().any(|a| a == "--locked"),
            "expected no forwarded --locked in {cmd:?}"
        );
    }

    fn meta_with_bldopts(bldopts: Vec<String>) -> ExtractedMetadata {
        ExtractedMetadata {
            bldimg: good_bldimg(),
            source_uri: Some("https://github.com/foo/bar".to_string()),
            source_sha256: Some("b".repeat(64)),
            bldopts,
            meta_entries: Vec::new(),
        }
    }

    #[test]
    fn find_rebuilt_wasm_picks_single() {
        let dir = tempfile::TempDir::new().unwrap();
        let release = dir.path().join("target/wasm32v1-none/release");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(release.join("hello.wasm"), b"x").unwrap();

        let meta = meta_with_bldopts(vec![]);
        let p = find_rebuilt_wasm(dir.path(), &meta, &HashSet::new()).unwrap();
        assert!(p.ends_with("hello.wasm"));
    }

    #[test]
    fn find_rebuilt_wasm_disambiguates_by_package() {
        let dir = tempfile::TempDir::new().unwrap();
        let release = dir.path().join("target/wasm32v1-none/release");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(release.join("hello.wasm"), b"x").unwrap();
        std::fs::write(release.join("other_thing.wasm"), b"x").unwrap();

        let meta = meta_with_bldopts(vec!["--package=other-thing".to_string()]);
        let p = find_rebuilt_wasm(dir.path(), &meta, &HashSet::new()).unwrap();
        assert!(p.ends_with("other_thing.wasm"));
    }

    #[test]
    fn find_rebuilt_wasm_errors_when_ambiguous_without_package() {
        let dir = tempfile::TempDir::new().unwrap();
        let release = dir.path().join("target/wasm32v1-none/release");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(release.join("hello.wasm"), b"x").unwrap();
        std::fs::write(release.join("other.wasm"), b"x").unwrap();

        let meta = meta_with_bldopts(vec![]);
        let err = find_rebuilt_wasm(dir.path(), &meta, &HashSet::new()).unwrap_err();
        assert!(matches!(err, Error::AmbiguousRebuiltWasm { .. }));
    }

    #[test]
    fn find_rebuilt_wasm_errors_when_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let meta = meta_with_bldopts(vec![]);
        let err = find_rebuilt_wasm(dir.path(), &meta, &HashSet::new()).unwrap_err();
        assert!(matches!(err, Error::NoRebuiltWasm { .. }));
    }

    #[test]
    fn find_rebuilt_wasm_finds_target_at_workspace_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let release = dir.path().join("target/wasm32v1-none/release");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(release.join("blocked_message_lib.wasm"), b"x").unwrap();
        std::fs::create_dir_all(
            dir.path()
                .join("contracts/message-libs/blocked-message-lib/src"),
        )
        .unwrap();

        let meta = meta_with_bldopts(vec![
            "--manifest-path=contracts/message-libs/blocked-message-lib/Cargo.toml".to_string(),
            "--package=blocked-message-lib".to_string(),
        ]);
        let p = find_rebuilt_wasm(dir.path(), &meta, &HashSet::new()).unwrap();
        assert!(p.ends_with("blocked_message_lib.wasm"));
    }

    #[test]
    fn find_rebuilt_wasm_finds_relocated_target_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let release = dir.path().join("custom-out/wasm32-unknown-unknown/release");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(release.join("hello.wasm"), b"x").unwrap();

        let meta = meta_with_bldopts(vec![]);
        let p = find_rebuilt_wasm(dir.path(), &meta, &HashSet::new()).unwrap();
        assert!(p.ends_with("hello.wasm"));
    }

    #[test]
    fn find_rebuilt_wasm_ignores_release_deps_artifacts() {
        let dir = tempfile::TempDir::new().unwrap();
        let release = dir.path().join("target/wasm32v1-none/release");
        let deps = release.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(release.join("hello.wasm"), b"x").unwrap();
        std::fs::write(deps.join("hello-abc123.wasm"), b"x").unwrap();

        let meta = meta_with_bldopts(vec![]);
        let p = find_rebuilt_wasm(dir.path(), &meta, &HashSet::new()).unwrap();
        assert!(p.ends_with("hello.wasm"));
    }

    #[test]
    fn find_rebuilt_wasm_excludes_preexisting_injected_wasm() {
        let dir = tempfile::TempDir::new().unwrap();
        let release = dir.path().join("target/wasm32v1-none/release");
        std::fs::create_dir_all(&release).unwrap();
        let injected = release.join("hello.wasm");
        std::fs::write(&injected, b"x").unwrap();

        let preexisting = snapshot_preexisting_wasms(dir.path());
        assert!(preexisting.contains(&injected));

        let meta = meta_with_bldopts(vec![]);
        let err = find_rebuilt_wasm(dir.path(), &meta, &preexisting).unwrap_err();
        assert!(matches!(err, Error::NoRebuiltWasm { .. }));
    }

    #[test]
    fn find_rebuilt_wasm_keeps_freshly_built_alongside_preexisting() {
        let dir = tempfile::TempDir::new().unwrap();
        let release = dir.path().join("target/wasm32v1-none/release");
        std::fs::create_dir_all(&release).unwrap();
        let old = release.join("stale.wasm");
        std::fs::write(&old, b"x").unwrap();

        let preexisting = snapshot_preexisting_wasms(dir.path());

        std::fs::write(release.join("hello.wasm"), b"x").unwrap();

        let meta = meta_with_bldopts(vec![]);
        let p = find_rebuilt_wasm(dir.path(), &meta, &preexisting).unwrap();
        assert!(p.ends_with("hello.wasm"));
    }
}
