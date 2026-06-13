//! End-to-end CLI tests for the `gila` binary.
//!
//! These run the real compiled binary (via `assert_cmd`) and assert on its
//! stdout / exit status. They cover `main()` and the `matrix` subcommand arm —
//! the shim lines that the pure unit tests in `src/lib.rs` can't reach. The
//! only deliberately-uncovered line is the `gila code` hand-off to newt's
//! interactive TUI (`newt_tui::run_code`), which cannot run headless in CI.

use assert_cmd::Command;
use predicates::prelude::*;

fn gila() -> Command {
    Command::cargo_bin("gila").expect("gila binary builds")
}

#[test]
fn matrix_prints_identity_and_scaffold_notice() {
    gila()
        .arg("matrix")
        .assert()
        .success()
        .stdout(predicate::str::contains("operator identity"))
        .stdout(predicate::str::contains("is not yet built"))
        .stdout(predicate::str::contains("agent-mesh airspace"));
}

#[test]
fn matrix_identity_line_resolves_when_home_set() {
    // With HOME set (the normal case, incl. CI) the inherited
    // newt_identity::default_key_path() resolves a real path, so the report
    // takes the Ok arm rather than the "HOME unset" fallback.
    gila()
        .arg("matrix")
        .env("HOME", "/home/example")
        .assert()
        .success()
        .stdout(predicate::str::contains("inherited from newt-identity"));
}

#[test]
fn version_flag_reports_a_version() {
    gila()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("gila"));
}

#[test]
fn help_flag_lists_subcommands() {
    gila()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("code"))
        .stdout(predicate::str::contains("matrix"));
}

#[test]
fn unknown_subcommand_is_rejected() {
    gila().arg("definitely-not-a-command").assert().failure();
}
