use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn binary_help_succeeds() {
    let mut cmd = Command::cargo_bin("sshpal").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Sync files and proxy local-only tasks through SSH"));
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
