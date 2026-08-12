//! Rust-native `gila top5` + `gila standup` — Phase 3 of the gila-parity plan.
//!
//! The deterministic slice of the weekly-status workflow: scaffold the Top-5
//! status document and the standup note from their templates. The interactive
//! interview (the LLM asks the 5 questions) stays in the assistant/pyo3 layer;
//! these commands own the markdown the interview fills in. Pure template
//! composition is unit-testable; the binary's `run_*` arm owns the file write.
//!
//! Format follows the workspace Top-5 rules (OOTO → Top 5 → Future Work →
//! Projects table); see `knowledge/docs/TOP5_FORMATTING_RULES.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default status-notes directory, relative to `$HOME`.
pub const DEFAULT_STATUS_REL: &str = ".gila/status";

/// Resolve the status-notes directory.
pub fn status_dir(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_STATUS_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the status directory"),
    }
}

/// The Top-5 status document template (the interview fills the `…` slots).
pub fn top5_template(date: &str) -> String {
    format!(
        "# Top 5 — {date}\n\n\
         ## OOTO\n\n- None\n\n\
         ## Top 5\n\n\
         1. **<initiative>** — OnGoing: … | Blockers: …\n\
         2. **<initiative>** — OnGoing: … | Blockers: …\n\
         3. **<initiative>** — OnGoing: … | Blockers: …\n\
         4. **<initiative>** — OnGoing: … | Blockers: …\n\
         5. **<initiative>** — OnGoing: … | Blockers: …\n\n\
         ## Future Work\n\n- \n\n\
         ## Projects\n\n| Project | Status | Notes |\n|---------|--------|-------|\n| | | |\n"
    )
}

/// The standup note template (yesterday / today / blockers).
pub fn standup_template(date: &str) -> String {
    format!(
        "# Standup — {date}\n\n\
         ## Yesterday\n\n- \n\n\
         ## Today\n\n- \n\n\
         ## Blockers\n\n- None\n"
    )
}

/// Create a status note of `kind` ("top5" or "standup") for `date` if absent,
/// returning its path. Existing notes are left untouched.
pub fn create_status(dir: &Path, kind: &str, date: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating status dir {}", dir.display()))?;
    let (filename, body) = match kind {
        "top5" => (format!("{date}-top5.md"), top5_template(date)),
        "standup" => (format!("{date}-standup.md"), standup_template(date)),
        other => anyhow::bail!("unknown status kind `{other}` (top5, standup)"),
    };
    let path = dir.join(filename);
    if !path.exists() {
        std::fs::write(&path, body)
            .with_context(|| format!("writing status note {}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_dir_requires_home() {
        assert!(status_dir(None).is_err());
        assert_eq!(
            status_dir(Some(Path::new("/home/op"))).unwrap(),
            PathBuf::from("/home/op/.gila/status")
        );
    }

    #[test]
    fn top5_template_has_all_sections() {
        let t = top5_template("2026-08-12");
        for s in ["## OOTO", "## Top 5", "## Future Work", "## Projects", "| Project | Status |"] {
            assert!(t.contains(s), "missing {s}");
        }
    }

    #[test]
    fn standup_template_has_all_sections() {
        let t = standup_template("2026-08-12");
        for s in ["## Yesterday", "## Today", "## Blockers"] {
            assert!(t.contains(s), "missing {s}");
        }
    }

    #[test]
    fn create_status_routes_kind_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("status");
        let t = create_status(&dir, "top5", "2026-08-12").unwrap();
        let s = create_status(&dir, "standup", "2026-08-12").unwrap();
        assert!(t.ends_with("2026-08-12-top5.md"));
        assert!(s.ends_with("2026-08-12-standup.md"));
        assert!(create_status(&dir, "wat", "2026-08-12").is_err());
        std::fs::write(&t, "edited").unwrap();
        create_status(&dir, "top5", "2026-08-12").unwrap();
        assert_eq!(std::fs::read_to_string(&t).unwrap(), "edited");
    }
}
