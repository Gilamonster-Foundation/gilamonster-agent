//! Rust-native `gila daily` — Phase 3 of the gila-parity plan.
//!
//! Creates (or opens) today's daily note: a `YYYY-MM-DD-daily.md` file under
//! the daily-notes directory (default `~/.gila/daily/`). The pure path +
//! template composition is unit-testable; the binary's `run_*` arm owns the
//! file write.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default daily-notes directory, relative to `$HOME`.
pub const DEFAULT_DAILY_REL: &str = ".gila/daily";

/// Resolve the daily-notes directory (defaults to `~/.gila/daily`).
pub fn daily_dir(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_DAILY_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the daily-notes directory"),
    }
}

/// The filename for a given day's note (`YYYY-MM-DD-daily.md`).
pub fn daily_filename(date: &str) -> String {
    format!("{date}-daily.md")
}

/// The markdown template written when the note does not yet exist.
pub fn daily_template(date: &str) -> String {
    format!(
        "# {date} — Daily Notes\n\n\
         ## Top of mind\n\n- \n\n\
         ## Log\n\n- \n"
    )
}

/// Resolve today's note path and, if absent, create it from the template.
/// Returns the path either way. The date is injected so tests are hermetic.
pub fn ensure_daily_note(dir: &Path, date: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating daily-notes dir {}", dir.display()))?;
    let path = dir.join(daily_filename(date));
    if !path.exists() {
        std::fs::write(&path, daily_template(date))
            .with_context(|| format!("writing daily note {}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_is_date_prefixed() {
        assert_eq!(daily_filename("2026-08-12"), "2026-08-12-daily.md");
    }

    #[test]
    fn template_has_sections() {
        let t = daily_template("2026-08-12");
        assert!(t.contains("# 2026-08-12 — Daily Notes"));
        assert!(t.contains("## Top of mind"));
        assert!(t.contains("## Log"));
    }

    #[test]
    fn daily_dir_requires_home() {
        assert!(daily_dir(None).is_err());
        assert_eq!(
            daily_dir(Some(Path::new("/home/op"))).unwrap(),
            PathBuf::from("/home/op/.gila/daily")
        );
    }

    #[test]
    fn ensure_creates_note_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("daily");
        let p1 = ensure_daily_note(&dir, "2026-08-12").unwrap();
        assert!(p1.exists());
        let body = std::fs::read_to_string(&p1).unwrap();
        std::fs::write(&p1, "edited").unwrap();
        let p2 = ensure_daily_note(&dir, "2026-08-12").unwrap();
        assert_eq!(std::fs::read_to_string(&p2).unwrap(), "edited");
        assert!(body.contains("Daily Notes"));
    }
}
