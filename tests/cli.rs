//! End-to-end CLI tests for the `gila` binary.
//!
//! These run the real compiled binary (via `assert_cmd`) and assert on its
//! stdout / exit status. They cover `main()`, the `matrix` arm, and the
//! `follow` arm's no-typescript path — the shim lines that the pure unit tests
//! in `src/lib.rs` / `src/follow.rs` can't reach. The deliberately-uncovered
//! lines are the `gila code` hand-off to newt's interactive TUI
//! (`newt_tui::run_code`) and the live `gila follow` tail/print loop, neither of
//! which can run headless in CI.

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
        .stdout(predicate::str::contains("follow"))
        .stdout(predicate::str::contains("matrix"));
}

#[test]
fn follow_help_describes_read_only_observation() {
    gila()
        .args(["follow", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Read-only"))
        .stdout(predicate::str::contains("typescript"));
}

#[test]
fn follow_with_no_typescript_prints_read_only_guidance() {
    // An empty watch dir → no typescript to tail. The follow arm short-circuits
    // BEFORE touching any inference backend, printing the read-only guidance and
    // exiting cleanly. This exercises the binary's `Follow` arm headlessly.
    let dir = tempfile::tempdir().expect("tempdir");
    gila()
        .args(["follow", "--dir"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("read-only"))
        .stdout(predicate::str::contains("no typescript found"))
        .stdout(predicate::str::contains("never drives your shell"));
}

#[test]
fn unknown_subcommand_is_rejected() {
    gila().arg("definitely-not-a-command").assert().failure();
}
