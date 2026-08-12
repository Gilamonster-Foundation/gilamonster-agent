//! Rust-native `gila ideas` — Phase 3 of the gila-parity plan.
//!
//! Appends a one-line idea to the idea-capture file (default
//! `~/.gila/ideas.md`), or lists captured ideas with `--list`. Pure line
//! formatting is unit-testable; the binary's `run_*` arm owns the file I/O.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default idea-capture file, relative to `$HOME`.
pub const DEFAULT_IDEAS_REL: &str = ".gila/ideas.md";

/// Resolve the idea-capture file path.
pub fn ideas_path(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_IDEAS_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the ideas file"),
    }
}

/// The markdown line appended for one idea (ISO date prefix + text).
pub fn idea_line(date: &str, idea: &str) -> String {
    format!("- [{date}] {idea}\n")
}

/// Append an idea, creating the file (and a header) if absent. Returns the path.
pub fn append_idea(path: &Path, date: &str, idea: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating ideas dir {}", parent.display()))?;
    }
    if !path.exists() {
        std::fs::write(path, "# Ideas\n\n")
            .with_context(|| format!("writing ideas file {}", path.display()))?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("opening ideas file {}", path.display()))?;
    f.write_all(idea_line(date, idea).as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_is_date_prefixed_bullet() {
        assert_eq!(idea_line("2026-08-12", "ship it"), "- [2026-08-12] ship it\n");
    }

    #[test]
    fn ideas_path_requires_home() {
        assert!(ideas_path(None).is_err());
        assert_eq!(
            ideas_path(Some(Path::new("/home/op"))).unwrap(),
            PathBuf::from("/home/op/.gila/ideas.md")
        );
    }

    #[test]
    fn append_creates_then_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("ideas.md");
        append_idea(&p, "2026-08-12", "first").unwrap();
        append_idea(&p, "2026-08-12", "second").unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.starts_with("# Ideas\n"));
        assert!(body.contains("- [2026-08-12] first\n"));
        assert!(body.contains("- [2026-08-12] second\n"));
    }
}
