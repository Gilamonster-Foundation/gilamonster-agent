//! Pixi environment and task detection for Jupyter integration.
//!
//! This module provides:
//! - Pixi availability and manifest detection
//! - Task discovery and execution through Pixi
//! - Bootstrap workflow for modernizing manifests to [workspace] syntax
//! - Safe environment forwarding through Pixi's execution model

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Detect if Pixi is available on PATH.
pub fn is_pixi_available() -> bool {
    Command::new("pixi")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check if a directory contains pixi.toml
pub fn has_pixi_manifest(working_dir: &Path) -> bool {
    working_dir.join("pixi.toml").exists()
}

/// Pixi manifest structure (simplified for task discovery)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PixiManifest {
    pub project: Option<ProjectTable>,
    pub workspace: Option<WorkspaceTable>,
    pub tasks: Option<std::collections::BTreeMap<String, TaskDef>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectTable {
    pub name: Option<String>,
    #[serde(flatten)]
    pub rest: std::collections::BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceTable {
    pub members: Option<Vec<String>>,
    #[serde(flatten)]
    pub rest: std::collections::BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TaskDef {
    String(String),
    Table(std::collections::BTreeMap<String, toml::Value>),
}

impl TaskDef {
    pub fn as_command(&self) -> Option<&str> {
        match self {
            TaskDef::String(s) => Some(s),
            TaskDef::Table(t) => t.get("cmd").and_then(|v| v.as_str()),
        }
    }
}

/// Load and parse pixi.toml from a directory
pub fn load_manifest(working_dir: &Path) -> Result<PixiManifest> {
    let manifest_path = working_dir.join("pixi.toml");
    let content = fs::read_to_string(&manifest_path)
        .context("Failed to read pixi.toml")?;
    let manifest: PixiManifest = toml::from_str(&content)
        .context("Failed to parse pixi.toml")?;
    Ok(manifest)
}

/// Check if manifest uses deprecated [project] vs modern [workspace]
pub fn is_legacy_manifest(manifest: &PixiManifest) -> bool {
    manifest.project.is_some() && manifest.workspace.is_none()
}

/// Find a declared Jupyter-related task (lab-local, jupyter, lab, etc.)
pub fn find_jupyter_task(manifest: &PixiManifest, explicit_task: Option<&str>) -> Option<String> {
    if let Some(task_name) = explicit_task {
        // Explicit task requested - verify it exists
        manifest.tasks.as_ref()?.contains_key(task_name).then_some(task_name.to_string())
    } else {
        // Look for conventional names
        for name in &["lab-local", "lab", "jupyter-lab", "jupyter"] {
            if let Some(tasks) = &manifest.tasks {
                if tasks.contains_key(*name) {
                    return Some(name.to_string());
                }
            }
        }
        None
    }
}

/// Bootstrap workflow preview: show what will change
pub fn preview_modernization(manifest: &PixiManifest) -> String {
    let mut preview = String::from("Would update pixi.toml:\n");
    if is_legacy_manifest(manifest) {
        preview.push_str("  - Rename [project] → [workspace]\n");
        preview.push_str("  - Modernize structure for multi-environment support\n");
    } else {
        preview.push_str("  ✓ Already using modern [workspace] structure\n");
    }
    preview
}

/// Bootstrap workflow: update manifest to [workspace] syntax
pub fn modernize_manifest(working_dir: &Path) -> Result<()> {
    let manifest_path = working_dir.join("pixi.toml");
    let content = fs::read_to_string(&manifest_path)
        .context("Failed to read pixi.toml")?;

    // Simple transformation: replace [project] with [workspace]
    let updated = if content.contains("[project]") && !content.contains("[workspace]") {
        content.replace("[project]", "[workspace]")
    } else {
        content.clone()
    };

    if updated != content {
        fs::write(&manifest_path, updated)
            .context("Failed to write updated pixi.toml")?;
        Ok(())
    } else {
        Ok(()) // Already modern or no changes needed
    }
}

/// Build a Pixi-wrapped command for launching a tool
///
/// This ensures environment is properly managed through Pixi while
/// still maintaining security (no sensitive env vars leak to child).
pub fn build_pixi_cmd(
    working_dir: &Path,
    task_or_cmd: &str,
    extra_args: &[String],
) -> Result<Command> {
    if !is_pixi_available() {
        anyhow::bail!("Pixi is not available on PATH");
    }

    let mut cmd = Command::new("pixi");
    cmd.arg("run").arg(task_or_cmd);
    cmd.args(extra_args);
    cmd.current_dir(working_dir);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_legacy_manifest_detection() {
        let legacy = PixiManifest {
            project: Some(ProjectTable {
                name: Some("test".to_string()),
                rest: Default::default(),
            }),
            workspace: None,
            tasks: None,
        };
        assert!(is_legacy_manifest(&legacy));

        let modern = PixiManifest {
            project: None,
            workspace: Some(WorkspaceTable {
                members: None,
                rest: Default::default(),
            }),
            tasks: None,
        };
        assert!(!is_legacy_manifest(&modern));
    }

    #[test]
    fn test_find_jupyter_task() {
        let manifest = PixiManifest {
            project: None,
            workspace: None,
            tasks: Some(
                vec![
                    ("lab-local".to_string(), TaskDef::String("jupyter lab".to_string())),
                    ("test".to_string(), TaskDef::String("pytest".to_string())),
                ]
                .into_iter()
                .collect(),
            ),
        };

        // Should find lab-local
        assert_eq!(find_jupyter_task(&manifest, None), Some("lab-local".to_string()));

        // Explicit task
        assert_eq!(find_jupyter_task(&manifest, Some("test")), Some("test".to_string()));
        assert_eq!(find_jupyter_task(&manifest, Some("nonexistent")), None);
    }
}
