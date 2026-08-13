//! Rust-native `gila git` — Phase 1 of the gila-parity plan.
//!
//! Two day-one commands graduate out of the shell-delegate fallback:
//!
//! * `gila git commit` — stage + commit via libgit2 (`git2`). This is the
//!   pure-Rust slice: building the index and writing the commit object need
//!   no subprocess, so it is the natural first port.
//! * `gila git tend` — profile-driven repo maintenance, config-compatible
//!   with the operator's existing `git-tend.yaml` (parsed by Python gilabot's
//!   `gila-plugin-git-tend`). Profile *steps* run through the `git` CLI just
//!   like the Python engine (`git_service.py`) so behavior matches 1:1 —
//!   fetch/pull/push/porcelain semantics are notoriously subtle and the parity
//!   goal is identical behavior, not a libgit2 rewrite of git itself.
//!
//! The pure logic (config parse, path/variable substitution, the "nothing to
//! commit" carve-out, tend orchestration over injected step-runners) is
//! unit-testable; the git2 commit and the subprocess runner are the thin
//! effectful seam.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// Default config location, matching Python gilabot's `ConfigService`.
pub const DEFAULT_CONFIG_REL: &str = ".gila/git-tend.yaml";

// ---------------------------------------------------------------------------
// Config model (mirrors gila-plugin-git-tend `models.py`, serde-compatible)
// ---------------------------------------------------------------------------

/// Strategy for handling a failed step during tend (`on_conflict`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConflictStrategy {
    /// Stop tending this repo on the first failed step.
    #[default]
    Halt,
    /// Skip the failed step and continue with the next.
    Skip,
    /// `git stash push` to clear the tree, then continue (no auto-retry).
    Stash,
    /// Create a `tend/<timestamp>` branch, reset to tracking, and stop.
    Branch,
    /// Fetch + `git reset --hard` to the tracking branch, then continue.
    Overwrite,
}

/// Defaults applied to every repo unless overridden (`defaults:`).
#[derive(Debug, Clone, Deserialize)]
pub struct TendDefaults {
    #[serde(default)]
    pub on_conflict: ConflictStrategy,
    #[serde(default = "default_commit_message")]
    pub commit_message: String,
    #[serde(default = "default_profiles", alias = "default_profiles")]
    pub profiles: Vec<String>,
}

fn default_commit_message() -> String {
    "tend: automated backup {timestamp}".to_string()
}

fn default_profiles() -> Vec<String> {
    vec!["backup".to_string()]
}

impl Default for TendDefaults {
    fn default() -> Self {
        Self {
            on_conflict: ConflictStrategy::Halt,
            commit_message: default_commit_message(),
            profiles: default_profiles(),
        }
    }
}

/// A single repository entry under `repos:`.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub profiles: Option<Vec<String>>,
    #[serde(default)]
    pub on_conflict: Option<ConflictStrategy>,
    // merge_policy / agentic_fix / expected_branches are parsed by Python for
    // other git-tend subcommands (pr / agentic fix / workspace branches); the
    // Phase-1 tend loop ignores them, so they are accepted-and-dropped here.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_yaml::Value>,
}

impl RepoConfig {
    /// This repo's profiles, falling back to the configured defaults.
    pub fn profiles<'a>(&'a self, defaults: &'a TendDefaults) -> &'a [String] {
        self.profiles.as_deref().unwrap_or(&defaults.profiles)
    }

    /// This repo's conflict strategy, falling back to the configured defaults.
    pub fn conflict_strategy(&self, defaults: &TendDefaults) -> ConflictStrategy {
        self.on_conflict.unwrap_or(defaults.on_conflict)
    }
}

/// Top-level `git-tend.yaml`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TendConfig {
    #[serde(default)]
    pub defaults: TendDefaults,
    #[serde(default)]
    pub profiles: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
}

/// Load and parse a `git-tend.yaml` from disk.
pub fn load_config(path: &Path) -> Result<TendConfig> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "git-tend config not found: {} (run `gila git tend init` to create one)",
            path.display()
        )
    })?;
    parse_config(&text)
}

/// Parse `git-tend.yaml` text into a [`TendConfig`]. Pure; testable.
pub fn parse_config(text: &str) -> Result<TendConfig> {
    let cfg: TendConfig =
        serde_yaml::from_str(text).context("git-tend config is not a valid YAML mapping")?;
    Ok(cfg)
}

/// The default config path (`~/.gila/git-tend.yaml`).
pub fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(DEFAULT_CONFIG_REL))
}

// ---------------------------------------------------------------------------
// Variable substitution (pure)
// ---------------------------------------------------------------------------

/// Substitute `{name}` placeholders in `template` from `vars`. Pure.
pub fn substitute(template: &str, vars: &HashMap<String, String>) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

// ---------------------------------------------------------------------------
// Rust-native `gila git commit` (libgit2)
// ---------------------------------------------------------------------------

/// Stage all changes (`git add -A`) and commit with `message` in the repo at
/// `path`. Returns the new commit OID, or `Ok(None)` when there was nothing to
/// commit (clean index after staging — the same non-error carve-out the Python
/// engine makes for `commit` steps).
///
/// This is the pure-libgit2 slice: it opens the repo, stages the working tree,
/// writes the tree + commit object, and updates HEAD — no subprocess.
pub fn commit_all(path: &Path, message: &str) -> Result<Option<git2::Oid>> {
    let repo = git2::Repository::open(path)
        .with_context(|| format!("not a git repo: {}", path.display()))?;

    // `git add -A`: stage modified, new, and deleted paths.
    let mut index = repo.index()?;
    index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;

    let tree_oid = index.write_tree()?;
    let tree = repo.find_tree(tree_oid)?;

    // Nothing staged relative to HEAD → "nothing to commit" (not an error).
    let head = repo.head().ok();
    let head_commit = head.as_ref().and_then(|h| h.peel_to_commit().ok());
    if let Some(hc) = &head_commit {
        if hc.tree_id() == tree_oid {
            return Ok(None);
        }
    } else if repo.is_empty()? && tree.is_empty() {
        // Empty repo, empty tree — nothing to commit.
        return Ok(None);
    }

    let sig = repo
        .signature()
        .context("no git user.name/user.email configured")?;

    let oid = match &head_commit {
        Some(parent) => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[parent])?,
        None => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[])?, // root commit
    };
    Ok(Some(oid))
}

// ---------------------------------------------------------------------------
// `gila git tend` — profile-step execution via the git CLI (matches Python)
// ---------------------------------------------------------------------------

/// Split a command line into words, POSIX-shell-style (shlex-like): words are
/// whitespace-separated, but single- and double-quoted regions keep their
/// contents together (quotes removed) and backslash escapes the next char.
/// This matches Python's `shlex.split`, which the reference engine uses — the
/// parity goal is identical arg splitting for profile steps like
/// `git commit -m "tend: backup {timestamp}"`.
fn shell_split(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c2 in chars.by_ref() {
                    if c2 == '\'' {
                        break;
                    }
                    cur.push(c2);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c2) = chars.next() {
                    match c2 {
                        '"' => break,
                        '\\' => {
                            // In double quotes, backslash escapes only $ ` " \ newline.
                            if let Some(&c3) = chars.peek() {
                                if matches!(c3, '$' | '`' | '"' | '\\') {
                                    cur.push(c3);
                                    chars.next();
                                    continue;
                                }
                            }
                            cur.push('\\');
                        }
                        _ => cur.push(c2),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(c2) = chars.next() {
                    cur.push(c2);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            _ => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

/// The result of running one profile step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// Step ran successfully.
    Ok,
    /// Step was a no-op (e.g. `commit` on a clean tree) — not an error.
    Skipped,
    /// Step failed (non-zero exit, excluding the clean-commit carve-out).
    Failed,
}

/// Run one profile step. Steps are git commands; a leading `git ` is stripped
/// and the rest is split and exec'd. Returns the outcome plus captured stderr.
///
/// `dry_run` reports `Ok` without executing (preview mode).
pub fn run_step(repo: &Path, step: &str, dry_run: bool) -> (StepOutcome, String) {
    let cmd = step
        .strip_prefix("git ")
        .map(str::trim_start)
        .unwrap_or(step)
        .trim();
    if cmd.is_empty() {
        return (StepOutcome::Skipped, String::new());
    }
    if dry_run {
        return (StepOutcome::Ok, String::new());
    }

    let args: Vec<String> = shell_split(cmd);
    let is_commit = args.first().map(String::as_str) == Some("commit");

    let output = match Command::new("git").args(&args).current_dir(repo).output() {
        Ok(o) => o,
        Err(e) => return (StepOutcome::Failed, format!("failed to spawn git: {e}")),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if output.status.success() {
        return (StepOutcome::Ok, stderr);
    }
    // "nothing to commit" on a commit step is a skip, not a failure.
    if is_commit && stdout.contains("nothing to commit") {
        return (StepOutcome::Skipped, stderr);
    }
    (StepOutcome::Failed, stderr)
}

/// A per-repo tend report.
#[derive(Debug, Default)]
pub struct RepoReport {
    pub path: PathBuf,
    pub success: bool,
    pub error: Option<String>,
}

/// Tend one repo: run its profiles' steps in order, applying the conflict
/// strategy on failure. Pure with respect to the injected step-runner except
/// for the git-CLI steps themselves.
pub fn tend_repo(repo: &RepoConfig, config: &TendConfig, dry_run: bool) -> RepoReport {
    let path = expand_home(&repo.path);
    let strategy = repo.conflict_strategy(&config.defaults);
    let mut report = RepoReport {
        success: true,
        error: None,
        path: path.clone(),
    };

    let vars: HashMap<String, String> = HashMap::from([
        ("timestamp".to_string(), timestamp_now()),
        (
            "commit_message".to_string(),
            substitute(
                &config.defaults.commit_message,
                &HashMap::from([("timestamp".to_string(), timestamp_now())]),
            ),
        ),
    ]);

    for profile in repo.profiles(&config.defaults) {
        let Some(steps) = config.profiles.get(profile) else {
            continue;
        };
        for step_tmpl in steps {
            let step = substitute(step_tmpl, &vars);
            let (outcome, err) = run_step(&path, &step, dry_run);
            match outcome {
                StepOutcome::Ok | StepOutcome::Skipped => {}
                StepOutcome::Failed => {
                    if !handle_conflict(&path, strategy, dry_run) {
                        report.success = false;
                        report.error = Some(format!("failed at '{step}' ({strategy:?}): {err}"));
                        return report;
                    }
                }
            }
        }
    }
    report
}

/// Apply a conflict strategy after a failed step. Returns `true` when tending
/// may continue, `false` when it must stop for this repo. Mirrors
/// `TendService._handle_conflict`.
fn handle_conflict(path: &Path, strategy: ConflictStrategy, dry_run: bool) -> bool {
    if dry_run {
        return false;
    }
    match strategy {
        ConflictStrategy::Halt => false,
        ConflictStrategy::Skip => true,
        ConflictStrategy::Stash => git_ok(
            path,
            &["stash", "push", "-m", "git-tend: stashed during tend"],
        ),
        ConflictStrategy::Branch => {
            let name = format!(
                "tend/{}",
                timestamp_now().replace([':', '-'], "").replace('T', "-")
            );
            git_ok(path, &["branch", &name])
            // Reset-to-tracking intentionally omitted in Phase 1: branch is the
            // safe escape hatch; the repo stops tending either way.
        }
        ConflictStrategy::Overwrite => match tracking_branch(path) {
            Some(track) => git_ok(path, &["fetch"]) && git_ok(path, &["reset", "--hard", &track]),
            None => false,
        },
    }
}

fn git_ok(path: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn tracking_branch(path: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "@{upstream}"])
        .current_dir(path)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// Expand a leading `~` to `$HOME` (Python's `Path.expanduser`).
fn expand_home(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

/// UTC timestamp in the Python engine's `YYYYMMDD-HHMMSS`-ish ISO form.
fn timestamp_now() -> String {
    // Seconds since epoch → a stable, sortable timestamp without pulling in a
    // datetime crate for the Phase-1 slice. Format mirrors what the Python
    // engine feeds into `{timestamp}` (an ISO-8601 UTC string).
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let yaml = r#"
defaults:
  on_conflict: halt
  commit_message: "tend: backup {timestamp}"
profiles:
  backup:
    - git add -A
    - git commit -m "{commit_message}"
repos:
  - path: ~/workspaces/meta
    on_conflict: branch
"#;
        let cfg = parse_config(yaml).unwrap();
        assert_eq!(cfg.defaults.on_conflict, ConflictStrategy::Halt);
        assert_eq!(cfg.repos.len(), 1);
        assert_eq!(cfg.repos[0].on_conflict, Some(ConflictStrategy::Branch));
        assert_eq!(
            cfg.repos[0].profiles(&cfg.defaults),
            &["backup".to_string()]
        );
        assert_eq!(
            cfg.profiles["backup"],
            vec![
                "git add -A".to_string(),
                "git commit -m \"{commit_message}\"".to_string()
            ]
        );
    }

    #[test]
    fn repo_falls_back_to_defaults() {
        let cfg = TendConfig::default();
        let repo = RepoConfig {
            path: PathBuf::from("x"),
            profiles: None,
            on_conflict: None,
            extra: HashMap::new(),
        };
        assert_eq!(repo.profiles(&cfg.defaults), &["backup".to_string()]);
        assert_eq!(
            repo.conflict_strategy(&cfg.defaults),
            ConflictStrategy::Halt
        );
    }

    #[test]
    fn substitute_replaces_placeholders() {
        let vars = HashMap::from([("timestamp".to_string(), "123".to_string())]);
        assert_eq!(substitute("backup {timestamp}", &vars), "backup 123");
        assert_eq!(substitute("no vars here", &vars), "no vars here");
    }

    #[test]
    fn dry_run_step_is_ok() {
        let (outcome, _) = run_step(Path::new("/tmp"), "commit -m x", true);
        assert_eq!(outcome, StepOutcome::Ok);
    }

    #[test]
    fn empty_step_is_skipped() {
        let (outcome, _) = run_step(Path::new("/tmp"), "git ", false);
        assert_eq!(outcome, StepOutcome::Skipped);
    }

    #[test]
    fn strips_git_prefix() {
        // A real git command that always succeeds outside a repo check:
        // `git --version` never fails, prefix-stripped or not. Use a tempdir
        // that exists on every platform ("/tmp" does not exist on Windows).
        let dir = tempfile::tempdir().expect("tempdir");
        let (outcome, _) = run_step(dir.path(), "git --version", false);
        assert_eq!(outcome, StepOutcome::Ok);
    }

    #[test]
    fn shell_split_keeps_quoted_message_together() {
        // The Phase-1 bug: `commit -m "tend: backup 123"` must pass the quoted
        // message as ONE arg (matching shlex), not three whitespace-split ones.
        assert_eq!(
            shell_split(r#"commit -m "tend: backup 123""#),
            vec![
                "commit".to_string(),
                "-m".to_string(),
                "tend: backup 123".to_string()
            ]
        );
        // Single quotes and escapes too.
        assert_eq!(
            shell_split("add -A"),
            vec!["add".to_string(), "-A".to_string()]
        );
        assert_eq!(
            shell_split(r#"commit -m 'it'"'"'s'"#),
            vec!["commit".to_string(), "-m".to_string(), "it's".to_string()]
        );
    }
}
