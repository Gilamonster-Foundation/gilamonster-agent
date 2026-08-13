//! Rust-native `gila worktree` — Phase 3 of the gila-parity plan.
//!
//! Manage git worktrees for a repo: `list` shows them, `add <name>` creates
//! one (a new branch checked out at `<repo>.worktrees/<name>`), `remove
//! <name>` deletes it. Worktree add/remove shell out to the `git` CLI (the
//! effectful seam — worktree semantics are subtle and the parity goal is
//! identical behavior); `list` parses `git worktree list --porcelain`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// One worktree entry from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Worktree path.
    pub path: PathBuf,
    /// Checked-out branch (short name), or `detached`.
    pub branch: String,
}

/// Parse `git worktree list --porcelain` output into worktrees. Pure.
pub fn parse_porcelain(out: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch = "detached".to_string();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(prev) = path.take() {
                worktrees.push(Worktree {
                    path: prev,
                    branch: branch.clone(),
                });
            }
            path = Some(PathBuf::from(p));
            branch = "detached".to_string();
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            branch = b.to_string();
        }
    }
    if let Some(p) = path {
        worktrees.push(Worktree { path: p, branch });
    }
    worktrees
}

/// List worktrees for the repo at `repo` (via `git worktree list --porcelain`).
pub fn list(repo: &Path) -> Result<Vec<Worktree>> {
    let out = Command::new("git")
        .args([
            "-C",
            &repo.display().to_string(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .output()
        .context("running git worktree list")?;
    if !out.status.success() {
        anyhow::bail!("git worktree list failed in {}", repo.display());
    }
    Ok(parse_porcelain(&String::from_utf8_lossy(&out.stdout)))
}

/// The worktree path for a repo + name: `<repo>.worktrees/<name>` sits beside
/// the repo, not inside it (so the repo's own status stays clean).
pub fn worktree_path(repo: &Path, name: &str) -> PathBuf {
    let parent = repo.parent().unwrap_or_else(|| Path::new("."));
    let base = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    parent.join(format!("{base}.worktrees")).join(name)
}

/// The argv for `git worktree add` (new branch `<name>` at the target path).
pub fn add_argv(repo: &Path, name: &str) -> Vec<String> {
    vec![
        "-C".into(),
        repo.display().to_string(),
        "worktree".into(),
        "add".into(),
        "-b".into(),
        name.into(),
        worktree_path(repo, name).display().to_string(),
    ]
}

/// The argv for `git worktree remove`.
pub fn remove_argv(repo: &Path, name: &str) -> Vec<String> {
    vec![
        "-C".into(),
        repo.display().to_string(),
        "worktree".into(),
        "remove".into(),
        worktree_path(repo, name).display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORCELAIN: &str = "worktree /repo\n\
                             HEAD abc123\n\
                             branch refs/heads/main\n\
                             \n\
                             worktree /repo.worktrees/feat\n\
                             HEAD def456\n\
                             branch refs/heads/feat\n\
                             \n\
                             worktree /repo.worktrees/det\n\
                             HEAD 789abc\n\
                             detached\n";

    #[test]
    fn parses_porcelain_worktrees() {
        let wts = parse_porcelain(PORCELAIN);
        assert_eq!(wts.len(), 3);
        assert_eq!(wts[0].path, PathBuf::from("/repo"));
        assert_eq!(wts[0].branch, "main");
        assert_eq!(wts[1].branch, "feat");
        assert_eq!(wts[2].branch, "detached");
    }

    #[test]
    fn worktree_path_sits_beside_repo() {
        assert_eq!(
            worktree_path(Path::new("/ws/myrepo"), "feat"),
            PathBuf::from("/ws/myrepo.worktrees/feat")
        );
    }

    #[test]
    fn argv_builders() {
        let add = add_argv(Path::new("/repo"), "feat");
        assert!(add.contains(&"add".to_string()));
        assert!(add.contains(&"-b".to_string()));
        // Windows joins path components with '\', so match the platform-
        // specific joined suffix rather than a hardcoded '/' separator.
        let want = Path::new("repo.worktrees").join("feat");
        assert!(
            add.iter()
                .any(|a| a.ends_with(want.to_string_lossy().as_ref())),
            "add argv contains the worktree path; got {add:?}"
        );
        let rm = remove_argv(Path::new("/repo"), "feat");
        assert!(rm.contains(&"remove".to_string()));
    }
}
