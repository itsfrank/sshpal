use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const CONFIG_FILE_NAME: &str = ".sshpal.toml";
pub const DEFAULT_RPC_PORT: u16 = 45_678;
pub const DEFAULT_REMOTE_BIN_PATH: &str = "~/.local/bin/sshpal-run";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub ssh_target: String,
    pub local_root: PathBuf,
    pub remote_root: PathBuf,
    pub rpc_port: u16,
    pub remote_bin_path: String,
    pub tasks: BTreeMap<String, Task>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RawConfig {
    ssh_target: String,
    local_root: Option<PathBuf>,
    remote_root: PathBuf,
    #[serde(default = "default_rpc_port")]
    rpc_port: u16,
    #[serde(default = "default_remote_bin_path")]
    remote_bin_path: String,
    #[serde(default)]
    tasks: BTreeMap<String, RawTask>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub steps: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum RawTask {
    Command(Vec<String>),
    Sequence(Vec<Vec<String>>),
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub path: PathBuf,
    pub project_root: PathBuf,
}

fn default_rpc_port() -> u16 {
    DEFAULT_RPC_PORT
}

fn default_remote_bin_path() -> String {
    DEFAULT_REMOTE_BIN_PATH.to_string()
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.ssh_target.trim().is_empty() {
            bail!("config ssh_target must not be empty");
        }
        if !self.local_root.is_absolute() {
            bail!("config local_root must be an absolute path");
        }
        if !self.remote_root.is_absolute() {
            bail!("config remote_root must be an absolute path");
        }
        if self.remote_bin_path.trim().is_empty() {
            bail!("config remote_bin_path must not be empty");
        }
        for (task, task_def) in &self.tasks {
            if task.trim().is_empty() {
                bail!("config task names must not be empty");
            }
            if task_def.steps.is_empty() {
                bail!("config task `{task}` must define at least one command");
            }
            for argv in &task_def.steps {
                if argv.is_empty() || argv[0].trim().is_empty() {
                    bail!("config task `{task}` must define non-empty command arrays");
                }
            }
        }
        Ok(())
    }
}

impl RawConfig {
    fn resolve(self, project_root: &Path) -> Config {
        Config {
            ssh_target: self.ssh_target,
            local_root: self
                .local_root
                .unwrap_or_else(|| project_root.to_path_buf()),
            remote_root: self.remote_root,
            rpc_port: self.rpc_port,
            remote_bin_path: self.remote_bin_path,
            tasks: self
                .tasks
                .into_iter()
                .map(|(name, task)| (name, task.resolve()))
                .collect(),
        }
    }
}

impl RawTask {
    fn resolve(self) -> Task {
        match self {
            Self::Command(argv) => Task { steps: vec![argv] },
            Self::Sequence(steps) => Task { steps },
        }
    }
}

pub fn discover_config(start_dir: &Path) -> Result<LoadedConfig> {
    let mut current = fs::canonicalize(start_dir)
        .with_context(|| format!("failed to canonicalize {}", start_dir.display()))?;
    loop {
        let candidate = current.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate)
                .with_context(|| format!("failed to read {}", candidate.display()))?;
            let raw_config: RawConfig = toml::from_str(&text)
                .with_context(|| format!("failed to parse {}", candidate.display()))?;
            let config = raw_config.resolve(&current);
            config.validate()?;
            return Ok(LoadedConfig {
                config,
                path: candidate,
                project_root: current,
            });
        }
        if !current.pop() {
            bail!("no {} found from {}", CONFIG_FILE_NAME, start_dir.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn sample_config() -> String {
        r#"
ssh_target = "me@example"
remote_root = "/remote/project"

[tasks]
test = ["cargo", "test"]
"#
        .to_string()
    }

    #[test]
    fn validates_required_fields() {
        let config = Config {
            ssh_target: String::new(),
            local_root: PathBuf::from("/tmp/local"),
            remote_root: PathBuf::from("/tmp/remote"),
            rpc_port: DEFAULT_RPC_PORT,
            remote_bin_path: DEFAULT_REMOTE_BIN_PATH.to_string(),
            tasks: BTreeMap::new(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn discovers_nearest_config() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let child = root.join("proj/sub");
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join(CONFIG_FILE_NAME), sample_config()).unwrap();
        fs::write(child.join(CONFIG_FILE_NAME), sample_config()).unwrap();

        let loaded = discover_config(&child).unwrap();
        assert_eq!(loaded.project_root, child.canonicalize().unwrap());
    }

    #[test]
    fn infers_local_root_from_config_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(CONFIG_FILE_NAME), sample_config()).unwrap();

        let loaded = discover_config(&root).unwrap();
        assert_eq!(loaded.config.local_root, root.canonicalize().unwrap());
    }

    #[test]
    fn errors_when_missing() {
        let dir = tempdir().unwrap();
        let err = discover_config(dir.path()).unwrap_err().to_string();
        assert!(err.contains(CONFIG_FILE_NAME));
    }

    #[test]
    fn minimal_example_config_stays_valid() {
        let raw: RawConfig =
            toml::from_str(include_str!("../examples/minimal.sshpal.toml")).unwrap();
        let project_root = Path::new("/tmp/example-project");
        let config = raw.resolve(project_root);
        config.validate().unwrap();
        assert_eq!(config.local_root, project_root);
        assert_eq!(config.remote_root, PathBuf::from("/work/project"));
        assert!(config.tasks.contains_key("test"));
        assert_eq!(config.rpc_port, DEFAULT_RPC_PORT);
        assert_eq!(config.remote_bin_path, DEFAULT_REMOTE_BIN_PATH);
        assert_eq!(
            config.tasks.get("test").unwrap(),
            &Task {
                steps: vec![vec!["bin/test".to_string()]]
            }
        );
    }

    #[test]
    fn complete_example_config_stays_valid() {
        let raw: RawConfig =
            toml::from_str(include_str!("../examples/complete.sshpal.toml")).unwrap();
        let project_root = Path::new("/tmp/example-project");
        let config = raw.resolve(project_root);
        config.validate().unwrap();
        assert_eq!(config.local_root, PathBuf::from("/tmp/local-worktree"));
        assert_eq!(config.remote_root, PathBuf::from("/work/project"));
        assert_eq!(config.rpc_port, 40_001);
        assert_eq!(config.remote_bin_path, "~/bin/sshpal-run-custom");
        assert_eq!(
            config.tasks.get("lint").unwrap(),
            &Task {
                steps: vec![vec![
                    "bin/lint".to_string(),
                    "--format".to_string(),
                    "json".to_string(),
                    "--strict".to_string()
                ]]
            }
        );
    }

    #[test]
    fn parses_sequential_task_steps() {
        let raw: RawConfig = toml::from_str(
            r#"
ssh_target = "me@example"
remote_root = "/remote/project"

[tasks]
check = [["cargo", "fmt", "--check"], ["cargo", "test"]]
"#,
        )
        .unwrap();

        let config = raw.resolve(Path::new("/tmp/example-project"));
        config.validate().unwrap();
        assert_eq!(
            config.tasks.get("check").unwrap(),
            &Task {
                steps: vec![
                    vec![
                        "cargo".to_string(),
                        "fmt".to_string(),
                        "--check".to_string()
                    ],
                    vec!["cargo".to_string(), "test".to_string()]
                ]
            }
        );
    }
}
