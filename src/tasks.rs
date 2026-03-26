use std::collections::BTreeMap;
use std::env;

use anyhow::{bail, Context, Result};

use crate::config::{parse_template, Placeholder, Task, TaskRun, TemplatePart};

pub const TASKS_HELP_NAME: &str = "tasks-help";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTask {
    pub steps: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentedVar {
    pub name: String,
    pub description: Option<String>,
    pub optional: bool,
    pub documented: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationArgs {
    pub vars: BTreeMap<String, String>,
    pub forwarded_args: Vec<String>,
}

pub fn parse_invocation_args(args: &[String]) -> Result<InvocationArgs> {
    let mut vars = BTreeMap::new();
    let mut forwarded_args = Vec::new();
    let mut after_separator = false;

    for arg in args {
        if after_separator {
            forwarded_args.push(arg.clone());
            continue;
        }
        if arg == "--" {
            after_separator = true;
            continue;
        }
        let Some((name, value)) = arg.split_once('=') else {
            bail!("invalid task argument `{arg}`; use name=value before `--`");
        };
        if !is_valid_invocation_name(name) {
            bail!("invalid task variable `{name}`; names must match [A-Za-z_][A-Za-z0-9_]*");
        }
        vars.insert(name.to_string(), value.to_string());
    }

    Ok(InvocationArgs {
        vars,
        forwarded_args,
    })
}

pub fn prepare_task(
    task_name: &str,
    task: &Task,
    vars: &BTreeMap<String, String>,
    forwarded_args: &[String],
) -> Result<PreparedTask> {
    validate_client_vars(task_name, task, vars)?;

    let steps = match &task.run {
        TaskRun::String(command) => vec![split_string_run(
            task_name,
            &render_template(task_name, command, task, vars)?,
        )?],
        TaskRun::Command(argv) => vec![render_argv(task_name, argv, task, vars)?],
        TaskRun::Sequence(steps) => steps
            .iter()
            .map(|argv| render_argv(task_name, argv, task, vars))
            .collect::<Result<Vec<_>>>()?,
    };

    let mut steps = steps;
    if let Some(last) = steps.last_mut() {
        last.extend(forwarded_args.iter().cloned());
    }

    Ok(PreparedTask { steps })
}

pub fn task_help(command_name: &str, tasks: &BTreeMap<String, Task>) -> Result<String> {
    let mut lines = Vec::new();
    for (index, (name, task)) in tasks.iter().enumerate() {
        if index > 0 {
            lines.push(String::new());
        }
        lines.push(name.clone());
        if let Some(description) = &task.description {
            lines.push(format!("  {description}"));
        }
        lines.push(format!(
            "  usage: {command_name} {name}{} [-- <args...>]",
            usage_var_suffix(task)?
        ));
        lines.push(format!("  run: {}", run_preview(task)));

        let vars = documented_vars(task)?;
        if !vars.is_empty() {
            lines.push("  vars:".to_string());
            for var in vars {
                let detail = match (&var.description, var.optional, var.documented) {
                    (Some(description), false, true) => format!("{description} (required)"),
                    (Some(description), true, true) => format!("{description} (optional)"),
                    (None, false, true) => "required".to_string(),
                    (None, true, true) => "optional".to_string(),
                    (_, false, false) => "undocumented (required, inferred)".to_string(),
                    (_, true, false) => "undocumented (optional, inferred)".to_string(),
                };
                lines.push(format!("    {}=<value>  {}", var.name, detail));
            }
        }
    }

    if lines.is_empty() {
        lines.push("no tasks defined".to_string());
    }

    Ok(lines.join("\n"))
}

pub fn documented_vars(task: &Task) -> Result<Vec<DocumentedVar>> {
    let mut vars = task
        .vars
        .iter()
        .map(|(name, var)| DocumentedVar {
            name: name.clone(),
            description: var.description.clone(),
            optional: var.optional,
            documented: true,
        })
        .collect::<Vec<_>>();

    for name in task.referenced_client_vars()? {
        if !task.vars.contains_key(&name) {
            vars.push(DocumentedVar {
                name,
                description: None,
                optional: false,
                documented: false,
            });
        }
    }
    vars.sort_by(|left, right| {
        left.optional
            .cmp(&right.optional)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(vars)
}

fn validate_client_vars(
    task_name: &str,
    task: &Task,
    vars: &BTreeMap<String, String>,
) -> Result<()> {
    for documented in documented_vars(task)? {
        if documented.optional || vars.contains_key(&documented.name) {
            continue;
        }
        bail!("task `{task_name}` requires var `{}`", documented.name);
    }

    for env_name in task.referenced_env_vars()? {
        env::var(&env_name)
            .with_context(|| format!("task `{task_name}` requires env var `{env_name}`"))?;
    }
    Ok(())
}

fn render_argv(
    task_name: &str,
    argv: &[String],
    task: &Task,
    vars: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    argv.iter()
        .map(|arg| render_template(task_name, arg, task, vars))
        .collect()
}

fn render_template(
    task_name: &str,
    template: &str,
    task: &Task,
    vars: &BTreeMap<String, String>,
) -> Result<String> {
    let parts = parse_template(template)
        .with_context(|| format!("task `{task_name}` has invalid template"))?;
    let mut rendered = String::new();
    for part in parts {
        match part {
            TemplatePart::Literal(literal) => rendered.push_str(&literal),
            TemplatePart::Placeholder(Placeholder::Client(name)) => {
                if let Some(value) = vars.get(&name) {
                    rendered.push_str(value);
                } else if task.vars.get(&name).is_some_and(|var| var.optional) {
                } else {
                    bail!("task `{task_name}` requires var `{name}`")
                }
            }
            TemplatePart::Placeholder(Placeholder::Env(name)) => {
                let value = env::var(&name)
                    .with_context(|| format!("task `{task_name}` requires env var `{name}`"))?;
                rendered.push_str(&value);
            }
        }
    }
    Ok(rendered)
}

fn split_string_run(task_name: &str, command: &str) -> Result<Vec<String>> {
    let argv = shell_words::split(command)
        .with_context(|| format!("task `{task_name}` produced an invalid command string"))?;
    if argv.is_empty() {
        bail!("task `{task_name}` produced an empty command string");
    }
    Ok(argv)
}

fn run_preview(task: &Task) -> String {
    match &task.run {
        TaskRun::String(command) => command.clone(),
        TaskRun::Command(argv) => argv_preview(argv),
        TaskRun::Sequence(steps) => steps
            .iter()
            .map(|step| argv_preview(step))
            .collect::<Vec<_>>()
            .join(" && "),
    }
}

fn argv_preview(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| shell_words::quote(arg).to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn usage_var_suffix(task: &Task) -> Result<String> {
    let vars = documented_vars(task)?;
    let suffix = vars
        .into_iter()
        .map(|var| {
            if var.optional {
                format!(" [{}=<value>]", var.name)
            } else {
                format!(" {}=<value>", var.name)
            }
        })
        .collect::<String>();
    Ok(suffix)
}

fn is_valid_invocation_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(run: TaskRun) -> Task {
        Task {
            run,
            description: Some("Example task".to_string()),
            vars: BTreeMap::new(),
        }
    }

    #[test]
    fn prepares_argv_task_with_substitution_and_forwarded_args() {
        let mut vars = BTreeMap::new();
        vars.insert("crate".to_string(), "sshpal".to_string());
        let task = Task {
            run: TaskRun::Command(vec![
                "cargo".to_string(),
                "test".to_string(),
                "--package".to_string(),
                "{#crate}".to_string(),
            ]),
            description: None,
            vars: BTreeMap::from([(
                "crate".to_string(),
                crate::config::TaskVar {
                    description: None,
                    optional: false,
                },
            )]),
        };

        let prepared = prepare_task("test", &task, &vars, &["--nocapture".to_string()]).unwrap();

        assert_eq!(
            prepared.steps,
            vec![vec![
                "cargo".to_string(),
                "test".to_string(),
                "--package".to_string(),
                "sshpal".to_string(),
                "--nocapture".to_string(),
            ]]
        );
    }

    #[test]
    fn prepares_string_task_by_splitting_after_substitution() {
        let mut vars = BTreeMap::new();
        vars.insert("crate".to_string(), "my crate".to_string());
        let task = Task {
            run: TaskRun::String("cargo test --package '{#crate}'".to_string()),
            description: None,
            vars: BTreeMap::from([(
                "crate".to_string(),
                crate::config::TaskVar {
                    description: None,
                    optional: false,
                },
            )]),
        };

        let prepared = prepare_task("test", &task, &vars, &[]).unwrap();
        assert_eq!(
            prepared.steps,
            vec![vec![
                "cargo".to_string(),
                "test".to_string(),
                "--package".to_string(),
                "my crate".to_string(),
            ]]
        );
    }

    #[test]
    fn optional_vars_expand_to_empty_strings() {
        let task = Task {
            run: TaskRun::Command(vec!["cmd".to_string(), "{#label}".to_string()]),
            description: None,
            vars: BTreeMap::from([(
                "label".to_string(),
                crate::config::TaskVar {
                    description: None,
                    optional: true,
                },
            )]),
        };

        let prepared = prepare_task("test", &task, &BTreeMap::new(), &[]).unwrap();
        assert_eq!(prepared.steps, vec![vec!["cmd".to_string(), String::new()]]);
    }

    #[test]
    fn help_includes_documented_and_inferred_vars() {
        let task = Task {
            run: TaskRun::Command(vec![
                "cargo".to_string(),
                "build".to_string(),
                "--package".to_string(),
                "{#crate}".to_string(),
                "--target".to_string(),
                "{#target}".to_string(),
            ]),
            description: Some("Build one crate".to_string()),
            vars: BTreeMap::from([(
                "crate".to_string(),
                crate::config::TaskVar {
                    description: Some("Cargo package name".to_string()),
                    optional: false,
                },
            )]),
        };
        let tasks = BTreeMap::from([("build".to_string(), task)]);

        let output = task_help("sshpal-run", &tasks).unwrap();
        assert!(
            output.contains("usage: sshpal-run build crate=<value> target=<value> [-- <args...>]")
        );
        assert!(output.contains("crate=<value>  Cargo package name (required)"));
        assert!(output.contains("target=<value>  undocumented (required, inferred)"));
    }

    #[test]
    fn help_reports_no_tasks() {
        assert_eq!(
            task_help("sshpal-run", &BTreeMap::new()).unwrap(),
            "no tasks defined"
        );
    }

    #[test]
    fn quoted_preview_is_shell_like() {
        let preview = run_preview(&task(TaskRun::Command(vec![
            "cargo".to_string(),
            "test package".to_string(),
        ])));
        assert_eq!(preview, "cargo 'test package'");
    }

    #[test]
    fn parses_vars_and_forwarded_args() {
        let args = vec![
            "crate=sshpal".to_string(),
            "message=hello world".to_string(),
            "--".to_string(),
            "--nocapture".to_string(),
        ];

        let parsed = parse_invocation_args(&args).unwrap();
        assert_eq!(
            parsed.vars,
            BTreeMap::from([
                ("crate".to_string(), "sshpal".to_string()),
                ("message".to_string(), "hello world".to_string()),
            ])
        );
        assert_eq!(parsed.forwarded_args, vec!["--nocapture".to_string()]);
    }

    #[test]
    fn rejects_invalid_pre_separator_args() {
        let err = parse_invocation_args(&["not-a-var".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("use name=value before `--`"));
    }
}
