//! Rust-native `gila cache` — Phase 3 of the gila-parity plan.
//!
//! Manage the gilabot cache directory (default `~/.gila/cache/`): `status`
//! reports size + entry count, `clear` empties it. Pure path resolution and
//! the size/count walk are unit-testable; the binary's `run_*` arm owns the
//! delete + print.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default cache directory, relative to `$HOME`.
pub const DEFAULT_CACHE_REL: &str = ".gila/cache";

/// Resolve the cache directory.
pub fn cache_dir(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_CACHE_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the cache directory"),
    }
}

/// A cache status snapshot: total bytes + file count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStatus {
    /// Total size of all files under the cache dir, in bytes.
    pub bytes: u64,
    /// Number of files (not directories) under the cache dir.
    pub files: usize,
}

/// Walk the cache dir and total its size + file count. Missing dir = empty.
pub fn status(dir: &Path) -> CacheStatus {
    let mut s = CacheStatus { bytes: 0, files: 0 };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(m) = p.metadata() {
                    s.bytes += m.len();
                    s.files += 1;
                }
            }
        }
    }
    s
}

/// Render a status snapshot as a display line.
pub fn render_status(dir: &Path, s: &CacheStatus) -> String {
    format!(
        "{}: {} file(s), {} byte(s)\n",
        dir.display(),
        s.files,
        s.bytes
    )
}

/// Empty the cache dir (remove all contents, keep the dir). Returns the count
/// of top-level entries removed. Missing dir = no-op (0).
pub fn clear(dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for e in std::fs::read_dir(dir)
        .with_context(|| format!("reading cache dir {}", dir.display()))?
        .filter_map(|e| e.ok())
    {
        let p = e.path();
        if p.is_dir() {
            std::fs::remove_dir_all(&p).ok();
        } else {
            std::fs::remove_file(&p).ok();
        }
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_requires_home() {
        assert!(cache_dir(None).is_err());
        assert_eq!(
            cache_dir(Some(Path::new("/home/op"))).unwrap(),
            PathBuf::from("/home/op/.gila/cache")
        );
    }

    #[test]
    fn status_totals_bytes_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.path().join("a"), b"1234").unwrap();
        std::fs::write(sub.join("b"), b"12").unwrap();
        let s = status(tmp.path());
        assert_eq!(s.files, 2);
        assert_eq!(s.bytes, 6);
    }

    #[test]
    fn status_missing_dir_is_empty() {
        let s = status(Path::new("/nonexistent-gila-cache"));
        assert_eq!(s, CacheStatus { bytes: 0, files: 0 });
    }

    #[test]
    fn render_status_line() {
        let s = CacheStatus { bytes: 6, files: 2 };
        let r = render_status(Path::new("/c"), &s);
        assert!(r.contains("2 file(s)"));
        assert!(r.contains("6 byte(s)"));
    }

    #[test]
    fn clear_empties_but_keeps_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cache");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("f"), b"x").unwrap();
        let n = clear(&dir).unwrap();
        assert_eq!(n, 2);
        assert!(dir.exists());
        assert_eq!(status(&dir), CacheStatus { bytes: 0, files: 0 });
        assert_eq!(clear(Path::new("/nonexistent-gila-cache")).unwrap(), 0);
    }
}
