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
    use std::fs;
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

    #[tokio::test]
    async fn sync_uses_rsync() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("proj");
        let sub = root.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        write_config(&root);
        std::env::set_current_dir(&sub).unwrap();
        let runner = Arc::new(RecordingRunner::default());
        let cli = Cli::parse_from(["sshpal", "push", "."]);
        run_with(cli, runner.clone()).await.unwrap();
        let specs = runner.take();
        assert_eq!(specs[0].program.to_string_lossy(), "rsync");
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
