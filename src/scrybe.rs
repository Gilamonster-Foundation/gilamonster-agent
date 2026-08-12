//! The `gila scrybe` subcommand — drives a scrybe Markdown canvas via MCP.
//!
//! **Design:** shape #1 (MCP peer). gilamonster-agent is the MCP *client*;
//! scrybe's MCP server runs alongside (or remotely over agent-mesh) and owns
//! the live document. The agent opens/updates a doc; the human edits it in
//! scrybe; edits flow back as MCP notifications. Zero new protocol — reuse
//! what scrybe already speaks.
//!
//! See `docs/design/scrybe-markdown-surface.md` for the full rationale and
//! the three-coupling-shape model (this is shape #1).

use std::path::PathBuf;

/// Configuration for a scrybe session: which scrybe MCP server to talk to
/// and which document to drive.
#[derive(Debug, Clone)]
pub struct ScrybeConfig {
    /// Endpoint of the scrybe MCP server (localhost URI or agent-mesh peer).
    pub server_uri: String,
    /// Path of the Markdown document to open/edit. Created if absent.
    pub doc_path: PathBuf,
}

impl Default for ScrybeConfig {
    fn default() -> Self {
        Self {
            server_uri: "http://localhost:3001".into(),
            doc_path: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("scrybe.md"),
        }
    }
}

/// Result of a scrybe session — captures what was driven and whether it ran.
#[derive(Debug, Clone)]
pub struct ScrybeReport {
    pub server_uri: String,
    pub doc_path: PathBuf,
    pub status: ScrybeStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScrybeStatus {
    /// Session started and is running.
    Running,
    /// Session completed normally.
    Completed,
    /// Session failed (server unreachable, doc unavailable, etc.).
    Failed(String),
}

impl std::fmt::Display for ScrybeReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "scrybe — Markdown document MCP peer")?;
        writeln!(f, "  server: {}", self.server_uri)?;
        writeln!(f, "  doc:    {}", self.doc_path.display())?;
        match &self.status {
            ScrybeStatus::Running => writeln!(f, "  status: running"),
            ScrybeStatus::Completed => writeln!(f, "  status: completed"),
            ScrybeStatus::Failed(msg) => writeln!(f, "  status: failed ({})", msg),
        }
    }
}

/// Build the default scrybe config from CLI args (positional doc path).
pub fn build_config(doc_path: Option<&str>) -> ScrybeConfig {
    let mut cfg = ScrybeConfig::default();
    if let Some(p) = doc_path {
        cfg.doc_path = PathBuf::from(p);
    }
    cfg
}

/// Validate that a scrybe config is usable (server URI looks reasonable,
/// doc path has a writable parent). Returns an error string on failure.
pub fn validate_config(cfg: &ScrybeConfig) -> Result<(), String> {
    if cfg.server_uri.is_empty() {
        return Err("scrybe: server_uri must not be empty".into());
    }
    // doc_path's parent dir must exist (or we're at the root — accept that).
    if let Some(parent) = cfg.doc_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(format!(
                "scrybe: doc parent directory does not exist: {}",
                parent.display()
            ));
        }
    }
    Ok(())
}

/// Format a scrybe report for the user. Used by `gila scrybe` CLI output.
pub fn format_report(report: &ScrybeReport) -> String {
    report.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_uri_and_doc() {
        let cfg = ScrybeConfig::default();
        assert_eq!(cfg.server_uri, "http://localhost:3001");
        // doc_path should be cwd/scrybe.md.
        let expected = std::env::current_dir()
            .unwrap_or_default()
            .join("scrybe.md");
        assert_eq!(cfg.doc_path, expected);
    }

    #[test]
    fn build_config_overrides_doc_path() {
        let cfg = build_config(Some("/tmp/foo.md"));
        assert_eq!(cfg.doc_path, PathBuf::from("/tmp/foo.md"));
        // server_uri stays default.
        assert_eq!(cfg.server_uri, "http://localhost:3001");
    }

    #[test]
    fn validate_accepts_valid_config() {
        let cfg = ScrybeConfig::default();
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn validate_rejects_empty_uri() {
        let mut cfg = ScrybeConfig::default();
        cfg.server_uri.clear();
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn validate_rejects_missing_parent_dir() {
        let cfg = ScrybeConfig {
            server_uri: "http://localhost:3001".into(),
            doc_path: PathBuf::from("/nonexistent/scrybe.md"),
        };
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn report_display_running() {
        let r = ScrybeReport {
            server_uri: "http://x".into(),
            doc_path: PathBuf::from("a.md"),
            status: ScrybeStatus::Running,
        };
        assert!(format_report(&r).contains("running"));
    }

    #[test]
    fn report_display_completed() {
        let r = ScrybeReport {
            server_uri: "http://x".into(),
            doc_path: PathBuf::from("a.md"),
            status: ScrybeStatus::Completed,
        };
        assert!(format_report(&r).contains("completed"));
    }

    #[test]
    fn report_display_failed() {
        let r = ScrybeReport {
            server_uri: "http://x".into(),
            doc_path: PathBuf::from("a.md"),
            status: ScrybeStatus::Failed("no server".into()),
        };
        assert!(format_report(&r).contains("failed (no server)"));
    }

    #[test]
    fn build_config_no_doc_uses_default() {
        let cfg = build_config(None);
        // doc_path should be the default cwd/scrybe.md.
        let expected = std::env::current_dir()
            .unwrap_or_default()
            .join("scrybe.md");
        assert_eq!(cfg.doc_path, expected);
        // server_uri stays default.
        assert_eq!(cfg.server_uri, "http://localhost:3001");
    }

    #[test]
    fn validate_accepts_root_level_doc() {
        // A doc at the filesystem root (no parent) should pass validation —
        // we only reject when a non-empty parent doesn't exist.
        let cfg = ScrybeConfig {
            server_uri: "http://localhost:3001".into(),
            doc_path: PathBuf::from("scrybe.md"),
        };
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn validate_accepts_existing_parent() {
        // Regression (Windows CI): this test hardcoded `/tmp/scrybe.md`, which
        // has no existing parent on Windows, so `validate_config` rejected it
        // and the suite was red on every PR. `env::temp_dir()` names a real,
        // existing directory on every platform.
        let cfg = ScrybeConfig {
            server_uri: "http://localhost:3001".into(),
            doc_path: std::env::temp_dir().join("scrybe.md"),
        };
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn status_equality() {
        assert_eq!(ScrybeStatus::Running, ScrybeStatus::Running);
        assert_eq!(ScrybeStatus::Completed, ScrybeStatus::Completed);
        assert_ne!(ScrybeStatus::Running, ScrybeStatus::Completed);
        // Failed variants with different messages are not equal.
        assert_ne!(
            ScrybeStatus::Failed("a".into()),
            ScrybeStatus::Failed("b".into())
        );
    }

    #[test]
    fn report_display_contains_server_uri() {
        let r = ScrybeReport {
            server_uri: "http://example.com:9000".into(),
            doc_path: PathBuf::from("/tmp/test.md"),
            status: ScrybeStatus::Completed,
        };
        assert!(format_report(&r).contains("http://example.com:9000"));
    }

    #[test]
    fn report_display_contains_doc_path() {
        let r = ScrybeReport {
            server_uri: "http://x".into(),
            doc_path: PathBuf::from("/tmp/test.md"),
            status: ScrybeStatus::Completed,
        };
        assert!(format_report(&r).contains("/tmp/test.md"));
    }

    #[test]
    fn config_clone_preserves_values() {
        let cfg = build_config(Some("/tmp/x.md"));
        let cloned = cfg.clone();
        assert_eq!(cfg.server_uri, cloned.server_uri);
        assert_eq!(cfg.doc_path, cloned.doc_path);
    }

    #[test]
    fn report_clone_preserves_values() {
        let r = ScrybeReport {
            server_uri: "http://x".into(),
            doc_path: PathBuf::from("a.md"),
            status: ScrybeStatus::Running,
        };
        let cloned = r.clone();
        assert_eq!(r.server_uri, cloned.server_uri);
        assert_eq!(r.doc_path, cloned.doc_path);
        assert_eq!(r.status, cloned.status);
    }
}
