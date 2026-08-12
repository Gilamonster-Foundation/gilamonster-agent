//! Rust-native `gila projects` — Phase 3 of the gila-parity plan.
//!
//! Lists active projects: the git repositories directly under the workspace
//! root (default `~/workspaces`). A directory counts as a project when it has
//! a `.git` entry. Pure scanning + formatting is unit-testable against a temp
//! dir; the binary's `run_*` arm owns the root resolution + print.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Default workspace root, relative to `$HOME`.
pub const DEFAULT_WORKSPACE_REL: &str = "workspaces";

/// Resolve the workspace root.
pub fn workspace_root(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_WORKSPACE_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the workspace root"),
    }
}

/// True when `dir` looks like a project (contains a `.git` entry).
pub fn is_project(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// List project directories directly under `root`, sorted by name.
pub fn list_projects(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir() && is_project(p))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Render the project list as display lines (names only, 1 per line).
pub fn render_projects(projects: &[PathBuf]) -> String {
    if projects.is_empty() {
        return "no projects found\n".to_string();
    }
    let mut out = String::new();
    for p in projects {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push_str(&name);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_root_requires_home() {
        assert!(workspace_root(None).is_err());
        assert_eq!(
            workspace_root(Some(Path::new("/home/op"))).unwrap(),
            PathBuf::from("/home/op/workspaces")
        );
    }

    #[test]
    fn is_project_detects_dot_git() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(is_project(&proj));
        assert!(!is_project(&plain));
    }

    #[test]
    fn list_and_render_only_git_dirs_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["zeta", "alpha"] {
            std::fs::create_dir_all(tmp.path().join(name).join(".git")).unwrap();
        }
        std::fs::create_dir_all(tmp.path().join("not-a-repo")).unwrap();
        let projs = list_projects(tmp.path());
        let names: Vec<_> = projs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        assert_eq!(render_projects(&projs), "alpha\nzeta\n");
        assert_eq!(render_projects(&[]), "no projects found\n");
    }
}
