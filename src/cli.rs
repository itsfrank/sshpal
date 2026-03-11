use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::process::Command;

use crate::config::{RemoteArch, discover_config};
use crate::paths::{SyncDirection, build_sync_plan, relative_cwd};
use crate::process::{
    SharedRunner, SystemRunner, cargo_zigbuild_command, install_copy_command,
    install_finalize_command, install_prepare_command, reverse_tunnel_command, rsync_command,
};
use crate::rpc;

#[derive(Debug, Parser)]
#[command(name = "sshpal")]
#[command(about = "Sync files and proxy local-only tasks through SSH")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Push { path: PathBuf },
    Pull { path: PathBuf },
    Serve,
    OtherRun {
        task: String,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    InstallRemote {
        #[arg(long)]
        remote_arch: Option<RemoteArch>,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    run_with(cli, Arc::new(SystemRunner)).await
}

async fn run_with(cli: Cli, runner: SharedRunner) -> Result<()> {
    match cli.command {
        Commands::Push { path } => sync(path, SyncDirection::Push, runner),
        Commands::Pull { path } => sync(path, SyncDirection::Pull, runner),
        Commands::Serve => serve().await,
        Commands::OtherRun { task, args } => {
            let loaded = discover_config(&std::env::current_dir()?)?;
            let code = rpc::other_run(&loaded.config, task, args).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Commands::InstallRemote { remote_arch } => install_remote(remote_arch, runner).await,
    }
}

fn sync(path: PathBuf, direction: SyncDirection, runner: SharedRunner) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let loaded = discover_config(&cwd)?;
    let cwd_rel = relative_cwd(&loaded.config.local_root, &cwd)?;
    let plan = build_sync_plan(
        &loaded.config.local_root,
        &loaded.config.remote_root,
        &cwd_rel,
        &path,
        direction,
    )?;
    runner.run(&rsync_command(&loaded.config, &plan))
}

async fn serve() -> Result<()> {
    let loaded = discover_config(&std::env::current_dir()?)?;
    let mut tunnel = Command::new("ssh")
        .args(reverse_tunnel_command(&loaded.config).args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn reverse SSH tunnel")?;

    let server_result = rpc::serve(loaded.config).await;
    let _ = tunnel.kill().await;
    server_result
}

async fn install_remote(remote_arch: Option<RemoteArch>, runner: SharedRunner) -> Result<()> {
    let loaded = discover_config(&std::env::current_dir()?)?;
    let mut config = loaded.config;
    if let Some(arch) = remote_arch {
        config.remote_arch = arch;
    }

    let build_spec = cargo_zigbuild_command(&config.remote_arch);
    runner.run(&build_spec).with_context(|| {
        format!(
            "failed to build remote binary for {}; install cargo-zigbuild and Zig locally",
            config.remote_arch.target_triple()
        )
    })?;

    let artifact = artifact_path(&config.remote_arch);
    if !artifact.is_file() {
        bail!("expected built artifact at {}", artifact.display());
    }
    runner.run(&install_prepare_command(&config))?;
    runner.run(&install_copy_command(&config, artifact.as_os_str()))?;
    runner.run(&install_finalize_command(&config))?;
    Ok(())
}

fn artifact_path(arch: &RemoteArch) -> PathBuf {
    Path::new("target")
        .join(arch.target_triple())
        .join("release")
        .join(exe_name())
}

fn exe_name() -> &'static str {
    if cfg!(windows) { "sshpal.exe" } else { "sshpal" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::RecordingRunner;
    use serial_test::serial;
    use std::fs;
    use std::path::PathBuf as StdPathBuf;
    use tempfile::tempdir;

    fn write_config(dir: &Path) {
        fs::write(
            dir.join(".sshpal.toml"),
            r#"
ssh_target = "me@example"
remote_root = "/remote/proj"
remote_arch = "x86_64"
"#,
        )
        .unwrap();
    }

    struct CwdGuard {
        previous: StdPathBuf,
    }

    impl CwdGuard {
        fn change_to(path: &Path) -> Self {
            let previous = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self { previous }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.previous).unwrap();
        }
    }

    fn write_config_with_arch(dir: &Path, arch: &str) {
        fs::write(
            dir.join(".sshpal.toml"),
            format!(
                r#"
ssh_target = "me@example"
remote_root = "/remote/proj"
remote_arch = "{arch}"
"#
            ),
        )
        .unwrap();
    }

    fn create_artifact(root: &Path, arch: &RemoteArch) -> PathBuf {
        let artifact = root
            .join("target")
            .join(arch.target_triple())
            .join("release")
            .join(exe_name());
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "fake-binary").unwrap();
        artifact
    }

    #[tokio::test]
    #[serial]
    async fn sync_uses_rsync() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("proj");
        let sub = root.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        write_config(&root);
        let _guard = CwdGuard::change_to(&sub);
        let runner = Arc::new(RecordingRunner::default());
        let cli = Cli::parse_from(["sshpal", "push", "."]);
        run_with(cli, runner.clone()).await.unwrap();
        let specs = runner.take();
        assert_eq!(specs[0].program.to_string_lossy(), "rsync");
    }

    #[tokio::test]
    #[serial]
    async fn pull_uses_rsync() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("proj");
        let sub = root.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        write_config(&root);
        let _guard = CwdGuard::change_to(&sub);
        let runner = Arc::new(RecordingRunner::default());
        let cli = Cli::parse_from(["sshpal", "pull", "."]);
        run_with(cli, runner.clone()).await.unwrap();
        let specs = runner.take();
        assert_eq!(specs[0].program.to_string_lossy(), "rsync");
        assert!(specs[0].args[4].to_string_lossy().contains("me@example:/remote/proj/a/b"));
    }

    #[tokio::test]
    #[serial]
    async fn install_remote_runs_expected_command_sequence() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        write_config(&root);
        let _guard = CwdGuard::change_to(&root);
        create_artifact(&root, &RemoteArch::X86_64);

        let runner = Arc::new(RecordingRunner::default());
        let cli = Cli::parse_from(["sshpal", "install-remote"]);
        run_with(cli, runner.clone()).await.unwrap();

        let specs = runner.take();
        assert_eq!(specs.len(), 4);
        assert_eq!(specs[0].program.to_string_lossy(), "cargo");
        assert_eq!(
            specs[0].args.iter().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>(),
            vec!["zigbuild", "--release", "--target", "x86_64-unknown-linux-musl"]
        );
        assert_eq!(specs[1].program.to_string_lossy(), "ssh");
        assert_eq!(specs[2].program.to_string_lossy(), "scp");
        assert_eq!(
            specs[2].args[0].to_string_lossy(),
            "target/x86_64-unknown-linux-musl/release/sshpal"
        );
        assert_eq!(
            specs[2].args[1].to_string_lossy(),
            "me@example:~/.local/bin/sshpal.tmp"
        );
        assert_eq!(specs[3].program.to_string_lossy(), "ssh");
    }

    #[tokio::test]
    #[serial]
    async fn install_remote_honors_cli_arch_override() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        write_config_with_arch(&root, "x86_64");
        let _guard = CwdGuard::change_to(&root);
        create_artifact(&root, &RemoteArch::Aarch64);

        let runner = Arc::new(RecordingRunner::default());
        let cli = Cli::parse_from(["sshpal", "install-remote", "--remote-arch", "aarch64"]);
        run_with(cli, runner.clone()).await.unwrap();

        let specs = runner.take();
        assert_eq!(
            specs[0].args.iter().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>(),
            vec!["zigbuild", "--release", "--target", "aarch64-unknown-linux-musl"]
        );
        assert!(
            specs[2].args[0]
                .to_string_lossy()
                .contains("target/aarch64-unknown-linux-musl/release/sshpal")
        );
    }

    #[tokio::test]
    #[serial]
    async fn install_remote_errors_when_artifact_is_missing() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        write_config(&root);
        let _guard = CwdGuard::change_to(&root);

        let runner = Arc::new(RecordingRunner::default());
        let cli = Cli::parse_from(["sshpal", "install-remote"]);
        let err = run_with(cli, runner).await.unwrap_err().to_string();

        assert!(err.contains("expected built artifact"));
    }

    #[tokio::test]
    #[serial]
    async fn install_remote_propagates_subprocess_failures() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        write_config(&root);
        let _guard = CwdGuard::change_to(&root);
        create_artifact(&root, &RemoteArch::X86_64);

        let runner = Arc::new(RecordingRunner::default());
        runner.fail_on("cargo");
        let cli = Cli::parse_from(["sshpal", "install-remote"]);
        let err = run_with(cli, runner).await.unwrap_err().to_string();

        assert!(err.contains("failed to build remote binary"));
    }

    #[test]
    fn artifact_path_matches_target() {
        assert_eq!(
            artifact_path(&RemoteArch::Aarch64),
            Path::new("target")
                .join("aarch64-unknown-linux-musl")
                .join("release")
                .join(exe_name())
        );
    }

    #[test]
    fn remote_arch_cli_override_parses() {
        let cli = Cli::try_parse_from(["sshpal", "install-remote", "--remote-arch", "aarch64"]);
        assert!(cli.is_ok());
    }
}
