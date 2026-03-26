use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;

fn write_task_config(dir: &std::path::Path) {
    fs::write(
        dir.join(".sshpal.toml"),
        r#"
ssh_target = "me@example"
remote_root = "/remote/project"

[tasks.render]
run = ["sh", "-c", "printf '%s|%s|%s\n' \"$0\" \"$1\" \"$2\"", "{#name}", "{#mode}"]
description = "Render a formatted line"

[tasks.render.vars.name]
description = "Value to print"

[tasks.render.vars.mode]
description = "Rendering mode"
optional = true

[tasks.quick]
run = "printf 'quick task\\n'"
description = "Run a quick command"

[tasks.cwd_env]
run = ["sh", "-c", "printf '%s|%s' \"$PWD\" \"$SPECIAL\""]
cwd = "."
timeout = "5s"

[tasks.cwd_env.env]
SPECIAL = "hello-env"
"#,
    )
    .unwrap();
}

#[test]
fn binary_help_succeeds() {
    let mut cmd = Command::cargo_bin("sshpal").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Sync files and proxy local-only tasks through SSH",
        ))
        .stdout(predicate::str::contains("serve"))
        .stdout(predicate::str::contains("checkhealth"))
        .stdout(predicate::str::contains("other-run").not())
        .stdout(predicate::str::contains("install-remote").not());
}

#[test]
fn binary_reports_missing_config() {
    let temp = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("sshpal").unwrap();
    cmd.current_dir(temp.path())
        .args(["push", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no .sshpal.toml found"));
}

#[test]
fn tasks_help_lists_local_usage() {
    let temp = tempfile::tempdir().unwrap();
    write_task_config(temp.path());

    let mut cmd = Command::cargo_bin("sshpal").unwrap();
    cmd.current_dir(temp.path())
        .arg("tasks-help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "usage: sshpal run render name=<value> [mode=<value>] [-- <args...>]",
        ))
        .stdout(predicate::str::contains("Render a formatted line"))
        .stdout(predicate::str::contains("run: printf 'quick task\\n'"));
}

#[test]
fn local_run_executes_task_with_vars_and_forwarded_args() {
    let temp = tempfile::tempdir().unwrap();
    write_task_config(temp.path());

    let mut cmd = Command::cargo_bin("sshpal").unwrap();
    cmd.current_dir(temp.path())
        .args(["run", "render", "name=hello world", "--", "tail"])
        .assert()
        .success()
        .stdout("hello world||tail\n");
}

#[test]
fn local_run_applies_task_cwd_and_env() {
    let temp = tempfile::tempdir().unwrap();
    write_task_config(temp.path());

    let mut cmd = Command::cargo_bin("sshpal").unwrap();
    cmd.current_dir(temp.path())
        .args(["run", "cwd_env"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            temp.path().to_string_lossy().as_ref(),
        ))
        .stdout(predicate::str::contains("|hello-env"));
}
