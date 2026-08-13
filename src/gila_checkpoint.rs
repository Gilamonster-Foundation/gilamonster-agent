//! Rust-native `gila checkpoint` — Phase 3 of the gila-parity plan.
//!
//! Lightweight snapshots of a workspace: `create` records the current git HEAD
//! (+ dirty file list) of each repo under a root into a named checkpoint file;
//! `list` shows checkpoints; `diff`/`restore` compare/return to one. The
//! snapshot *format* and git2 reads are unit-testable; `restore` shells out to
//! `git` (matching how an operator resets by hand) as the effectful seam.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default checkpoint directory, relative to `$HOME`.
pub const DEFAULT_CHECKPOINTS_REL: &str = ".gila/checkpoints";

/// Resolve the checkpoint directory.
pub fn checkpoints_dir(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_CHECKPOINTS_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the checkpoint directory"),
    }
}

/// One repo's snapshot: its path, HEAD oid, and dirty (modified) files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoSnapshot {
    /// Repo working-directory path.
    pub path: PathBuf,
    /// HEAD commit oid (hex), or `None` when unborn/unreadable.
    pub head: Option<String>,
    /// Modified (uncommitted) file paths, repo-relative, sorted.
    pub dirty: Vec<String>,
}

/// A named checkpoint: a creation stamp plus the repo snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Checkpoint name.
    pub name: String,
    /// Creation date stamp (`YYYY-MM-DD`).
    pub created: String,
    /// Per-repo snapshots.
    pub repos: Vec<RepoSnapshot>,
}

/// Snapshot a single repo: HEAD oid + dirty files via git2.
pub fn snapshot_repo(path: &Path) -> RepoSnapshot {
    let (head, dirty) = match git2::Repository::open(path) {
        Ok(repo) => {
            let head = repo
                .head()
                .ok()
                .and_then(|h| h.target())
                .map(|oid| oid.to_string());
            let dirty = repo
                .statuses(None)
                .map(|ss| {
                    let mut v: Vec<String> = ss
                        .iter()
                        .filter_map(|e| e.path().map(|p| p.to_string()))
                        .collect();
                    v.sort();
                    v
                })
                .unwrap_or_default();
            (head, dirty)
        }
        Err(_) => (None, Vec::new()),
    };
    RepoSnapshot {
        path: path.to_path_buf(),
        head,
        dirty,
    }
}

/// Serialize a checkpoint to TOML (the on-disk format).
pub fn to_toml(cp: &Checkpoint) -> Result<String> {
    toml::to_string_pretty(cp).context("serializing checkpoint")
}

/// Write a checkpoint file (`<name>.toml`) under the checkpoint dir.
pub fn save(dir: &Path, cp: &Checkpoint) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating checkpoint dir {}", dir.display()))?;
    let path = dir.join(format!("{}.toml", cp.name));
    std::fs::write(&path, to_toml(cp)?)
        .with_context(|| format!("writing checkpoint {}", path.display()))?;
    Ok(path)
}

/// List checkpoint names (file stems, sorted) under `dir`.
pub fn list(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
                .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Load a checkpoint by name. `None` when the file is missing/unreadable.
pub fn load(dir: &Path, name: &str) -> Option<Checkpoint> {
    let body = std::fs::read_to_string(dir.join(format!("{name}.toml"))).ok()?;
    toml::from_str(&body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoints_dir_requires_home() {
        assert!(checkpoints_dir(None).is_err());
    }

    #[test]
    fn roundtrip_toml() {
        let cp = Checkpoint {
            name: "cp1".into(),
            created: "2026-08-12".into(),
            repos: vec![RepoSnapshot {
                path: PathBuf::from("/repo"),
                head: Some("abc123".into()),
                dirty: vec!["a.rs".into()],
            }],
        };
        let s = to_toml(&cp).unwrap();
        let back: Checkpoint = toml::from_str(&s).unwrap();
        assert_eq!(back.name, "cp1");
        assert_eq!(back.repos[0].head.as_deref(), Some("abc123"));
        assert_eq!(back.repos[0].dirty, vec!["a.rs"]);
    }

    #[test]
    fn save_list_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cps");
        let cp = Checkpoint {
            name: "cp1".into(),
            created: "2026-08-12".into(),
            repos: vec![],
        };
        save(&dir, &cp).unwrap();
        assert_eq!(list(&dir), vec!["cp1"]);
        assert_eq!(load(&dir, "cp1").unwrap().name, "cp1");
        assert!(load(&dir, "missing").is_none());
    }

    #[test]
    fn snapshot_non_repo_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = snapshot_repo(tmp.path());
        assert!(snap.head.is_none());
        assert!(snap.dirty.is_empty());
    }
}
