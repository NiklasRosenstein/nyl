use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_cli_help() {
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Kubernetes manifest generator"));
}

#[test]
fn test_cli_version() {
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("--version");
    cmd.assert().success().stdout(predicate::str::contains("nyl"));
}

#[test]
fn test_render_command_stub() {
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("render");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("not yet implemented"));
}

#[test]
fn test_diff_command_stub() {
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("diff");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("not yet implemented"));
}

#[test]
fn test_apply_command_stub() {
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("apply");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("not yet implemented"));
}

#[test]
fn test_new_command_stub() {
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("new").arg("test-project");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("not yet implemented"));
}

#[test]
fn test_validate_command_stub() {
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("validate");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("not yet implemented"));
}
