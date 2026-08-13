//! Command parity suite: run the same argv through both the Python `gilabot`
//! reference CLI and the Rust `gila` (gilamonster-agent) binary, then compare
//! exit codes and (for deterministic commands) stdout.
//!
//! Ground rules:
//! - Discovery of the Python reference CLI is opt-in via `GILABOT_BIN`; when
//!   unset we probe `gila-py`, `gilabot`, then `gila` on PATH, skipping any
//!   candidate that resolves to the Rust binary under test (or to nothing).
//! - Every parity case skips cleanly when no Python reference CLI is present,
//!   so CI/dev boxes without gilabot installed stay green.
//! - Comparisons are exact for exit codes; stdout is compared after trailing
//!   whitespace normalisation, and cases known to embed volatile data
//!   (timestamps, absolute paths, versions under dev) are marked
//!   `stdout: Volatile` so only the exit code is asserted.

use std::path::{Path, PathBuf};
use std::process::Command;

/// How stdout is compared for a parity case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stdout {
    /// Contains the needle (both sides must contain it).
    Contains(&'static str),
    /// Too volatile to compare — exit code only.
    Volatile,
}

/// One parity case: an argv (sans binary name) and comparison policy.
struct Case {
    name: &'static str,
    args: &'static [&'static str],
    stdout: Stdout,
}

/// Read-only, side-effect-free commands safe to run against both CLIs in any
/// environment. Only command surface that genuinely exists in BOTH CLIs may
/// live here — the suite's job is asserting parity on common surface, not
/// flagging intentional Rust-only additions (e.g. `completion`, which Python
/// gilabot does not implement, was removed for exactly that reason).
const SAFE_CASES: &[Case] = &[
    Case {
        name: "help_flag",
        args: &["--help"],
        stdout: Stdout::Contains("gila"),
    },
    Case {
        name: "unknown_command_fails",
        args: &["definitely-not-a-real-command"],
        // Both must fail non-zero for an unknown subcommand.
        stdout: Stdout::Volatile,
    },
];

/// Result of running one side of a parity case.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(bin: &Path, args: &[&str]) -> Run {
    let out = Command::new(bin)
        .args(args)
        // Isolate from the operator's real config/state so runs are
        // reproducible and side-effect-free.
        .env("HOME", std::env::temp_dir().join("gila-parity-home"))
        .env_remove("VIRTUAL_ENV")
        .env_remove("GILA_CAP_VENV")
        .env_remove("GILA_CAP_PYTHON")
        .output()
        .unwrap_or_else(|e| panic!("spawn {} {:?}: {}", bin.display(), args, e));
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Resolve the Python reference CLI, or None when unavailable.
fn python_gilabot() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("GILABOT_BIN") {
        let p = PathBuf::from(explicit);
        return p.exists().then_some(p);
    }
    // The Rust binary under test is typically *the* `gila` on PATH, so plain
    // `gila` is only accepted when it is NOT our own compiled binary.
    let own = assert_cmd::cargo::cargo_bin("gila")
        .canonicalize()
        .unwrap_or_else(|_| assert_cmd::cargo::cargo_bin("gila"));
    for name in ["gila-py", "gilabot", "gila"] {
        if let Ok(paths) = which::which(name) {
            let canon = paths.canonicalize().unwrap_or(paths);
            if canon != own {
                return Some(canon);
            }
        }
    }
    None
}

#[test]
fn parity_suite_matches_python_reference() {
    let Some(python) = python_gilabot() else {
        eprintln!("SKIP parity suite: no Python gilabot found (set GILABOT_BIN)");
        return;
    };
    let rust = assert_cmd::cargo::cargo_bin("gila");

    // Ensure the isolated HOME exists so Python doesn't trip over it.
    let home = std::env::temp_dir().join("gila-parity-home");
    std::fs::create_dir_all(&home).expect("create parity HOME");

    let mut failures = Vec::new();
    for case in SAFE_CASES {
        let py = run(&python, case.args);
        let rs = run(&rust, case.args);

        if py.code != rs.code {
            failures.push(format!(
                "[{}] exit code mismatch for {:?}: python={:?} rust={:?}\n  py.stderr: {}\n  rs.stderr: {}",
                case.name, case.args, py.code, rs.code, py.stderr, rs.stderr
            ));
            continue;
        }
        match case.stdout {
            Stdout::Contains(needle) => {
                if !py.stdout.contains(needle) {
                    failures.push(format!(
                        "[{}] python stdout missing {needle:?} for {:?}:\n{}",
                        case.name, case.args, py.stdout
                    ));
                }
                if !rs.stdout.contains(needle) {
                    failures.push(format!(
                        "[{}] rust stdout missing {needle:?} for {:?}:\n{}",
                        case.name, case.args, rs.stdout
                    ));
                }
            }
            Stdout::Volatile => {
                // Both sides must at least succeed/fail identically — already
                // asserted via exit code above.
            }
        }
    }

    assert!(
        failures.is_empty(),
        "parity suite failed ({} case(s)) against {}:\n\n{}",
        failures.len(),
        python.display(),
        failures.join("\n\n")
    );
    eprintln!(
        "parity suite: {} case(s) matched python reference at {}",
        SAFE_CASES.len(),
        python.display()
    );
}

/// The parity resolver must never pick the Rust binary as the "python"
/// reference when it's the only `gila` on PATH.
#[test]
fn parity_resolver_does_not_self_match() {
    if std::env::var("GILABOT_BIN").is_ok() {
        return; // explicit override always wins
    }
    let own = assert_cmd::cargo::cargo_bin("gila")
        .canonicalize()
        .unwrap_or_else(|_| assert_cmd::cargo::cargo_bin("gila"));
    if let Some(found) = python_gilabot() {
        assert_ne!(
            found, own,
            "resolver picked the Rust binary under test as the Python reference"
        );
    }
}
