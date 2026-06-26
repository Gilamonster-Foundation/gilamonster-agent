//! End-to-end CLI tests for the `gila` binary.
//!
//! These run the real compiled binary (via `assert_cmd`) and assert on its
//! stdout / exit status. They cover `main()`, the `matrix` arm, and the
//! `follow` arm's no-typescript path — the shim lines that the pure unit tests
//! in `src/lib.rs` / `src/follow.rs` can't reach. The deliberately-uncovered
//! lines are the `gila code` hand-off to newt's interactive TUI
//! (`newt_tui::run_code`), the live `gila follow` tail/print loop, and the
//! `gila cowork` full-screen render/event loop (`run_cowork`) — none of which
//! can run headless in CI (they need a real tty). The cowork *logic* is unit-
//! tested in `src/cowork.rs`; only the tty wiring is the carve-out.

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
        .stdout(predicate::str::contains("cowork"))
        .stdout(predicate::str::contains("hotseat"))
        .stdout(predicate::str::contains("matrix"));
}

#[test]
fn hotseat_help_describes_the_triage_cockpit() {
    // `gila hotseat --help` exercises the clap registration of the new arm
    // without launching the TUI (which needs a real tty). The help text should
    // describe the on-call/triage cockpit, the read-only posture, the modulex
    // MCP search surface, and the `--skill` flag — and name NO enterprise system.
    gila()
        .args(["hotseat", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("triage"))
        .stdout(predicate::str::contains("read-only"))
        .stdout(predicate::str::contains("modulex"))
        .stdout(predicate::str::contains("--skill"));
}

#[test]
fn cowork_help_describes_the_split_pane_cockpit() {
    // `gila cowork --help` exercises the clap registration of the new arm
    // without launching the full-screen TUI (which needs a real tty). The help
    // text should describe the split cockpit and name its separation from
    // `gila code`.
    gila()
        .args(["cowork", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("split-pane"))
        .stdout(predicate::str::contains("chat"))
        .stdout(predicate::str::contains("shell"));
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

#[test]
fn capabilities_run_dispatches_and_errors_clearly_without_a_caps_venv() {
    // Point the venv resolver at a venv that does not exist (a high-precedence
    // env knob), so `gilacap` cannot be spawned regardless of the host. This
    // proves the `capabilities run` arm is wired end-to-end (argv → main dispatch
    // → capabilities::run → spawn attempt) and that a missing caps venv fails
    // with an actionable message rather than a panic.
    gila()
        .args(["capabilities", "run", "confluence", "blog"])
        .env("GILA_CAP_VENV", "/gila-caps-venv-does-not-exist")
        .env_remove("GILA_CAP_PYTHON")
        .assert()
        .failure()
        .stderr(predicate::str::contains("confluence:blog"));
}

#[test]
fn capabilities_run_engages_the_confined_path_when_the_manifest_marks_it() {
    // A manifest entry with `confined = true` routes `run` through the agent-bridle
    // confined spawn (caveats mint + ConfinedCommand) instead of the bare spawn.
    // The venv is bogus so the spawn fails fast — the point is to prove the
    // *confined* code path executes, which surfaces a "confined spawn" error
    // (or a fail-closed refusal on a kernel without Landlock — both wrapped here).
    let home = tempfile::tempdir().expect("tempdir");
    let gdir = home.path().join(".gila");
    std::fs::create_dir_all(&gdir).expect("mkdir .gila");
    std::fs::write(
        gdir.join("capabilities.toml"),
        "[[capabilities]]\nname = \"demo\"\nconfined = true\n",
    )
    .expect("write manifest");

    gila()
        .args(["capabilities", "run", "demo", "sometool"])
        .env("HOME", home.path())
        .env("GILA_CAP_VENV", "/gila-caps-venv-does-not-exist")
        .env_remove("GILA_CAP_PYTHON")
        .env_remove("VIRTUAL_ENV")
        .assert()
        .failure()
        .stderr(predicate::str::contains("confined spawn"));
}
