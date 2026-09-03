//! The herder cockpit's agent **palette manifest**, `~/.gila/agents.toml`.
//!
//! The cockpit's command palette (`docs/design/herder-cockpit.md`) spawns
//! subordinate agent harnesses, newt, claude, codex, a plain shell, into PTY
//! panes. This manifest is where the operator declares which argv each palette
//! entry runs, herdr-style. This module is pure parsing + validation; it does
//! not spawn anything (that is `pane_backend.rs`'s job) and does not depend on
//! newt's config types.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// How a pane spawned from this entry is confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Posture {
    /// Spawned through `agent_bridle_core::spawn_confined_subprocess`, the
    /// safe default (matches the cockpit's `ambient_native_shell_default`
    /// decision for non-shell panes).
    #[default]
    Confined,
    /// Spawned with no leash. Requires an explicit opt-in per entry.
    Ambient,
}

/// One `[[agents]]` entry, a palette-spawnable agent harness or shell.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AgentEntry {
    /// The palette label (also the dedupe key, must be unique in the file).
    pub name: String,
    /// The argv to spawn: `argv[0]` is the executable, the rest are arguments.
    pub argv: Vec<String>,
    /// One-line description shown in the palette.
    pub description: String,
    /// Working directory for the spawned process. Defaults to the cockpit's
    /// own cwd when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra environment variables to grant the subprocess, `KEY=VALUE`.
    /// Nothing else reaches it, the external-boundary invariant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// Confinement posture for panes spawned from this entry.
    #[serde(default)]
    pub posture: Posture,
}

impl AgentEntry {
    /// Fail loud on the shapes that would otherwise silently break the
    /// palette or the spawn: an empty name, an empty argv, or a malformed
    /// `env` entry (must be `KEY=VALUE`, non-empty key).
    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("agents.toml: an [[agents]] entry has an empty name");
        }
        if self.argv.is_empty() {
            bail!("agents.toml: agent '{}' has an empty argv", self.name);
        }
        for kv in &self.env {
            match kv.split_once('=') {
                Some((key, _)) if !key.trim().is_empty() => {}
                _ => bail!(
                    "agents.toml: agent '{}' has a malformed env entry '{}' (want KEY=VALUE)",
                    self.name,
                    kv
                ),
            }
        }
        Ok(())
    }
}

/// The parsed `agents.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct AgentManifest {
    /// The `[[agents]]` array.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentEntry>,
}

impl AgentManifest {
    /// Parse manifest TOML, then validate. An empty document is the empty
    /// manifest (no palette entries beyond the built-in defaults callers may
    /// layer on top).
    pub fn parse(toml_str: &str) -> Result<Self> {
        let manifest: Self = toml::from_str(toml_str).context("parsing agents manifest TOML")?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Load from `path`. A **missing** file returns [`Self::defaults`]
    /// (shell, newt, claude, codex) rather than an error: the palette must
    /// never be empty just because the operator has not written a manifest.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::defaults()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The built-in palette when no manifest file exists: the user's real
    /// shell (via `$SHELL`, falling back to `/bin/sh`), plus `newt`, `claude`,
    /// and `codex` invoked bare (resolved on `PATH` at spawn time).
    /// Reads the live `$SHELL`; see [`defaults_with_shell`](Self::defaults_with_shell)
    /// for the pure form the tests use.
    #[must_use]
    pub fn defaults() -> Self {
        Self::defaults_with_shell(std::env::var("SHELL").ok())
    }

    /// [`defaults`](Self::defaults) over an injected `$SHELL` value.
    ///
    /// Pure, so the palette's shell entry is testable without `set_var`:
    /// mutating the process environment races every other test in the binary
    /// (cargo runs them as threads in one process). Resolution goes through
    /// [`crate::pty::resolve_shell`], the same lookup the cockpit's shell panes
    /// spawn with, so the palette entry and the real pane can never disagree.
    #[must_use]
    pub fn defaults_with_shell(shell_env: Option<String>) -> Self {
        let shell = crate::pty::resolve_shell(shell_env);
        Self {
            agents: vec![
                AgentEntry {
                    name: "shell".to_string(),
                    argv: vec![shell],
                    description: "The user's real shell".to_string(),
                    cwd: None,
                    env: Vec::new(),
                    posture: Posture::Confined,
                },
                AgentEntry {
                    name: "newt".to_string(),
                    argv: vec!["newt".to_string()],
                    description: "newt-agent, in a PTY pane".to_string(),
                    cwd: None,
                    env: Vec::new(),
                    posture: Posture::Confined,
                },
                AgentEntry {
                    name: "claude".to_string(),
                    argv: vec!["claude".to_string()],
                    description: "Claude Code, in a PTY pane".to_string(),
                    cwd: None,
                    env: Vec::new(),
                    posture: Posture::Confined,
                },
                AgentEntry {
                    name: "codex".to_string(),
                    argv: vec!["codex".to_string()],
                    description: "codex, in a PTY pane".to_string(),
                    cwd: None,
                    env: Vec::new(),
                    posture: Posture::Confined,
                },
            ],
        }
    }

    /// The default manifest path, `<home>/.gila/agents.toml`.
    #[must_use]
    pub fn default_path(home: &Path) -> PathBuf {
        home.join(".gila").join("agents.toml")
    }

    /// Fail loud on an empty name/argv in any entry, or on a duplicate
    /// `name` across entries, a duplicate would make the palette ambiguous
    /// about which argv actually spawns.
    pub fn validate(&self) -> Result<()> {
        for entry in &self.agents {
            entry.validate()?;
        }
        let mut seen = std::collections::HashSet::new();
        for entry in &self.agents {
            if !seen.insert(entry.name.as_str()) {
                bail!("agents.toml: duplicate agent name '{}'", entry.name);
            }
        }
        Ok(())
    }

    /// Look up one entry by name.
    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&AgentEntry> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// A commented example manifest, written by `gila cockpit --init-agents`
    /// (future) and shown by `--help`.
    #[must_use]
    pub fn template() -> String {
        r#"# ~/.gila/agents.toml — the herder cockpit's palette manifest.
# Each [[agents]] entry becomes a "spawn: <name>" option in the command
# palette (prefix key, palette-open binding). Missing this file entirely is
# fine — the cockpit falls back to a built-in shell + newt + claude + codex
# palette.

[[agents]]
name = "newt"
argv = ["newt"]
description = "newt-agent, in a PTY pane"
# cwd = "/path/to/project"          # defaults to the cockpit's own cwd
# env = ["OPENAI_API_KEY"]          # extra vars granted to the subprocess
# posture = "confined"              # "confined" (default) or "ambient"

[[agents]]
name = "claude"
argv = ["claude"]
description = "Claude Code, in a PTY pane"

[[agents]]
name = "codex"
argv = ["codex"]
description = "codex, in a PTY pane"
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SAMPLE: &str = r#"
        [[agents]]
        name = "newt"
        argv = ["newt"]
        description = "newt-agent, in a PTY pane"
        cwd = "/work"
        env = ["OPENAI_API_KEY=sk-abc"]
        posture = "confined"

        [[agents]]
        name = "trusted-shell"
        argv = ["/bin/bash", "-l"]
        description = "an ambient login shell"
        posture = "ambient"
    "#;

    #[test]
    fn empty_document_has_no_agents() {
        let m = AgentManifest::parse("").unwrap();
        assert!(m.agents.is_empty());
    }

    #[test]
    fn missing_file_loads_defaults() {
        let m = AgentManifest::load(Path::new("/no/such/agents.toml")).unwrap();
        assert_eq!(m, AgentManifest::defaults());
        assert!(m.entry("shell").is_some());
        assert!(m.entry("newt").is_some());
        assert!(m.entry("claude").is_some());
        assert!(m.entry("codex").is_some());
    }

    #[test]
    fn defaults_use_shell_env_var() {
        // Injected, not `set_var`: mutating the process environment races the
        // other tests in this binary (cargo runs them as threads in one
        // process), and `pty::pty_shell_program_returns_a_nonempty_program`
        // reads SHELL concurrently.
        let m = AgentManifest::defaults_with_shell(Some("/opt/custom/fish".to_string()));
        assert_eq!(m.entry("shell").unwrap().argv, vec!["/opt/custom/fish"]);
    }

    #[test]
    fn defaults_fall_back_when_shell_is_unset_or_blank() {
        for env in [None, Some(String::new()), Some("   ".to_string())] {
            let m = AgentManifest::defaults_with_shell(env);
            assert_eq!(
                m.entry("shell").unwrap().argv,
                vec![crate::pty::DEFAULT_SHELL],
                "a blank $SHELL falls back to the same default the pane spawns"
            );
        }
    }

    #[test]
    fn parses_entries_with_defaults() {
        let m = AgentManifest::parse(SAMPLE).unwrap();
        let newt = m.entry("newt").unwrap();
        assert_eq!(newt.argv, ["newt"]);
        assert_eq!(newt.cwd.as_deref(), Some("/work"));
        assert_eq!(newt.env, ["OPENAI_API_KEY=sk-abc"]);
        assert_eq!(newt.posture, Posture::Confined);

        let shell = m.entry("trusted-shell").unwrap();
        assert_eq!(shell.posture, Posture::Ambient);
    }

    #[test]
    fn load_reads_from_a_real_file() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(SAMPLE.as_bytes()).unwrap();
        let m = AgentManifest::load(f.path()).unwrap();
        assert_eq!(m.agents.len(), 2);
    }

    #[test]
    fn empty_name_fails_loud() {
        let toml = r#"
            [[agents]]
            name = ""
            argv = ["sh"]
            description = "x"
        "#;
        let err = AgentManifest::parse(toml).unwrap_err();
        assert!(err.to_string().contains("empty name"));
    }

    #[test]
    fn empty_argv_fails_loud() {
        let toml = r#"
            [[agents]]
            name = "broken"
            argv = []
            description = "x"
        "#;
        let err = AgentManifest::parse(toml).unwrap_err();
        assert!(err.to_string().contains("empty argv"));
    }

    #[test]
    fn malformed_env_fails_loud() {
        let toml = r#"
            [[agents]]
            name = "broken"
            argv = ["sh"]
            description = "x"
            env = ["NOT_KV"]
        "#;
        let err = AgentManifest::parse(toml).unwrap_err();
        assert!(err.to_string().contains("malformed env entry"));
    }

    #[test]
    fn duplicate_name_fails_loud() {
        let toml = r#"
            [[agents]]
            name = "dup"
            argv = ["sh"]
            description = "one"

            [[agents]]
            name = "dup"
            argv = ["bash"]
            description = "two"
        "#;
        let err = AgentManifest::parse(toml).unwrap_err();
        assert!(err.to_string().contains("duplicate agent name"));
    }

    #[test]
    fn default_path_is_under_dot_gila() {
        let p = AgentManifest::default_path(Path::new("/home/op"));
        assert!(p.ends_with(".gila/agents.toml"));
    }

    #[test]
    fn template_parses_and_validates() {
        let toml = AgentManifest::template();
        let m = AgentManifest::parse(&toml).unwrap();
        assert_eq!(m.agents.len(), 3);
    }
}
