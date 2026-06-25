//! The capability **selection manifest** — `~/.gila/capabilities.toml`.
//!
//! The MCP/agent surface of every installed capability would flood an agent's
//! context, so it is **opt-in**: by default a capability is human-CLI only. This
//! manifest is where the user (via `gila cap config`, future) records, per
//! capability, whether it loads as agent tools, whether it runs confined, which
//! env it needs, and which venv serves it.
//!
//! This module is pure parsing + lookup — it deliberately does not depend on
//! newt's config types; composing the agent-exposed entries into newt
//! `[[mcp_servers]]` lives in [`crate::capabilities`] (which owns the newt seam).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Where a capability's tools are exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expose {
    /// Human CLI only — the safe default (no agent tools, no context flooding).
    #[default]
    Cli,
    /// Mounted as agent tools (over MCP) only.
    Agent,
    /// Both the human CLI and agent tools.
    Both,
    /// Not exposed at all.
    Off,
}

impl Expose {
    /// Does this capability mount as agent tools (`Agent` or `Both`)?
    #[must_use]
    pub fn is_agent(self) -> bool {
        matches!(self, Expose::Agent | Expose::Both)
    }
}

/// One `[[capabilities]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CapabilityEntry {
    /// The capability name (matches the `gilamonster.capabilities` entry point).
    pub name: String,
    /// Where its tools are exposed. Defaults to [`Expose::Cli`].
    #[serde(default)]
    pub expose: Expose,
    /// Run the capability's tools under an agent-bridle leash when invoked as an
    /// agent tool. (Enforced once the confined-spawn path lands — agent-bridle#55.)
    #[serde(default)]
    pub confined: bool,
    /// Environment variables to grant the capability subprocess (and nothing
    /// else reaches it — the external-boundary invariant).
    #[serde(default)]
    pub env: Vec<String>,
    /// Optional per-tool allow-list when exposed as agent tools.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// A venv that serves this capability's `gilacap`, overriding the global.
    #[serde(default)]
    pub venv: Option<String>,
}

/// The parsed `capabilities.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    /// A venv applied to every capability that does not name its own.
    #[serde(default)]
    pub venv: Option<String>,
    /// The `[[capabilities]]` array.
    #[serde(default)]
    pub capabilities: Vec<CapabilityEntry>,
}

impl Manifest {
    /// Parse manifest TOML. An empty document is the empty (CLI-only) manifest.
    pub fn parse(toml_str: &str) -> Result<Self> {
        toml::from_str(toml_str).context("parsing capabilities manifest TOML")
    }

    /// Load from `path`; a **missing** file is the empty manifest (the safe
    /// CLI-only default), not an error.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The default manifest path, `<home>/.gila/capabilities.toml`.
    #[must_use]
    pub fn default_path(home: &Path) -> PathBuf {
        home.join(".gila").join("capabilities.toml")
    }

    /// Look up one capability's entry.
    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&CapabilityEntry> {
        self.capabilities.iter().find(|c| c.name == name)
    }

    /// The capabilities exposed as agent tools (`Agent` or `Both`).
    pub fn agent_exposed(&self) -> impl Iterator<Item = &CapabilityEntry> {
        self.capabilities.iter().filter(|c| c.expose.is_agent())
    }

    /// The venv override for `name`: its own `venv`, else the manifest global.
    #[must_use]
    pub fn venv_for(&self, name: &str) -> Option<&str> {
        self.entry(name)
            .and_then(|c| c.venv.as_deref())
            .or(self.venv.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        venv = "/home/op/.gila/caps-venv"

        [[capabilities]]
        name = "confluence"
        expose = "both"
        confined = true
        env = ["CONFLUENCE_BASE_URL", "CONFLUENCE_TOKEN"]
        tools = ["fetch", "blog"]

        [[capabilities]]
        name = "mogul"
        # expose defaults to cli; venv overrides the global
        venv = "/opt/mogulvenv"
    "#;

    #[test]
    fn empty_document_is_the_cli_only_default() {
        let m = Manifest::parse("").unwrap();
        assert!(m.capabilities.is_empty());
        assert!(m.venv.is_none());
        assert_eq!(m.agent_exposed().count(), 0);
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let m = Manifest::load(Path::new("/no/such/capabilities.toml")).unwrap();
        assert_eq!(m, Manifest::default());
    }

    #[test]
    fn parses_entries_with_defaults() {
        let m = Manifest::parse(SAMPLE).unwrap();
        let c = m.entry("confluence").unwrap();
        assert_eq!(c.expose, Expose::Both);
        assert!(c.confined);
        assert_eq!(c.env, ["CONFLUENCE_BASE_URL", "CONFLUENCE_TOKEN"]);
        assert_eq!(
            c.tools.as_deref(),
            Some(&["fetch".to_string(), "blog".to_string()][..])
        );

        let mogul = m.entry("mogul").unwrap();
        assert_eq!(mogul.expose, Expose::Cli); // default
        assert!(!mogul.confined);
    }

    #[test]
    fn agent_exposed_filters_to_agent_and_both() {
        let m = Manifest::parse(SAMPLE).unwrap();
        let names: Vec<_> = m.agent_exposed().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["confluence"]); // mogul is cli-only
    }

    #[test]
    fn venv_for_prefers_entry_then_global() {
        let m = Manifest::parse(SAMPLE).unwrap();
        assert_eq!(m.venv_for("mogul"), Some("/opt/mogulvenv")); // entry override
        assert_eq!(m.venv_for("confluence"), Some("/home/op/.gila/caps-venv")); // global
        assert_eq!(m.venv_for("absent"), Some("/home/op/.gila/caps-venv")); // global fallback
    }

    #[test]
    fn default_path_is_under_dot_gila() {
        let p = Manifest::default_path(Path::new("/home/op"));
        assert!(p.ends_with(".gila/capabilities.toml"));
    }
}
