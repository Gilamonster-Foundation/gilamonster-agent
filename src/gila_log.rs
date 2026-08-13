//! Rust-native `gila log` — Phase 3 of the gila-parity plan.
//!
//! Two subcommands:
//!
//! * `gila log activity collect` — gather the day's activity into a markdown
//!   digest: for each repo under the workspace root, the commits authored
//!   today (via git2). Pure aggregation is unit-testable over synthetic
//!   records; the git2 walk + file write are the binary's seam.
//! * `gila log prompt create` — scaffold a session-log entry
//!   (`YYYY-MM-DD-<slug>.md`) under the prompt-log directory from a template.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default prompt-log directory, relative to `$HOME`.
pub const DEFAULT_PROMPT_LOG_REL: &str = ".gila/logs/prompts";

/// Resolve the prompt-log directory.
pub fn prompt_log_dir(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_PROMPT_LOG_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the prompt-log directory"),
    }
}

/// One repo's activity for the digest: repo name + today's commit subjects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoActivity {
    /// Repo directory name.
    pub repo: String,
    /// Today's commit subjects (newest first).
    pub commits: Vec<String>,
}

/// Render the activity digest for a date as markdown.
pub fn render_activity(date: &str, activities: &[RepoActivity]) -> String {
    let mut out = format!("# Activity — {date}\n\n");
    let mut any = false;
    for a in activities {
        if a.commits.is_empty() {
            continue;
        }
        any = true;
        out.push_str(&format!("## {}\n\n", a.repo));
        for c in &a.commits {
            out.push_str(&format!("- {c}\n"));
        }
        out.push('\n');
    }
    if !any {
        out.push_str("no commits today\n");
    }
    out
}

/// Collect today's commits (author date `== date`, `YYYY-MM-DD`) for one repo.
pub fn repo_activity(path: &Path, date: &str, max: usize) -> RepoActivity {
    let repo_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut commits = Vec::new();
    if let Ok(repo) = git2::Repository::open(path) {
        if let Ok(mut rw) = repo.revwalk() {
            let _ = rw.push_head();
            for oid in rw.take(max).filter_map(|r| r.ok()) {
                if let Ok(c) = repo.find_commit(oid) {
                    let day = epoch_day_string(c.time().seconds());
                    if day == date {
                        commits.push(c.summary().unwrap_or("").to_string());
                    }
                }
            }
        }
    }
    RepoActivity {
        repo: repo_name,
        commits,
    }
}

/// The session-log filename for a date + description slug.
pub fn prompt_log_filename(date: &str, description: &str) -> String {
    let slug = crate::gila_meeting::slugify(description);
    if slug.is_empty() {
        format!("{date}-session.md")
    } else {
        format!("{date}-{slug}.md")
    }
}

/// The session-log template.
pub fn prompt_log_template(
    date: &str,
    description: &str,
    log_type: &str,
    duration: &str,
) -> String {
    format!(
        "# {description}\n\n**Date:** {date}\n**Type:** {log_type}\n**Duration:** {duration}\n\n## Summary\n\n- \n\n## Changes\n\n- \n\n## Follow-ups\n\n- [ ] \n"
    )
}

/// Create a session-log entry if absent and return its path.
pub fn create_prompt_log(
    dir: &Path,
    date: &str,
    description: &str,
    log_type: &str,
    duration: &str,
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating prompt-log dir {}", dir.display()))?;
    let path = dir.join(prompt_log_filename(date, description));
    if !path.exists() {
        std::fs::write(
            &path,
            prompt_log_template(date, description, log_type, duration),
        )
        .with_context(|| format!("writing prompt log {}", path.display()))?;
    }
    Ok(path)
}

/// Format seconds-since-epoch as `YYYY-MM-DD` (UTC) via `date` (no chrono dep).
fn epoch_day_string(secs: i64) -> String {
    std::process::Command::new("date")
        .args(["-u", "-r", &secs.to_string(), "+%F"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_log_dir_requires_home() {
        assert!(prompt_log_dir(None).is_err());
    }

    #[test]
    fn render_activity_groups_by_repo_and_handles_empty() {
        let acts = vec![
            RepoActivity {
                repo: "alpha".into(),
                commits: vec!["feat: x".into()],
            },
            RepoActivity {
                repo: "empty".into(),
                commits: vec![],
            },
        ];
        let r = render_activity("2026-08-12", &acts);
        assert!(r.contains("# Activity — 2026-08-12"));
        assert!(r.contains("## alpha"));
        assert!(r.contains("- feat: x"));
        assert!(!r.contains("## empty"));
        assert_eq!(
            render_activity("2026-08-12", &[]),
            "# Activity — 2026-08-12\n\nno commits today\n"
        );
    }

    #[test]
    fn prompt_log_filename_slugifies() {
        assert_eq!(
            prompt_log_filename("2026-08-12", "Fix the bug"),
            "2026-08-12-fix-the-bug.md"
        );
        assert_eq!(
            prompt_log_filename("2026-08-12", "!!!"),
            "2026-08-12-session.md"
        );
    }

    #[test]
    fn create_prompt_log_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("prompts");
        let p = create_prompt_log(&dir, "2026-08-12", "Sess", "feature", "~1h").unwrap();
        assert!(p.exists());
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("# Sess"));
        assert!(body.contains("**Type:** feature"));
        std::fs::write(&p, "edited").unwrap();
        create_prompt_log(&dir, "2026-08-12", "Sess", "feature", "~1h").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "edited");
    }
}
