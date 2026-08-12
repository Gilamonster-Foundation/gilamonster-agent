//! Rust-native `gila init` — Phase 3 of the gila-parity plan.
//!
//! Initialize the gila config directory: create `~/.gila/` (and the standard
//! subdirs the file-based commands use: `daily`, `logs`, `cache`) if absent.
//! Idempotent — existing dirs are left untouched. Pure path list is
//! unit-testable; the binary's `run_*` arm owns the create.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default config root, relative to `$HOME`.
pub const DEFAULT_GILA_REL: &str = ".gila";

/// The standard subdirectories `init` ensures under the config root.
pub const STANDARD_SUBDIRS: &[&str] = &["daily", "logs", "cache"];

/// Resolve the gila config root.
pub fn gila_root(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_GILA_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the gila config root"),
    }
}

/// The full set of directories `init` ensures (root + standard subdirs).
pub fn init_dirs(home: Option<&Path>) -> Result<Vec<PathBuf>> {
    let root = gila_root(home)?;
    let mut dirs = vec![root.clone()];
    dirs.extend(STANDARD_SUBDIRS.iter().map(|s| root.join(s)));
    Ok(dirs)
}

/// Create the config root + standard subdirs. Returns the dirs created
/// (already-existing dirs are skipped). Idempotent.
pub fn init(home: Option<&Path>) -> Result<Vec<PathBuf>> {
    let mut created = Vec::new();
    for d in init_dirs(home)? {
        if !d.exists() {
            std::fs::create_dir_all(&d)
                .with_context(|| format!("creating {}", d.display()))?;
            created.push(d);
        }
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gila_root_requires_home() {
        assert!(gila_root(None).is_err());
        assert_eq!(
            gila_root(Some(Path::new("/home/op"))).unwrap(),
            PathBuf::from("/home/op/.gila")
        );
    }

    #[test]
    fn init_dirs_covers_root_and_subdirs() {
        let dirs = init_dirs(Some(Path::new("/home/op"))).unwrap();
        assert_eq!(dirs.len(), 1 + STANDARD_SUBDIRS.len());
        assert!(dirs[0].ends_with(".gila"));
        assert!(dirs.iter().any(|d| d.ends_with("daily")));
        assert!(dirs.iter().any(|d| d.ends_with("logs")));
        assert!(dirs.iter().any(|d| d.ends_with("cache")));
    }

    #[test]
    fn init_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let created1 = init(Some(home)).unwrap();
        assert_eq!(created1.len(), 1 + STANDARD_SUBDIRS.len());
        let created2 = init(Some(home)).unwrap();
        assert!(created2.is_empty());
        // All dirs exist after init.
        for d in init_dirs(Some(home)).unwrap() {
            assert!(d.exists());
        }
    }
}
