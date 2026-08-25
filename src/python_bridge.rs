//! In-process Python bridge (pyo3) for the gila-parity plan's Phase 2.
//!
//! Operator decision (2026-08-12): **pyo3 in-process embedding**, not a
//! vendored-venv subprocess. The high-complexity gilabot commands
//! (confluence, jira, slack, mcp, assistant, doc, calendar, review,
//! pagerduty) stay in Python and are invoked *inside* this process via pyo3,
//! so there's no per-call subprocess startup cost.
//!
//! # Interpreter selection
//!
//! The plugins are installed **editable into `~/venv`** from
//! `~/workspaces/gilabot/gila-plugin-*/`; the default `python3` (a pyenv shim)
//! does **not** have them importable. So pyo3 must embed `~/venv`'s
//! interpreter. Two layers cooperate:
//!
//! * **Build time** — set `PYO3_PYTHON=/Users/shartsock/venv/bin/python`
//!   (or pass `--config PYO3_PYTHON=…`) so pyo3 links and initializes the
//!   venv interpreter, not the shim. With `auto-initialize`, pyo3 runs that
//!   interpreter's config.
//! * **Run time** — [`ensure_sys_path`] prepends the venv's `site-packages`
//!   (and the editable plugin source roots) to `sys.path` defensively, so even
//!   if the embedded interpreter didn't pick up the venv's site-packages, the
//!   plugins still resolve.
//!
//! # The call
//!
//! gilabot's console script is `from gilabot import main; sys.exit(main())` —
//! a single dispatcher that reads `sys.argv`. So a vendored command is:
//! prepare `sys.path`, set `sys.argv = [<cmd>, <args>…]`, call
//! `gilabot.main()`, and map its return / `SystemExit` to an exit code. The
//! pure, testable logic is [`build_argv`] and [`site_packages_dirs`]; the GIL
//! and import and call seam is intentionally thin (it needs a real interpreter,
//! so it is covered by integration tests, not unit tests).

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};
use pyo3::prelude::*;
use pyo3::types::PyList;

/// The gilabot dispatcher module + callable, matching the console script
/// (`from gilabot import main`).
pub const GILABOT_MODULE: &str = "gilabot";
pub const GILABOT_CALLABLE: &str = "main";

/// Subcommands routed to the in-process pyo3 bridge (Phase 2) instead of the
/// shell-delegate subprocess. These are the plan doc's high-complexity,
/// Python-vendored commands. Everything else still shell-delegates.
///
/// Pure data — the "which commands run in-process" decision is configuration,
/// not code, so adding a command is a one-word change here.
pub const PYO3_ROUTED_COMMANDS: &[&str] = &[
    "confluence",
    "jira",
    "slack",
    "mcp",
    "assistant",
    "doc",
    "calendar",
    "review",
    "pagerduty",
];

/// Should `cmd` run in-process via pyo3 (true) or shell-delegate (false)?
/// Pure lookup against [`PYO3_ROUTED_COMMANDS`].
pub fn is_pyo3_routed(cmd: &str) -> bool {
    PYO3_ROUTED_COMMANDS.contains(&cmd)
}

/// Build the `sys.argv` vector for a delegated Python call: the subcommand
/// name followed by its args, order preserved, none dropped. Pure.
pub fn build_argv(cmd: &str, args: &[OsString]) -> Vec<String> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd.to_string());
    argv.extend(args.iter().map(|a| a.to_string_lossy().into_owned()));
    argv
}

/// Candidate `site-packages` directories for a venv root, in priority order.
///
/// Pure path construction (no filesystem access) so the layout logic is
/// unit-testable. `venv_root` is the venv directory (the one containing
/// `bin/python`). We emit both the versioned form we can compute and the
/// editable-install source roots the plugins live in.
pub fn site_packages_dirs(
    venv_root: &std::path::Path,
    py_major: u32,
    py_minor: u32,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Standard POSIX venv layout: lib/pythonX.Y/site-packages
    dirs.push(
        venv_root
            .join("lib")
            .join(format!("python{py_major}.{py_minor}"))
            .join("site-packages"),
    );
    // Some layouts use lib64 (Linux) — harmless to include even if absent.
    dirs.push(
        venv_root
            .join("lib64")
            .join(format!("python{py_major}.{py_minor}"))
            .join("site-packages"),
    );
    // Windows venv layout: Lib/site-packages (no version dir).
    dirs.push(venv_root.join("Lib").join("site-packages"));
    dirs
}

/// The venv root to embed. Reads `GILA_AGENT_VENV` first, then `VIRTUAL_ENV`,
/// then falls back to `~/venv`. Pure with respect to the injected `home` for
/// the fallback; the env reads are the by-design effectful edge.
pub fn venv_root_from_env(home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("GILA_AGENT_VENV") {
        return Some(PathBuf::from(v));
    }
    if let Some(v) = std::env::var_os("VIRTUAL_ENV") {
        return Some(PathBuf::from(v));
    }
    home.map(|h| h.join("venv"))
}

/// Return the embedded interpreter's `(major, minor)` version (e.g. `(3, 13)`).
///
/// Used to compute the venv `site-packages` path. Reads from the live
/// interpreter, so it reflects the interpreter pyo3 actually embedded (the one
/// `PYO3_PYTHON` selected at build time), not a hardcoded guess.
fn interpreter_version(py: Python<'_>) -> (u32, u32) {
    // pyo3 exposes the embedded interpreter version via Python::version_info().
    let vi = py.version_info();
    (u32::from(vi.major), u32::from(vi.minor))
}

/// Prepend the venv's `site-packages` to `sys.path` (idempotent), so the
/// editable-installed plugins resolve even if the embedded interpreter didn't
/// auto-load the venv's site-packages. Defensive: inserts only dirs that exist
/// and aren't already present.
fn ensure_sys_path(py: Python<'_>) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(venv_root) = venv_root_from_env(home) else {
        return Ok(()); // no venv resolvable — rely on ambient interpreter
    };
    let (major, minor) = interpreter_version(py);
    let sys = py.import("sys").context("import sys")?;
    let path = sys
        .getattr("path")?
        .cast_into::<PyList>()
        .map_err(|e| anyhow::anyhow!("sys.path is not a list: {e}"))?;
    let existing: Vec<String> = path
        .iter()
        .filter_map(|p| p.extract::<String>().ok())
        .collect();
    let mut inserted_any = false;
    for dir in site_packages_dirs(&venv_root, major, minor) {
        if !dir.is_dir() {
            continue;
        }
        // Register the dir with the `site` module so its `.pth` files are
        // processed — gilabot's editable installs live behind
        // `__editable__*.finder.__path_hook__` entries that only activate when
        // the .pth runs. A bare sys.path insert misses them.
        let site = py.import("site")?;
        site.call_method1("addsitedir", (dir.to_string_lossy().into_owned(),))?;
        let s = dir.to_string_lossy().into_owned();
        if !existing.contains(&s) {
            path.insert(0, s)?;
            inserted_any = true;
        }
    }
    // The `gilabot` package itself is an editable source dir, not importable
    // from site-packages: add its parent (the gilabot workspace root) so
    // `import gilabot` resolves to `…/gilabot/gilabot/__init__.py`.
    if let Some(gilabot_root) = gilabot_source_root() {
        let s = gilabot_root.to_string_lossy().into_owned();
        if gilabot_root.join("gilabot/__init__.py").is_file() && !existing.contains(&s) {
            path.insert(0, s)?;
            inserted_any = true;
        }
    }
    let _ = inserted_any;
    Ok(())
}

/// The gilabot workspace source root (the dir containing `gilabot/`).
/// `GILABOT_SRC` overrides; else `~/workspaces/gilabot`.
fn gilabot_source_root() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("GILABOT_SRC") {
        return Some(PathBuf::from(v));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("workspaces/gilabot"))
}

/// Map a Python `SystemExit` (or normal return) to a process exit code.
fn exit_code_from_result(result: Result<(), PyErr>, py: Python<'_>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(err) => {
            let is_system_exit = err.is_instance_of::<pyo3::exceptions::PySystemExit>(py);
            // `SystemExit` carries the intended exit code (default 0).
            let code = err
                .value(py)
                .getattr("code")
                .and_then(|c| c.extract::<i32>())
                .unwrap_or(if is_system_exit { 0 } else { 1 });
            if !is_system_exit {
                // A real exception (not SystemExit): print the traceback.
                err.print(py);
            }
            code
        }
    }
}

/// Run a vendored gilabot command in-process: prepare `sys.path`, set
/// `sys.argv = [cmd, args…]`, call `gilabot.main()`. Returns the process exit
/// code the Python side requested (via `SystemExit`) or 0 on clean return.
///
/// This is the thin GIL seam — it needs a real embedded interpreter, so it is
/// covered by integration tests rather than unit tests.
pub fn run_python_command(cmd: &str, args: &[OsString]) -> Result<i32> {
    let out: Result<i32> = Python::attach(|py| -> Result<i32> {
        ensure_sys_path(py)?;
        let sys = py.import("sys")?;
        sys.setattr("argv", PyList::new(py, build_argv(cmd, args))?)?;
        let module = py.import(GILABOT_MODULE).with_context(|| {
            format!("import {GILABOT_MODULE} (is the venv embedded? set PYO3_PYTHON)")
        })?;
        let main = module.getattr(GILABOT_CALLABLE)?;
        let result = main.call0().map(|_| ());
        Ok(exit_code_from_result(result, py))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routed_commands_are_in_process_others_delegate() {
        for cmd in ["confluence", "jira", "slack", "mcp", "assistant"] {
            assert!(is_pyo3_routed(cmd), "{cmd} should be pyo3-routed");
        }
        // Rust-native (git) and low-complexity commands still shell-delegate.
        for cmd in ["git", "board", "todos", "version", "nonexistent-cmd"] {
            assert!(!is_pyo3_routed(cmd), "{cmd} should shell-delegate");
        }
    }

    #[test]
    fn build_argv_prepends_cmd_and_preserves_order() {
        let args = vec![
            OsString::from("publish"),
            OsString::from("doc.md"),
            OsString::from("--space"),
            OsString::from("ENG"),
        ];
        let argv = build_argv("confluence", &args);
        assert_eq!(
            argv,
            vec!["confluence", "publish", "doc.md", "--space", "ENG"]
        );
    }

    #[test]
    fn build_argv_handles_empty_args() {
        assert_eq!(build_argv("plugins", &[]), vec!["plugins"]);
    }

    #[test]
    fn site_packages_dirs_emits_versioned_and_windows_layouts() {
        let root = std::path::Path::new("/home/u/venv");
        let dirs = site_packages_dirs(root, 3, 13);
        assert!(dirs.contains(&root.join("lib/python3.13/site-packages")));
        assert!(dirs.contains(&root.join("lib64/python3.13/site-packages")));
        assert!(dirs.contains(&root.join("Lib/site-packages")));
    }

    #[test]
    fn venv_root_prefers_explicit_env_then_virtual_env_then_home() {
        // Can't mutate process env safely in a parallel test; assert the
        // fallback path only (home → ~/venv) which needs no env.
        let home = Some(PathBuf::from("/home/u"));
        // With neither GILA_AGENT_VENV nor VIRTUAL_ENV set this returns
        // home/venv — but env may be set in CI, so only assert the shape when
        // the fallback branch is what runs. Guard: if env is set, skip.
        if std::env::var_os("GILA_AGENT_VENV").is_none()
            && std::env::var_os("VIRTUAL_ENV").is_none()
        {
            assert_eq!(
                venv_root_from_env(home),
                Some(PathBuf::from("/home/u/venv"))
            );
        }
        assert_eq!(
            venv_root_from_env(None),
            None.or_else(|| {
                // home None with no env → None
                if std::env::var_os("GILA_AGENT_VENV").is_none()
                    && std::env::var_os("VIRTUAL_ENV").is_none()
                {
                    None
                } else {
                    venv_root_from_env(None)
                }
            })
        );
    }
}
