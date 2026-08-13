//! Rust-native `gila meeting` — Phase 3 of the gila-parity plan.
//!
//! Create a meeting note from a template: a `YYYY-MM-DD-<slug>.md` file under
//! the meetings directory (default `~/workspaces/meetings/`). The slug +
//! template composition is unit-testable; the binary's `run_*` arm owns the
//! file write.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default meetings directory, relative to `$HOME`.
pub const DEFAULT_MEETINGS_REL: &str = "workspaces/meetings";

/// Resolve the meetings directory.
pub fn meetings_dir(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_MEETINGS_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the meetings directory"),
    }
}

/// Slugify a meeting title: lowercase, non-alphanumeric runs → `-`, trimmed.
pub fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_dash = true; // suppress a leading dash
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The note filename for a date + title (`YYYY-MM-DD-<slug>.md`).
pub fn meeting_filename(date: &str, title: &str) -> String {
    let slug = slugify(title);
    if slug.is_empty() {
        format!("{date}-meeting.md")
    } else {
        format!("{date}-{slug}.md")
    }
}

/// The markdown template written for a new meeting note.
pub fn meeting_template(date: &str, title: &str) -> String {
    format!(
        "# {title}\n\n**Date:** {date}\n\n## Attendees\n\n- \n\n## Agenda\n\n- \n\n## Notes\n\n- \n\n## Action items\n\n- [ ] \n"
    )
}

/// Create the note (template) if absent and return its path. Existing notes
/// are left untouched. The date is injected so tests are hermetic.
pub fn create_meeting(dir: &Path, date: &str, title: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating meetings dir {}", dir.display()))?;
    let path = dir.join(meeting_filename(date, title));
    if !path.exists() {
        std::fs::write(&path, meeting_template(date, title))
            .with_context(|| format!("writing meeting note {}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Weekly Sync"), "weekly-sync");
        assert_eq!(slugify("  Top 5 / Status! "), "top-5-status");
        assert_eq!(slugify("Q3 Planning (final)"), "q3-planning-final");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn filename_uses_slug_or_fallback() {
        assert_eq!(
            meeting_filename("2026-08-12", "Weekly Sync"),
            "2026-08-12-weekly-sync.md"
        );
        assert_eq!(
            meeting_filename("2026-08-12", "!!!"),
            "2026-08-12-meeting.md"
        );
    }

    #[test]
    fn template_has_sections() {
        let t = meeting_template("2026-08-12", "Sync");
        assert!(t.contains("# Sync"));
        assert!(t.contains("**Date:** 2026-08-12"));
        assert!(t.contains("## Attendees"));
        assert!(t.contains("## Action items"));
    }

    #[test]
    fn create_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("meetings");
        let p = create_meeting(&dir, "2026-08-12", "Sync").unwrap();
        assert!(p.exists());
        std::fs::write(&p, "edited").unwrap();
        create_meeting(&dir, "2026-08-12", "Sync").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "edited");
    }
}
