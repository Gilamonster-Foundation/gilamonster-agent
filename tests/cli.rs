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

/// A temp dir holding a fake `gilacap` on PATH: `gilacap list` prints one demo
/// capability; anything else echoes. Lets the `cap list`/`config` arms run
/// headless without a real caps venv.
#[cfg(unix)]
fn fake_gilacap_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("gilacap");
    std::fs::write(
        &script,
        "#!/bin/sh\ncase \"$1\" in\n  list) printf 'demo\\tA demo capability.\\n' ;;\n  *) echo \"fake gilacap: $*\" ;;\nesac\n",
    )
    .expect("write fake gilacap");
    let mut perm = std::fs::metadata(&script).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&script, perm).unwrap();
    dir
}

/// Point `gila` at the fake `gilacap` (PATH) with a clean HOME and no venv-env
/// overrides, so resolution falls through to the bare `gilacap` on PATH.
#[cfg(unix)]
fn with_fake_gilacap<'a>(
    cmd: &'a mut Command,
    bin: &std::path::Path,
    home: &std::path::Path,
) -> &'a mut Command {
    let path = std::env::var("PATH").unwrap_or_default();
    cmd.env("HOME", home)
        .env("PATH", format!("{}:{}", bin.display(), path))
        .env_remove("GILA_CAP_VENV")
        .env_remove("GILA_CAP_PYTHON")
        .env_remove("VIRTUAL_ENV")
}

#[test]
fn matrix_prints_identity_and_scaffold_notice() {
    gila()
        .arg("matrix")
        .assert()
        .success()
        .stdout(predicate::str::contains("operator identity"))
        .stdout(predicate::str::contains("extension layer"))
        .stdout(predicate::str::contains("gila matrix --mock"))
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

#[cfg(unix)]
#[test]
fn cap_list_runs_gilacap_list() {
    // `gila cap list` (alias) shells the resolved `gilacap list`.
    let bin = fake_gilacap_dir();
    let home = tempfile::tempdir().expect("home");
    let mut cmd = gila();
    with_fake_gilacap(cmd.arg("cap").arg("list"), bin.path(), home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("demo"));
}

#[cfg(unix)]
#[test]
fn cap_config_writes_the_manifest_from_interactive_choices() {
    // Drive the selector headlessly: `demo` → [b]oth, confine [y]. The chosen
    // policy lands in <HOME>/.gila/capabilities.toml.
    let bin = fake_gilacap_dir();
    let home = tempfile::tempdir().expect("home");
    let mut cmd = gila();
    with_fake_gilacap(cmd.arg("cap").arg("config"), bin.path(), home.path())
        .write_stdin("b\ny\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote"));

    let manifest = std::fs::read_to_string(home.path().join(".gila").join("capabilities.toml"))
        .expect("manifest written");
    assert!(manifest.contains("name = \"demo\""));
    assert!(manifest.contains("expose = \"both\""));
    assert!(manifest.contains("confined = true"));
}

#[test]
fn cap_enable_prints_the_mcp_servers_snippet() {
    gila()
        .args(["cap", "enable", "confluence"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[[mcp_servers]]"))
        .stdout(predicate::str::contains("confluence"));
}

// --- gila-parity Phase 1: Rust-native `gila git` integration tests ----------
//
// These run the real binary against real temp git repos and assert on both the
// CLI output and the resulting git state (HEAD subject + clean tree), which is
// the true end-to-end contract of `commit` and `tend`.

/// Initialize a git repo in a fresh tempdir with one empty `init` commit.
/// Returns the TempDir guard (the repo lives at its root).
fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("repo tempdir");
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git runs")
    };
    assert!(run(&["init", "-q"]).status.success());
    // Persist identity in the repo config (not just `-c` on one command) so
    // git2::Repository::signature() resolves on CI runners with no global git
    // identity configured.
    assert!(run(&["config", "user.email", "t@t.t"]).status.success());
    assert!(run(&["config", "user.name", "t"]).status.success());
    assert!(run(&["commit", "-q", "--allow-empty", "-m", "init"])
        .status
        .success());
    dir
}

/// The subject of HEAD in `repo`.
fn head_subject(repo: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .current_dir(repo)
        .output()
        .expect("git log runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// True when `repo`'s working tree has no staged/unstaged/untracked changes.
fn is_clean(repo: &std::path::Path) -> bool {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .output()
        .expect("git status runs");
    String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

#[test]
fn git_commit_stages_all_and_writes_a_commit() {
    let repo = git_repo();
    std::fs::write(repo.path().join("a.txt"), "hello").expect("write a.txt");

    gila()
        .args(["git", "commit", "-m", "add a.txt", "--path"])
        .arg(repo.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("committed"));

    assert_eq!(head_subject(repo.path()), "add a.txt");
    assert!(is_clean(repo.path()), "tree clean after commit");
}

#[test]
fn git_commit_on_clean_tree_is_a_noop_not_an_error() {
    let repo = git_repo(); // already clean

    gila()
        .args(["git", "commit", "-m", "noop", "--path"])
        .arg(repo.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to commit"));

    assert_eq!(head_subject(repo.path()), "init", "no new commit created");
}

#[test]
fn git_tend_runs_the_backup_profile_end_to_end() {
    let repo = git_repo();
    std::fs::write(repo.path().join("b.txt"), "change").expect("write b.txt");

    let cfg = tempfile::NamedTempFile::new().expect("config");
    std::fs::write(
        cfg.path(),
        format!(
            "defaults:\n  on_conflict: halt\n  commit_message: \"tend: backup {{timestamp}}\"\n\
             profiles:\n  backup:\n    - git add -A\n    - git commit -m \"{{commit_message}}\"\n\
             repos:\n  - path: {}\n",
            repo.path().display()
        ),
    )
    .expect("write config");

    // Dry-run: reports, but does NOT commit.
    gila()
        .args(["git", "tend", "--config"])
        .arg(cfg.path())
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains("dry-run"));
    assert_eq!(head_subject(repo.path()), "init", "dry-run must not commit");

    // Real run: the substituted commit message lands and the tree goes clean.
    gila()
        .args(["git", "tend", "--config"])
        .arg(cfg.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
    assert!(
        head_subject(repo.path()).starts_with("tend: backup "),
        "backup commit landed, got: {}",
        head_subject(repo.path())
    );
    assert!(is_clean(repo.path()), "tree clean after tend");

    // Second run on the now-clean tree: commit step is skipped, still ok.
    gila()
        .args(["git", "tend", "--config"])
        .arg(cfg.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn git_tend_missing_config_fails_with_guidance() {
    let missing = tempfile::tempdir().expect("dir").path().join("nope.yaml");
    gila()
        .args(["git", "tend", "--config"])
        .arg(&missing)
        .assert()
        .failure()
        .stderr(predicate::str::contains("git-tend config not found"));
}
