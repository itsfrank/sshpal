use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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
    pub run: TaskRun,
    pub description: Option<String>,
    pub vars: BTreeMap<String, TaskVar>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum RawTask {
    String(String),
    Command(Vec<String>),
    Sequence(Vec<Vec<String>>),
    Detailed(RawDetailedTask),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRun {
    String(String),
    Command(Vec<String>),
    Sequence(Vec<Vec<String>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskVar {
    pub description: Option<String>,
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RawDetailedTask {
    run: RawTaskRun,
    description: Option<String>,
    #[serde(default)]
    vars: BTreeMap<String, RawTaskVar>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum RawTaskRun {
    String(String),
    Command(Vec<String>),
    Sequence(Vec<Vec<String>>),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RawTaskVar {
    description: Option<String>,
    #[serde(default)]
    optional: bool,
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
            if task == "tasks-help" {
                bail!("config task name `tasks-help` is reserved");
            }
            task_def.validate(task)?;
        }
        Ok(())
    }
}

impl Task {
    fn validate(&self, task_name: &str) -> Result<()> {
        match &self.run {
            TaskRun::String(command) => {
                if command.trim().is_empty() {
                    bail!("config task `{task_name}` must define a non-empty command string");
                }
                validate_template(command)
                    .with_context(|| format!("config task `{task_name}` has invalid run string"))?;
            }
            TaskRun::Command(argv) => validate_argv(task_name, argv)?,
            TaskRun::Sequence(steps) => {
                if steps.is_empty() {
                    bail!("config task `{task_name}` must define at least one command");
                }
                for argv in steps {
                    validate_argv(task_name, argv)?;
                }
            }
        }

        for (name, var) in &self.vars {
            if name.trim().is_empty() {
                bail!("config task `{task_name}` has an empty documented var name");
            }
            if !is_valid_name(name) {
                bail!(
                    "config task `{task_name}` documents invalid var `{name}`; names must match [A-Za-z_][A-Za-z0-9_]*"
                );
            }
            if let Some(description) = &var.description {
                if description.trim().is_empty() {
                    bail!("config task `{task_name}` var `{name}` description must not be empty");
                }
            }
        }

        let referenced = self.referenced_client_vars()?;
        for documented in self.vars.keys() {
            if !referenced.contains(documented) {
                bail!("config task `{task_name}` documents var `{documented}` but never references it in run");
            }
        }
        Ok(())
    }

    pub fn referenced_client_vars(&self) -> Result<Vec<String>> {
        self.referenced_placeholders(|placeholder| match placeholder {
            Placeholder::Client(name) => Some(name),
            Placeholder::Env(_) => None,
        })
    }

    pub fn referenced_env_vars(&self) -> Result<Vec<String>> {
        self.referenced_placeholders(|placeholder| match placeholder {
            Placeholder::Client(_) => None,
            Placeholder::Env(name) => Some(name),
        })
    }

    fn visit_templates<F>(&self, mut visit: F) -> Result<()>
    where
        F: FnMut(&str) -> Result<()>,
    {
        match &self.run {
            TaskRun::String(command) => visit(command)?,
            TaskRun::Command(argv) => {
                for arg in argv {
                    visit(arg)?;
                }
            }
            TaskRun::Sequence(steps) => {
                for step in steps {
                    for arg in step {
                        visit(arg)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn steps(&self) -> Vec<Vec<String>> {
        match &self.run {
            TaskRun::String(command) => vec![vec![command.clone()]],
            TaskRun::Command(argv) => vec![argv.clone()],
            TaskRun::Sequence(steps) => steps.clone(),
        }
    }

    fn referenced_placeholders<F>(&self, mut pick: F) -> Result<Vec<String>>
    where
        F: FnMut(Placeholder) -> Option<String>,
    {
        let mut names = Vec::new();
        self.visit_templates(|template| {
            let refs = parse_template(template)?;
            for reference in refs {
                if let TemplatePart::Placeholder(placeholder) = reference {
                    if let Some(name) = pick(placeholder) {
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                }
            }
            Ok(())
        })?;
        Ok(names)
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
            Self::String(command) => Task {
                run: TaskRun::String(command),
                description: None,
                vars: BTreeMap::new(),
            },
            Self::Command(argv) => Task {
                run: TaskRun::Command(argv),
                description: None,
                vars: BTreeMap::new(),
            },
            Self::Sequence(steps) => Task {
                run: TaskRun::Sequence(steps),
                description: None,
                vars: BTreeMap::new(),
            },
            Self::Detailed(task) => task.resolve(),
        }
    }
}

impl RawDetailedTask {
    fn resolve(self) -> Task {
        Task {
            run: self.run.resolve(),
            description: self.description,
            vars: self
                .vars
                .into_iter()
                .map(|(name, var)| (name, var.resolve()))
                .collect(),
        }
    }
}

impl RawTaskRun {
    fn resolve(self) -> TaskRun {
        match self {
            Self::String(command) => TaskRun::String(command),
            Self::Command(argv) => TaskRun::Command(argv),
            Self::Sequence(steps) => TaskRun::Sequence(steps),
        }
    }
}

impl RawTaskVar {
    fn resolve(self) -> TaskVar {
        TaskVar {
            description: self.description,
            optional: self.optional,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TemplatePart {
    Literal(String),
    Placeholder(Placeholder),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Placeholder {
    Client(String),
    Env(String),
}

fn validate_argv(task_name: &str, argv: &[String]) -> Result<()> {
    if argv.is_empty() {
        bail!("config task `{task_name}` must define at least one command");
    }
    if argv[0].trim().is_empty() {
        bail!("config task `{task_name}` must define non-empty command arrays");
    }
    for arg in argv {
        validate_template(arg)
            .with_context(|| format!("config task `{task_name}` has invalid argv template"))?;
    }
    Ok(())
}

fn validate_template(template: &str) -> Result<()> {
    let _ = parse_template(template)?;
    Ok(())
}

pub(crate) fn parse_template(template: &str) -> Result<Vec<TemplatePart>> {
    let chars = template.chars().collect::<Vec<_>>();
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut index = 0;

    while index < chars.len() {
        match chars[index] {
            '{' => {
                if let Some(next) = chars.get(index + 1) {
                    match next {
                        '{' => {
                            literal.push('{');
                            index += 2;
                        }
                        '#' | '$' => {
                            if !literal.is_empty() {
                                parts.push(TemplatePart::Literal(std::mem::take(&mut literal)));
                            }
                            let kind = *next;
                            index += 2;
                            let start = index;
                            while index < chars.len() && chars[index] != '}' {
                                index += 1;
                            }
                            if index >= chars.len() {
                                bail!("unterminated placeholder in `{template}`");
                            }
                            let name = chars[start..index].iter().collect::<String>();
                            if !is_valid_name(&name) {
                                bail!(
                                    "invalid placeholder name `{name}` in `{template}`; names must match [A-Za-z_][A-Za-z0-9_]*"
                                );
                            }
                            let placeholder = if kind == '#' {
                                Placeholder::Client(name)
                            } else {
                                Placeholder::Env(name)
                            };
                            parts.push(TemplatePart::Placeholder(placeholder));
                            index += 1;
                        }
                        _ => bail!("invalid placeholder start in `{template}`; use `{{`, `{{#name}}`, or `{{$NAME}}`")
                    }
                } else {
                    bail!("unterminated `{{` in `{template}`");
                }
            }
            '}' => {
                if chars.get(index + 1) == Some(&'}') {
                    literal.push('}');
                    index += 2;
                } else {
                    bail!("unescaped `}}` in `{template}`; use `}}}}` for a literal closing brace");
                }
            }
            ch => {
                literal.push(ch);
                index += 1;
            }
        }
    }

    if !literal.is_empty() {
        parts.push(TemplatePart::Literal(literal));
    }
    Ok(parts)
}

fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
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
                run: TaskRun::Command(vec!["bin/test".to_string()]),
                description: None,
                vars: BTreeMap::new(),
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
                run: TaskRun::Command(vec![
                    "bin/lint".to_string(),
                    "--format".to_string(),
                    "json".to_string(),
                    "--strict".to_string()
                ]),
                description: None,
                vars: BTreeMap::new(),
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
                run: TaskRun::Sequence(vec![
                    vec![
                        "cargo".to_string(),
                        "fmt".to_string(),
                        "--check".to_string()
                    ],
                    vec!["cargo".to_string(), "test".to_string()]
                ]),
                description: None,
                vars: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn parses_structured_task_with_docs_and_vars() {
        let raw: RawConfig = toml::from_str(
            r#"
ssh_target = "me@example"
remote_root = "/remote/project"

[tasks.build]
run = "cargo build --package '{#crate}'"
description = "Build one package"

[tasks.build.vars.crate]
description = "Cargo package name"
"#,
        )
        .unwrap();

        let config = raw.resolve(Path::new("/tmp/example-project"));
        config.validate().unwrap();
        assert_eq!(
            config.tasks.get("build").unwrap(),
            &Task {
                run: TaskRun::String("cargo build --package '{#crate}'".to_string()),
                description: Some("Build one package".to_string()),
                vars: BTreeMap::from([(
                    "crate".to_string(),
                    TaskVar {
                        description: Some("Cargo package name".to_string()),
                        optional: false,
                    },
                )]),
            }
        );
    }

    #[test]
    fn rejects_unused_documented_vars() {
        let raw: RawConfig = toml::from_str(
            r#"
ssh_target = "me@example"
remote_root = "/remote/project"

[tasks.build]
run = ["cargo", "build"]

[tasks.build.vars.crate]
description = "Cargo package name"
"#,
        )
        .unwrap();

        let config = raw.resolve(Path::new("/tmp/example-project"));
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("documents var `crate` but never references it"));
    }

    #[test]
    fn accepts_undocumented_referenced_vars() {
        let raw: RawConfig = toml::from_str(
            r#"
ssh_target = "me@example"
remote_root = "/remote/project"

[tasks.build]
run = ["cargo", "build", "--package", "{#crate}"]
"#,
        )
        .unwrap();

        let config = raw.resolve(Path::new("/tmp/example-project"));
        config.validate().unwrap();
        assert_eq!(
            config
                .tasks
                .get("build")
                .unwrap()
                .referenced_client_vars()
                .unwrap(),
            vec!["crate".to_string()]
        );
    }

    #[test]
    fn rejects_malformed_placeholders() {
        let raw: RawConfig = toml::from_str(
            r#"
ssh_target = "me@example"
remote_root = "/remote/project"

[tasks.build]
run = "cargo build {crate}"
"#,
        )
        .unwrap();

        let config = raw.resolve(Path::new("/tmp/example-project"));
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("invalid run string"));
    }

    #[test]
    fn rejects_reserved_task_name() {
        let raw: RawConfig = toml::from_str(
            r#"
ssh_target = "me@example"
remote_root = "/remote/project"

[tasks]
tasks-help = ["printf", "nope"]
"#,
        )
        .unwrap();

        let config = raw.resolve(Path::new("/tmp/example-project"));
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("reserved"));
    }
}
