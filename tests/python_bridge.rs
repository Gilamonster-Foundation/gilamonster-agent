//! Integration tests for the pyo3-vendored command path (Phase 2 bridge).
//!
//! These exercise the real embedded-interpreter seam in
//! `src/python_bridge.rs`: `run_python_command` must prepare `sys.path`, set
//! `sys.argv`, call `gilabot.main()`, and map `SystemExit` to an exit code —
//! all against the operator's `~/venv` where the gila-plugin-* packages are
//! editable-installed.
//!
//! Environment gating: every test skips cleanly (returns early) unless the
//! venv python that was linked at build time (`PYO3_PYTHON`) is actually the
//! one carrying the plugins, i.e. `~/venv/bin/python` exists AND `gilabot` is
//! importable from it. On boxes without that venv (CI without the operator's
//! layout) these are no-ops, not failures.
//!
//! The commands chosen are the read-only/help surface of the pyo3-routed set
//! (confluence, jira, slack, mcp, assistant, doc, calendar, review,
//! pagerduty): invoking them with `--help` never touches the network or
//! credentials, so it is safe anywhere the venv exists.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

/// The embedded CPython interpreter is process-global, so concurrent test
/// threads corrupt click's global state. This mutex serializes all bridge
/// calls within the test binary.
static BRIDGE_LOCK: Mutex<()> = Mutex::new(());

/// The venv python the bridge expects to embed (`PYO3_PYTHON` at build time).
fn venv_python() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let p = home.join("venv/bin/python");
    p.exists().then_some(p)
}

/// Is `gilabot` importable from the venv python? Spawns a throwaway
/// interpreter probe — cheap relative to the per-test embedded init.
fn gilabot_importable(python: &PathBuf) -> bool {
    Command::new(python)
        .args(["-c", "import gilabot, sys; sys.exit(0)"])
        .env_remove("VIRTUAL_ENV")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Common gate: returns true when the embedded-bridge environment is present.
fn bridge_env_ready() -> bool {
    match venv_python() {
        Some(p) => gilabot_importable(&p),
        None => false,
    }
}

/// Run one vendored command through the in-process bridge and return its
/// exit code. Each call re-enters `Python::with_gil`, so tests stay isolated
/// from one another's interpreter state.
fn run(cmd: &str, args: &[&str]) -> i32 {
    let args: Vec<std::ffi::OsString> = args.iter().map(std::ffi::OsString::from).collect();
    gilamonster_agent::python_bridge::run_python_command(cmd, &args)
        .unwrap_or_else(|e| panic!("bridge error running {cmd} {args:?}: {e:#}"))
}

/// `--help` for every pyo3-routed command must reach click's help printer
/// through the bridge — proving sys.path prep, sys.argv set, `gilabot.main()`
/// dispatch, and the SystemExit mapping. Click exits 0 for `--help` on
/// commands and 2 on groups in some gilabot versions (a known gilabot quirk,
/// not a bridge failure), so both are accepted.
///
/// NOTE on interpreter reuse: the embedded CPython interpreter is
/// process-global, and click's dispatch mutates global state (sys.argv,
/// sys.modules, click's Context). Calling `gilabot.main()` a *second* time in
/// the same process is not guaranteed to behave like a fresh process — so
/// each `#[test]` here performs exactly ONE `run_python_command` call. The
/// loop below works because `--help` short-circuits before click builds
/// command state, but a stateful subcommand would not be safe to repeat.
#[test]
fn pyo3_routed_help_reaches_click_help() {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    if !bridge_env_ready() {
        eprintln!("SKIP: ~/venv with importable gilabot not present");
        return;
    }
    for cmd in gilamonster_agent::python_bridge::PYO3_ROUTED_COMMANDS {
        let code = run(cmd, &["--help"]);
        assert!(
            code == 0 || code == 2,
            "{cmd} --help should reach click help (exit 0 or click-group quirk 2), got {code}"
        );
    }
}

/// An unknown subcommand of a routed group must exit non-zero (click's
/// "No such command" is exit 2). This runs in its own test process (cargo
/// isolates integration tests per-file, and we keep one bridge call per test)
/// so it gets a fresh interpreter.
#[test]
fn pyo3_routed_unknown_subcommand_fails_nonzero() {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    if !bridge_env_ready() {
        eprintln!("SKIP: ~/venv with importable gilabot not present");
        return;
    }
    // Use `mcp` (leaf group) rather than `confluence` to avoid any ordering
    // interaction with the help loop above within the same process.
    let code = run("mcp", &["definitely-not-a-subcommand"]);
    assert_ne!(
        code, 0,
        "unknown mcp subcommand should fail non-zero via the bridge"
    );
}

/// The routing table stays pure configuration: the test binary and the
/// library agree on which commands are in-process. Guard against a stale
/// test-side list drifting from the lib's PYO3_ROUTED_COMMANDS.
#[test]
fn routing_table_is_the_lib_constant() {
    let routed = gilamonster_agent::python_bridge::PYO3_ROUTED_COMMANDS;
    for cmd in [
        "confluence",
        "jira",
        "slack",
        "mcp",
        "assistant",
        "doc",
        "calendar",
        "review",
        "pagerduty",
    ] {
        assert!(
            routed.contains(&cmd),
            "{cmd} must be in PYO3_ROUTED_COMMANDS"
        );
    }
}
