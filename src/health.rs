use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::config::LoadedConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthReport {
    pub ok: bool,
    pub text: String,
}

pub fn checkhealth(loaded: &LoadedConfig) -> Result<HealthReport> {
    let mut ok = true;
    let mut lines = Vec::new();

    lines.push(format!("config: ok ({})", loaded.path.display()));
    lines.push(format!(
        "project root: ok ({})",
        loaded.project_root.display()
    ));

    if loaded.config.local_root.is_dir() {
        lines.push(format!(
            "local_root: ok ({})",
            loaded.config.local_root.display()
        ));
    } else {
        ok = false;
        lines.push(format!(
            "local_root: fail ({}) does not exist or is not a directory",
            loaded.config.local_root.display()
        ));
    }

    for tool in ["ssh", "scp", "rsync"] {
        if local_command_exists(tool) {
            lines.push(format!("local tool `{tool}`: ok"));
        } else {
            ok = false;
            lines.push(format!("local tool `{tool}`: fail (not found in PATH)"));
        }
    }

    match TcpListener::bind(("127.0.0.1", loaded.config.rpc_port)) {
        Ok(listener) => {
            drop(listener);
            lines.push(format!(
                "rpc port {}: ok (available)",
                loaded.config.rpc_port
            ));
        }
        Err(err) => {
            ok = false;
            lines.push(format!("rpc port {}: fail ({err})", loaded.config.rpc_port));
        }
    }

    for (name, task) in &loaded.config.tasks {
        let cwd = task
            .cwd
            .as_ref()
            .map(|value| loaded.config.local_root.join(value))
            .unwrap_or_else(|| loaded.config.local_root.clone());
        if cwd.is_dir() {
            lines.push(format!("task `{name}` cwd: ok ({})", cwd.display()));
        } else {
            ok = false;
            lines.push(format!(
                "task `{name}` cwd: fail ({}) does not exist or is not a directory",
                cwd.display()
            ));
        }
    }

    match remote_prereq_probe(&loaded.config.ssh_target) {
        Ok(true) => lines.push(format!(
            "remote ssh/prereqs: ok ({})",
            loaded.config.ssh_target
        )),
        Ok(false) => {
            ok = false;
            lines.push(format!(
                "remote ssh/prereqs: fail ({}) missing `/bin/sh`, `curl`, or `jq`",
                loaded.config.ssh_target
            ));
        }
        Err(err) => {
            ok = false;
            lines.push(format!(
                "remote ssh/prereqs: fail ({}) {err}",
                loaded.config.ssh_target
            ));
        }
    }

    Ok(HealthReport {
        ok,
        text: lines.join("\n"),
    })
}

fn local_command_exists(name: &str) -> bool {
    Command::new("sh")
        .args([
            "-lc",
            &format!("command -v {} >/dev/null 2>&1", shell_word(name)),
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn remote_prereq_probe(target: &str) -> Result<bool> {
    let status = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            target,
            "sh -lc '[ -x /bin/sh ] && command -v curl >/dev/null 2>&1 && command -v jq >/dev/null 2>&1'",
        ])
        .status()?;
    Ok(status.success())
}

fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn sentinel_path(local_root: &Path) -> std::path::PathBuf {
    local_root.join(".sshpal").join("sync-token")
}
