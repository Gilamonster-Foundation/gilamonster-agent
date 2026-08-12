//! Rust-native `gila dev` — Phase 3 of the gila-parity plan.
//!
//! Dev-environment checks: verify the tools gila relies on are on `PATH` and
//! report each one's resolved location (or "missing"). Pure check-list data +
//! result rendering are unit-testable; the binary's `run_*` arm owns the PATH
//! lookup + print.

use std::path::{Path, PathBuf};

/// A tool gila checks for, with the binary name to look up on `PATH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCheck {
    /// Display name.
    pub name: &'static str,
    /// Binary name resolved on `PATH`.
    pub bin: &'static str,
}

/// The default set of dev tools `gila dev` checks.
pub const DEFAULT_CHECKS: &[ToolCheck] = &[
    ToolCheck { name: "git", bin: "git" },
    ToolCheck { name: "cargo", bin: "cargo" },
    ToolCheck { name: "python", bin: "python3" },
    ToolCheck { name: "gh", bin: "gh" },
    ToolCheck { name: "mmdc", bin: "mmdc" },
];

/// The outcome of one check: where the binary resolved, or `None` if missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// The check that ran.
    pub check: ToolCheck,
    /// Resolved path when found.
    pub found: Option<PathBuf>,
}

/// Search `dirs` (PATH entries) for an executable named `bin`. Pure — injects
/// the dir list so tests are hermetic.
pub fn find_on_path(dirs: &[PathBuf], bin: &str) -> Option<PathBuf> {
    dirs.iter()
        .map(|d| d.join(bin))
        .find(|p| p.is_file() && is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Run every check against `dirs`. Pure given the dir list.
pub fn run_checks(dirs: &[PathBuf]) -> Vec<CheckResult> {
    DEFAULT_CHECKS
        .iter()
        .map(|&check| CheckResult { check, found: find_on_path(dirs, check.bin) })
        .collect()
}

/// Render check results as display lines (`name: path` or `name: MISSING`).
pub fn render(results: &[CheckResult]) -> String {
    let mut out = String::new();
    for r in results {
        match &r.found {
            Some(p) => out.push_str(&format!("{}: {}\n", r.check.name, p.display())),
            None => out.push_str(&format!("{}: MISSING\n", r.check.name)),
        }
    }
    out
}

/// Split a `PATH` value into dir entries (used by the binary arm).
pub fn path_dirs(path_var: &std::ffi::OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path_var).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_on_path_locates_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let tool = bin_dir.join("mytool");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let dirs = vec![bin_dir.clone()];
        assert_eq!(find_on_path(&dirs, "mytool"), Some(tool));
        assert_eq!(find_on_path(&dirs, "missing"), None);
    }

    #[test]
    fn render_marks_missing() {
        let results = vec![
            CheckResult { check: DEFAULT_CHECKS[0], found: Some(PathBuf::from("/usr/bin/git")) },
            CheckResult { check: DEFAULT_CHECKS[3], found: None },
        ];
        let r = render(&results);
        assert!(r.contains("git: /usr/bin/git"));
        assert!(r.contains("gh: MISSING"));
    }

    #[test]
    fn default_checks_cover_core_tools() {
        let names: Vec<_> = DEFAULT_CHECKS.iter().map(|c| c.name).collect();
        for n in ["git", "cargo", "python", "gh"] {
            assert!(names.contains(&n), "missing check for {n}");
        }
    }
}
