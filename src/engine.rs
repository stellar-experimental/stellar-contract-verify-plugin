//! Container-engine selection, ported from the CLI's `container::shared`.
//!
//! Supports Docker (or any Docker-compatible CLI) and Apple's `container` CLI,
//! selectable via `--engine` / `STELLAR_CONTAINER_ENGINE`. `--docker-host` and
//! the `--cpus` / `--memory` resource limits are honored where the engine
//! supports them. Only the subset `verify` needs is reproduced (base command,
//! pull, run); the container is invoked synchronously via `std::process`.

use core::fmt;
use std::process::Command;

use clap::{Parser, ValueEnum};

use crate::print::Print;

const DOCKER_HOST_HELP: &str = "Optional argument to override the default docker host. Useful with a non-standard docker host path, e.g. Docker Desktop's $HOME/.docker/run/docker.sock. Ignored by non-docker engines.";

/// Container runtime to shell out to.
#[derive(ValueEnum, Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum Engine {
    /// Docker, or any Docker-compatible CLI.
    #[default]
    Docker,
    /// Apple's `container` CLI (macOS 26+, Apple silicon).
    AppleContainer,
}

impl Engine {
    /// The executable name to invoke on `PATH`.
    fn program(self) -> &'static str {
        match self {
            Engine::Docker => "docker",
            Engine::AppleContainer => "container",
        }
    }

    /// Only docker honors `--docker-host`/`DOCKER_HOST`.
    fn supports_docker_host(self) -> bool {
        matches!(self, Engine::Docker)
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The flag value, which differs from the binary name for Apple.
        let s = match self {
            Engine::Docker => "docker",
            Engine::AppleContainer => "apple-container",
        };
        write!(f, "{s}")
    }
}

/// Engine-selection flags. Mirrors the CLI's container `Args`.
#[derive(Parser, Debug, Clone, Default)]
pub struct ContainerArgs {
    /// Override the default docker host (docker engine only).
    #[arg(short = 'd', long, env = "DOCKER_HOST", help = DOCKER_HOST_HELP)]
    pub docker_host: Option<String>,

    /// Container engine to use [default: docker].
    #[arg(long, value_enum, env = "STELLAR_CONTAINER_ENGINE")]
    pub engine: Option<Engine>,
}

impl ContainerArgs {
    fn engine(&self) -> Engine {
        self.engine.unwrap_or_default()
    }

    /// When neither `--engine` nor `STELLAR_CONTAINER_ENGINE` is set (clap would
    /// have populated `engine` from either), adopt the `stellar` CLI's resolved
    /// default — which includes the `config.toml` value written by `container
    /// use`. We ask the CLI itself (`stellar env STELLAR_CONTAINER_ENGINE`)
    /// rather than read the config, so the resolution stays in lockstep with the
    /// CLI. This matters only when the binary is run standalone: launched as a
    /// plugin, we already inherit `STELLAR_CONTAINER_ENGINE` from the parent
    /// `stellar` process. Silently leaves the docker default on any failure
    /// (no `stellar` on PATH, empty/unknown value).
    pub fn resolve_default_from_cli(&mut self) {
        if self.engine.is_some() {
            return;
        }
        let Ok(output) = Command::new("stellar")
            .args(["env", "STELLAR_CONTAINER_ENGINE"])
            .output()
        else {
            return;
        };
        if !output.status.success() {
            return;
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if let Ok(engine) = Engine::from_str(&value, true) {
            self.engine = Some(engine);
        }
    }

    /// The engine's executable name (`docker`, `container`), for rendering
    /// copy-pasteable reproduce commands that name the same binary we ran.
    pub fn program(&self) -> &'static str {
        self.engine().program()
    }

    /// Map a spawn/IO error to an engine-aware error carrying the binary name.
    pub fn invoke_error(&self, source: std::io::Error) -> crate::error::Error {
        crate::error::Error::EngineInvoke {
            program: self.program().to_string(),
            source,
        }
    }

    /// The leading command tokens (binary + any `-H <host>`), shared by the real
    /// command and the reproduce line so they stay in lockstep.
    fn prefix_tokens(&self) -> Vec<String> {
        let mut tokens = vec![self.program().to_string()];
        if self.engine().supports_docker_host() {
            if let Some(host) = &self.docker_host {
                tokens.push("-H".to_string());
                tokens.push(host.clone());
            }
        }
        tokens
    }

    /// Base command for the selected engine, including a docker `-H <host>` when
    /// applicable. Host resolution is otherwise left to the engine.
    pub fn base_command(&self) -> Command {
        let mut tokens = self.prefix_tokens().into_iter();
        let mut cmd = Command::new(tokens.next().expect("program token"));
        cmd.args(tokens);
        cmd
    }

    /// The pull command: docker `pull <image>` vs Apple's `image pull <image>`.
    pub fn pull_command(&self, image: &str) -> Command {
        let mut cmd = self.base_command();
        match self.engine() {
            Engine::Docker => cmd.args(["pull", image]),
            Engine::AppleContainer => cmd.args(["image", "pull", image]),
        };
        cmd
    }

    /// The `program [-H host]` prefix as a shell string, for the reproduce line.
    pub fn reproduce_prefix(&self) -> String {
        self.prefix_tokens().join(" ")
    }

    /// The argv that force-kills a named container (`kill <name>`), for interrupt
    /// cleanup. Both docker and Apple's `container` accept `kill <name>`.
    pub fn kill_argv(&self, name: &str) -> Vec<String> {
        let mut argv = self.prefix_tokens();
        argv.push("kill".to_string());
        argv.push(name.to_string());
        argv
    }

    /// Warn when `--docker-host`/`DOCKER_HOST` was provided but the selected
    /// engine ignores it.
    pub fn warn_if_host_ignored(&self, print: &Print) {
        if self.docker_host.is_some() && !self.engine().supports_docker_host() {
            print.warnln(format!(
                "`--docker-host`/`DOCKER_HOST` is ignored because the `{}` engine does not support it",
                self.engine()
            ));
        }
    }
}

/// Resource limits applied to the build container. Both `--cpus` and `--memory`
/// are accepted verbatim by docker and Apple's `container` on `run`.
#[derive(Parser, Debug, Clone, Default)]
pub struct RunArgs {
    /// Limit the number of CPUs available to the build container, e.g. `2`. A
    /// whole number: Apple's `container` engine does not accept fractional CPUs.
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Limit the memory available to the build container, e.g. `2g` or `512m`.
    #[arg(long)]
    pub memory: Option<String>,
}

impl RunArgs {
    /// The resource-limit flags as `run` argv tokens; empty when none set.
    pub fn flags(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(cpus) = &self.cpus {
            out.push("--cpus".to_string());
            out.push(cpus.to_string());
        }
        if let Some(memory) = &self.memory {
            out.push("--memory".to_string());
            out.push(memory.clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(docker_host: Option<&str>, engine: Option<Engine>) -> ContainerArgs {
        ContainerArgs {
            docker_host: docker_host.map(String::from),
            engine,
        }
    }

    fn program_of(cmd: &Command) -> String {
        cmd.get_program().to_string_lossy().into_owned()
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn engine_defaults_to_docker() {
        assert_eq!(args(None, None).engine(), Engine::Docker);
        assert_eq!(
            args(None, Some(Engine::AppleContainer)).engine(),
            Engine::AppleContainer
        );
    }

    #[test]
    fn docker_pull_uses_bare_pull() {
        let cmd = args(None, None).pull_command("img:tag");
        assert_eq!(program_of(&cmd), "docker");
        assert_eq!(args_of(&cmd), ["pull", "img:tag"]);
    }

    #[test]
    fn apple_pull_uses_image_pull_and_ignores_host() {
        let cmd = args(Some("ssh://host"), Some(Engine::AppleContainer)).pull_command("img:tag");
        assert_eq!(program_of(&cmd), "container");
        assert_eq!(args_of(&cmd), ["image", "pull", "img:tag"]);
    }

    #[test]
    fn docker_base_command_passes_host_as_h_flag() {
        let cmd = args(Some("ssh://host"), None).base_command();
        assert_eq!(program_of(&cmd), "docker");
        assert_eq!(args_of(&cmd), ["-H", "ssh://host"]);
    }

    #[test]
    fn apple_base_command_omits_host() {
        let cmd = args(Some("ssh://host"), Some(Engine::AppleContainer)).base_command();
        assert_eq!(program_of(&cmd), "container");
        assert!(args_of(&cmd).is_empty());
    }

    #[test]
    fn reproduce_prefix_reflects_engine_and_host() {
        assert_eq!(args(None, None).reproduce_prefix(), "docker");
        assert_eq!(
            args(Some("ssh://host"), None).reproduce_prefix(),
            "docker -H ssh://host"
        );
        assert_eq!(
            args(Some("ssh://host"), Some(Engine::AppleContainer)).reproduce_prefix(),
            "container"
        );
    }

    #[test]
    fn host_ignored_warning_only_for_non_docker() {
        // No panic paths; just exercise the branches with a quiet printer.
        let quiet = Print::new(true);
        args(Some("ssh://host"), Some(Engine::AppleContainer)).warn_if_host_ignored(&quiet);
        args(Some("ssh://host"), None).warn_if_host_ignored(&quiet);
    }

    #[test]
    fn kill_argv_includes_host_for_docker_and_omits_for_apple() {
        assert_eq!(
            args(Some("ssh://host"), None).kill_argv("build-1"),
            ["docker", "-H", "ssh://host", "kill", "build-1"]
        );
        assert_eq!(
            args(Some("ssh://host"), Some(Engine::AppleContainer)).kill_argv("build-1"),
            ["container", "kill", "build-1"]
        );
    }

    #[test]
    fn run_args_flags_emit_only_set_limits() {
        assert!(RunArgs::default().flags().is_empty());
        assert_eq!(
            RunArgs {
                cpus: Some(1),
                memory: None,
            }
            .flags(),
            ["--cpus", "1"]
        );
        assert_eq!(
            RunArgs {
                cpus: Some(2),
                memory: Some("2g".to_string()),
            }
            .flags(),
            ["--cpus", "2", "--memory", "2g"]
        );
    }
}
