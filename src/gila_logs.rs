//! Rust-native `gila logs` — Phase 3 of the gila-parity plan.
//!
//! View the gila activity/prompt logs: the newest `*.md` log files under the
//! log directory (default `~/.gila/logs/`), most-recent first, capped at
//! `--limit`. Pure scanning + ordering is unit-testable; the binary's
//! `run_*` arm owns the root resolution + print.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Default log directory, relative to `$HOME`.
pub const DEFAULT_LOGS_REL: &str = ".gila/logs";

/// Resolve the log directory.
pub fn logs_dir(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_LOGS_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the log directory"),
    }
}

/// A log entry: its path plus modified-time (seconds since epoch, for sort).
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Path to the log file.
    pub path: PathBuf,
    /// Modified time as seconds since the Unix epoch (0 when unknown).
    pub mtime_secs: u64,
}

/// List the `*.md` logs under `dir`, most-recent-modified first, capped at
/// `limit`. A missing/empty dir yields an empty list.
pub fn recent_logs(dir: &Path, limit: usize) -> Vec<LogEntry> {
    let mut entries: Vec<LogEntry> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
                .map(|p| {
                    let mtime_secs = p
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    LogEntry { path: p, mtime_secs }
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
    entries.truncate(limit);
    entries
}

/// Render log entries as display lines (file names, most-recent first).
pub fn render_logs(entries: &[LogEntry]) -> String {
    if entries.is_empty() {
        return "no logs found\n".to_string();
    }
    let mut out = String::new();
    for e in entries {
        let name = e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push_str(&name);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn logs_dir_requires_home() {
        assert!(logs_dir(None).is_err());
        assert_eq!(
            logs_dir(Some(Path::new("/home/op"))).unwrap(),
            PathBuf::from("/home/op/.gila/logs")
        );
    }

    #[test]
    fn recent_logs_orders_newest_first_and_caps() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, content) in [("old.md", "x"), ("mid.md", "xx"), ("new.md", "xxx")] {
            let mut f = std::fs::File::create(tmp.path().join(name)).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }
        std::fs::write(tmp.path().join("ignore.txt"), "").unwrap();
        let all = recent_logs(tmp.path(), 10);
        assert_eq!(all.len(), 3);
        // mtimes are monotonically non-increasing.
        assert!(all[0].mtime_secs >= all[1].mtime_secs);
        assert!(all[1].mtime_secs >= all[2].mtime_secs);
        // Cap respected.
        assert_eq!(recent_logs(tmp.path(), 2).len(), 2);
        // txt filtered out.
        assert!(all.iter().all(|e| e.path.extension().unwrap() == "md"));
    }

    #[test]
    fn render_handles_empty() {
        assert_eq!(render_logs(&[]), "no logs found\n");
    }
}
