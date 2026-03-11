use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::paths::{SyncDirection, SyncPlan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, spec: &CommandSpec) -> Result<()>;
}

#[derive(Debug, Clone, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, spec: &CommandSpec) -> Result<()> {
        let mut command = std::process::Command::new(&spec.program);
        command.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        let status = command
            .status()
            .with_context(|| format!("failed to run {}", spec.program.to_string_lossy()))?;
        if !status.success() {
            bail!(
                "command {} failed with status {}",
                spec.program.to_string_lossy(),
                format_exit_status(status)
            );
        }
        Ok(())
    }
}

fn format_exit_status(status: ExitStatus) -> String {
    status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string())
}

pub type SharedRunner = Arc<dyn CommandRunner>;

pub fn rsync_command(config: &Config, plan: &SyncPlan) -> CommandSpec {
    let source;
    let dest;
    match plan.direction {
        SyncDirection::Push => {
            source = plan.local_path.as_os_str().to_os_string();
            dest = format!("{}:{}", config.ssh_target, plan.remote_path.display()).into();
        }
        SyncDirection::Pull => {
            source = format!("{}:{}", config.ssh_target, plan.remote_path.display()).into();
            dest = plan.local_path.as_os_str().to_os_string();
        }
    }
    CommandSpec::new("rsync").args([
        OsString::from("-az"),
        OsString::from("--delete"),
        OsString::from("-e"),
        OsString::from("ssh"),
        source,
        dest,
    ])
}

pub fn reverse_tunnel_command(config: &Config) -> CommandSpec {
    CommandSpec::new("ssh").args([
        OsString::from("-N"),
        OsString::from("-R"),
        OsString::from(format!("{}:127.0.0.1:{}", config.rpc_port, config.rpc_port)),
        OsString::from(&config.ssh_target),
    ])
}

pub fn install_prepare_command(config: &Config) -> CommandSpec {
    CommandSpec::new("ssh").args([
        OsString::from(&config.ssh_target),
        OsString::from(format!(
            "mkdir -p \"$(dirname {})\"",
            shell_quote(&config.remote_bin_path)
        )),
    ])
}

pub fn install_copy_command(config: &Config, artifact: &OsStr) -> CommandSpec {
    CommandSpec::new("scp").args([
        artifact.to_os_string(),
        OsString::from(format!(
            "{}:{}.tmp",
            config.ssh_target, config.remote_bin_path
        )),
    ])
}

pub fn install_finalize_command(config: &Config) -> CommandSpec {
    CommandSpec::new("ssh").args([
        OsString::from(&config.ssh_target),
        OsString::from(format!(
            "chmod +x {path}.tmp && mv {path}.tmp {path}",
            path = shell_quote(&config.remote_bin_path)
        )),
    ])
}

fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\"'\"'"))
}

#[derive(Debug, Clone, Default)]
pub struct RecordingRunner {
    specs: Arc<Mutex<Vec<CommandSpec>>>,
    fail_program: Arc<Mutex<Option<String>>>,
}

impl RecordingRunner {
    pub fn take(&self) -> Vec<CommandSpec> {
        std::mem::take(&mut *self.specs.lock().unwrap())
    }

    pub fn fail_on(&self, program: &str) {
        *self.fail_program.lock().unwrap() = Some(program.to_string());
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, spec: &CommandSpec) -> Result<()> {
        self.specs.lock().unwrap().push(spec.clone());
        if self
            .fail_program
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|name| name == &spec.program.to_string_lossy())
        {
            bail!("forced failure");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn config() -> Config {
        Config {
            ssh_target: "me@example".to_string(),
            local_root: PathBuf::from("/local"),
            remote_root: PathBuf::from("/remote"),
            rpc_port: 12345,
            remote_bin_path: "~/.local/bin/sshpal-run".to_string(),
            tasks: BTreeMap::new(),
        }
    }

    #[test]
    fn builds_rsync_push_command() {
        let spec = rsync_command(
            &config(),
            &SyncPlan {
                direction: SyncDirection::Push,
                relative_path: PathBuf::from("foo"),
                local_path: PathBuf::from("/local/foo"),
                remote_path: PathBuf::from("/remote/foo"),
            },
        );
        assert_eq!(spec.program, OsString::from("rsync"));
        assert!(spec.args.iter().any(|a| a == "--delete"));
    }

    #[test]
    fn builds_rsync_pull_command() {
        let spec = rsync_command(
            &config(),
            &SyncPlan {
                direction: SyncDirection::Pull,
                relative_path: PathBuf::from("foo"),
                local_path: PathBuf::from("/local/foo"),
                remote_path: PathBuf::from("/remote/foo"),
            },
        );
        assert_eq!(spec.program, OsString::from("rsync"));
        assert_eq!(spec.args[4].to_string_lossy(), "me@example:/remote/foo");
        assert_eq!(spec.args[5].to_string_lossy(), "/local/foo");
    }

    #[test]
    fn builds_reverse_tunnel_command() {
        let spec = reverse_tunnel_command(&config());
        assert_eq!(spec.program, OsString::from("ssh"));
        assert!(spec.args.iter().any(|a| a == "-R"));
    }

    #[test]
    fn builds_install_commands() {
        let cfg = config();
        let prep = install_prepare_command(&cfg);
        let copy = install_copy_command(&cfg, Path::new("/tmp/bin").as_os_str());
        let fin = install_finalize_command(&cfg);
        assert_eq!(prep.program, OsString::from("ssh"));
        assert_eq!(copy.program, OsString::from("scp"));
        assert_eq!(fin.program, OsString::from("ssh"));
    }

    #[test]
    fn command_spec_builder_sets_args_and_cwd() {
        let spec = CommandSpec::new("echo")
            .arg("hello")
            .args(["world"])
            .cwd("/tmp");
        assert_eq!(spec.program, OsString::from("echo"));
        assert_eq!(
            spec.args
                .iter()
                .map(|a| a.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec!["hello", "world"]
        );
        assert_eq!(spec.cwd, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn recording_runner_can_fail_on_program() {
        let runner = RecordingRunner::default();
        runner.fail_on("scp");
        let err = runner
            .run(&CommandSpec::new("scp"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("forced failure"));
    }

    #[test]
    fn system_runner_reports_non_zero_exit() {
        let runner = SystemRunner;
        let err = runner
            .run(&CommandSpec::new("sh").args(["-c", "exit 7"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed with status 7"));
    }
}
