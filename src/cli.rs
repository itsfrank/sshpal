use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::{fs, time::SystemTime};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::process::Command;

use crate::config::discover_config;
use crate::paths::{SyncDirection, build_sync_plan, relative_cwd};
use crate::process::{
    SharedRunner, SystemRunner, install_copy_command, install_finalize_command,
    install_prepare_command, reverse_tunnel_command, rsync_command,
};
use crate::rpc;
use crate::tasks;

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
    Run {
        task: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    TasksHelp,
}

pub async fn run() -> Result<i32> {
    let cli = Cli::parse();
    run_with(cli, Arc::new(SystemRunner)).await
}

async fn run_with(cli: Cli, runner: SharedRunner) -> Result<i32> {
    match cli.command {
        Commands::Push { path } => sync(path, SyncDirection::Push, runner),
        Commands::Pull { path } => sync(path, SyncDirection::Pull, runner),
        Commands::Serve => serve_with(runner).await,
        Commands::Run { task, args } => run_task(task, args).await,
        Commands::TasksHelp => tasks_help(),
    }
}

fn sync(path: PathBuf, direction: SyncDirection, runner: SharedRunner) -> Result<i32> {
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
    runner.run(&rsync_command(&loaded.config, &plan))?;
    Ok(0)
}

async fn serve_with(runner: SharedRunner) -> Result<i32> {
    let loaded = discover_config(&std::env::current_dir()?)?;
    eprintln!(
        "sshpal: installing remote helper to {} on {}",
        loaded.config.remote_bin_path, loaded.config.ssh_target
    );
    install_remote_helper(&loaded.config, runner)?;

    let mut tunnel = Command::new("ssh")
        .args(reverse_tunnel_command(&loaded.config).args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn reverse SSH tunnel")?;
    eprintln!(
        "sshpal: reverse tunnel started for 127.0.0.1:{}",
        loaded.config.rpc_port
    );

    let server_result = rpc::serve(loaded.config).await;
    let _ = tunnel.kill().await;
    server_result?;
    Ok(0)
}

fn install_remote_helper(config: &crate::config::Config, runner: SharedRunner) -> Result<()> {
    let local_script = write_local_helper_script(rpc::remote_helper_script(config.rpc_port))?;
    let path = local_script.as_os_str();

    let result = (|| {
        runner.run(&install_prepare_command(config))?;
        runner.run(&install_copy_command(config, path))?;
        runner.run(&install_finalize_command(config))?;
        Ok(())
    })();

    let _ = fs::remove_file(&local_script);
    result
}

fn write_local_helper_script(contents: String) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("sshpal-run-{nanos}.tmp"));
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn tasks_help() -> Result<i32> {
    let loaded = discover_config(&std::env::current_dir()?)?;
    println!("{}", tasks::task_help("sshpal run", &loaded.config.tasks)?);
    Ok(0)
}

async fn run_task(task_name: String, args: Vec<String>) -> Result<i32> {
    let loaded = discover_config(&std::env::current_dir()?)?;
    let invocation = tasks::parse_invocation_args(&args)?;
    let task = loaded
        .config
        .tasks
        .get(&task_name)
        .with_context(|| format!("unknown task `{task_name}`"))?;
    let prepared = tasks::prepare_task(
        &task_name,
        task,
        &invocation.vars,
        &invocation.forwarded_args,
    )?;

    let mut exit_code = 0;
    for step in prepared.steps {
        let Some(program) = step.first() else {
            return Ok(1);
        };
        let status = Command::new(program)
            .args(step.iter().skip(1))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .with_context(|| format!("failed to spawn task `{task_name}`"))?;
        exit_code = status.code().unwrap_or(1);
        if exit_code != 0 {
            break;
        }
    }

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::RecordingRunner;
    use serial_test::serial;
    use std::path::Path;
    use std::path::PathBuf as StdPathBuf;
    use tempfile::tempdir;

    fn write_config(dir: &Path) {
        fs::write(
            dir.join(".sshpal.toml"),
            r#"
ssh_target = "me@example"
remote_root = "/remote/proj"
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
        assert_eq!(run_with(cli, runner.clone()).await.unwrap(), 0);
        let specs = runner.take();
        assert_eq!(specs[0].program.to_string_lossy(), "rsync");
        assert!(specs[0].args[4].to_string_lossy().ends_with("/proj/a/b/"));
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
        assert_eq!(run_with(cli, runner.clone()).await.unwrap(), 0);
        let specs = runner.take();
        assert_eq!(specs[0].program.to_string_lossy(), "rsync");
        assert!(
            specs[0].args[4]
                .to_string_lossy()
                .contains("me@example:/remote/proj/a/b/")
        );
    }

    #[test]
    #[serial]
    fn install_remote_helper_runs_expected_command_sequence() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        write_config(&root);
        let _guard = CwdGuard::change_to(&root);
        let loaded = discover_config(&root).unwrap();

        let runner = Arc::new(RecordingRunner::default());
        install_remote_helper(&loaded.config, runner.clone()).unwrap();

        let specs = runner.take();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].program.to_string_lossy(), "ssh");
        assert_eq!(specs[1].program.to_string_lossy(), "scp");
        assert!(specs[1].args[0].to_string_lossy().contains("sshpal-run-"));
        assert_eq!(
            specs[1].args[1].to_string_lossy(),
            "me@example:/tmp/sshpal-run-45678.tmp"
        );
        assert_eq!(specs[2].program.to_string_lossy(), "ssh");
    }

    #[test]
    fn write_local_helper_script_writes_expected_content() {
        let path = write_local_helper_script("echo test\n".to_string()).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "echo test\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn run_subcommand_keeps_var_and_forwarded_args() {
        let cli = Cli::parse_from([
            "sshpal",
            "run",
            "build",
            "crate=my crate",
            "--",
            "--nocapture",
        ]);

        match cli.command {
            Commands::Run { task, args } => {
                assert_eq!(task, "build");
                assert_eq!(
                    args,
                    vec![
                        "crate=my crate".to_string(),
                        "--".to_string(),
                        "--nocapture".to_string()
                    ]
                );
            }
            _ => panic!("expected run command"),
        }
    }
}
