//! Rust-native `gila insights` — Phase 3 of the gila-parity plan.
//!
//! Git activity analytics for a repo: commit counts by author and by day over
//! a recent window, read via git2 (no subprocess). Pure aggregation is
//! unit-testable over a synthetic log; the binary's `run_*` arm owns the repo
//! open + print.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

/// An activity report: commit counts grouped by author and by day.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Insights {
    /// Total commits counted.
    pub total: usize,
    /// Commits per author (sorted by name).
    pub by_author: BTreeMap<String, usize>,
    /// Commits per `YYYY-MM-DD` day (sorted ascending).
    pub by_day: BTreeMap<String, usize>,
}

/// Aggregate `(author, day)` pairs into an [`Insights`]. Pure; the git2 walk
/// feeds these pairs in.
pub fn aggregate<I: IntoIterator<Item = (String, String)>>(commits: I) -> Insights {
    let mut out = Insights::default();
    for (author, day) in commits {
        out.total += 1;
        *out.by_author.entry(author).or_insert(0) += 1;
        *out.by_day.entry(day).or_insert(0) += 1;
    }
    out
}

/// Walk the repo's HEAD history (up to `max` commits) and aggregate activity.
/// The day string comes from each commit's author timestamp (`date +%F` shape).
pub fn repo_insights(path: &Path, max: usize) -> Result<Insights> {
    let repo =
        git2::Repository::open(path).with_context(|| format!("opening repo {}", path.display()))?;
    let mut revwalk = repo.revwalk().context("creating revwalk")?;
    revwalk.push_head().context("pushing HEAD")?;
    let mut pairs = Vec::new();
    for oid in revwalk.take(max).filter_map(|r| r.ok()) {
        if let Ok(commit) = repo.find_commit(oid) {
            let author = commit.author().name().unwrap_or("unknown").to_string();
            let secs = commit.time().seconds();
            let day = epoch_day_string(secs);
            pairs.push((author, day));
        }
    }
    Ok(aggregate(pairs))
}

/// Render an [`Insights`] as a display block.
pub fn render(ins: &Insights) -> String {
    let mut out = format!("total commits: {}\n\nby author:\n", ins.total);
    for (a, n) in &ins.by_author {
        out.push_str(&format!("  {a}: {n}\n"));
    }
    out.push_str("\nby day:\n");
    for (d, n) in &ins.by_day {
        out.push_str(&format!("  {d}: {n}\n"));
    }
    out
}

/// Format seconds-since-epoch as `YYYY-MM-DD` (UTC). Uses the `date` binary to
/// avoid a chrono dep; falls back to the raw seconds when `date` fails.
fn epoch_day_string(secs: i64) -> String {
    std::process::Command::new("date")
        .args(["-u", "-r", &secs.to_string(), "+%F"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_counts_author_and_day() {
        let ins = aggregate([
            ("alice".to_string(), "2026-08-11".to_string()),
            ("alice".to_string(), "2026-08-12".to_string()),
            ("bob".to_string(), "2026-08-12".to_string()),
        ]);
        assert_eq!(ins.total, 3);
        assert_eq!(ins.by_author["alice"], 2);
        assert_eq!(ins.by_author["bob"], 1);
        assert_eq!(ins.by_day["2026-08-12"], 2);
        assert_eq!(ins.by_day["2026-08-11"], 1);
    }

    #[test]
    fn render_lists_sections() {
        let ins = aggregate([("alice".to_string(), "2026-08-12".to_string())]);
        let r = render(&ins);
        assert!(r.contains("total commits: 1"));
        assert!(r.contains("alice: 1"));
        assert!(r.contains("2026-08-12: 1"));
    }
}
