//! Interrupt cleanup.
//!
//! A rebuild can run for minutes inside a container while the materialized
//! source sits in a tempdir. If the user hits Ctrl-C (or the process is sent
//! SIGTERM/SIGHUP), we must not orphan the build container or leak the tempdir:
//! terminating the process skips `Drop`, so the `TempDir` guard never runs and
//! the engine keeps building in a container it no longer has a handle to.
//!
//! We register the in-flight container's kill command and the tempdir path in a
//! small global, and a signal handler drains it: kill the container, remove the
//! tempdir (unless `--keep`), then exit. Mirrors the CLI's signal handling,
//! adapted to a synchronous process via the `ctrlc` crate.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

struct State {
    /// Full argv (`[program, "-H", host?, "kill", name]`) that stops the build
    /// container, or `None` when no container is currently running.
    container_kill: Option<Vec<String>>,
    /// The materialized-source tempdir to remove on interrupt.
    tempdir: Option<PathBuf>,
    /// When set, keep the tempdir (mirrors `--keep`).
    keep: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    container_kill: None,
    tempdir: None,
    keep: false,
});

/// Install the signal handler (SIGINT via Ctrl-C, plus SIGTERM/SIGHUP through
/// the `termination` feature). Call once, early in `main`. A failure to install
/// is non-fatal — cleanup is best-effort.
pub fn install() {
    let _ = ctrlc::set_handler(|| {
        run();
        // 128 + SIGINT(2): the conventional exit code for a Ctrl-C'd process.
        std::process::exit(130);
    });
}

/// Record the tempdir to remove on interrupt (and whether `--keep` should spare
/// it).
pub fn set_tempdir(path: PathBuf, keep: bool) {
    if let Ok(mut state) = STATE.lock() {
        state.tempdir = Some(path);
        state.keep = keep;
    }
}

/// Record the command that stops the currently-running build container, so an
/// interrupt can tear it down.
pub fn set_container(kill_argv: Vec<String>) {
    if let Ok(mut state) = STATE.lock() {
        state.container_kill = Some(kill_argv);
    }
}

/// Forget the build container once it has exited, so a later interrupt doesn't
/// try to kill a container that's already gone.
pub fn clear_container() {
    if let Ok(mut state) = STATE.lock() {
        state.container_kill = None;
    }
}

/// Kill the in-flight container and remove the tempdir. Runs on the signal
/// handler thread (an ordinary thread under `ctrlc`, so blocking calls are
/// fine). Snapshots the state under the lock, then releases it before doing I/O.
fn run() {
    let (kill, tempdir, keep) = match STATE.lock() {
        Ok(state) => (
            state.container_kill.clone(),
            state.tempdir.clone(),
            state.keep,
        ),
        Err(_) => (None, None, false),
    };

    if let Some(argv) = kill {
        if let Some((program, rest)) = argv.split_first() {
            eprintln!("⚠️ Interrupted; stopping build container");
            let _ = Command::new(program)
                .args(rest)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    if !keep {
        if let Some(dir) = tempdir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
