//! Rust-native `gila prompt` — Phase 3 of the gila-parity plan.
//!
//! Manage reusable prompt templates stored as `*.md` files under the prompt
//! directory (default `~/.gila/prompts/`): `list` shows them, `show <name>`
//! prints one, `create <name>` scaffolds a new template. Pure listing +
//! rendering is unit-testable; the binary's `run_*` arm owns the file I/O.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default prompt directory, relative to `$HOME`.
pub const DEFAULT_PROMPTS_REL: &str = ".gila/prompts";

/// Resolve the prompt directory.
pub fn prompts_dir(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_PROMPTS_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the prompt directory"),
    }
}

/// List prompt template names (file stems, sorted) under `dir`.
pub fn list_prompts(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
                .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// The scaffold body written for a new template.
pub fn prompt_template(name: &str) -> String {
    format!("# Prompt: {name}\n\n<template body>\n")
}

/// Create a new template. Returns the path; errors if it already exists.
pub fn create_prompt(dir: &Path, name: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating prompt dir {}", dir.display()))?;
    let path = dir.join(format!("{name}.md"));
    if path.exists() {
        anyhow::bail!("prompt `{name}` already exists at {}", path.display());
    }
    std::fs::write(&path, prompt_template(name))
        .with_context(|| format!("writing prompt {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_dir_requires_home() {
        assert!(prompts_dir(None).is_err());
        assert_eq!(
            prompts_dir(Some(Path::new("/home/op"))).unwrap(),
            PathBuf::from("/home/op/.gila/prompts")
        );
    }

    #[test]
    fn list_returns_sorted_stems() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("zebra.md"), "").unwrap();
        std::fs::write(tmp.path().join("apple.md"), "").unwrap();
        std::fs::write(tmp.path().join("ignore.txt"), "").unwrap();
        assert_eq!(list_prompts(tmp.path()), vec!["apple", "zebra"]);
    }

    #[test]
    fn create_scaffolds_then_refuses_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("prompts");
        let p = create_prompt(&dir, "standup").unwrap();
        assert!(p.exists());
        assert!(std::fs::read_to_string(&p).unwrap().contains("# Prompt: standup"));
        assert!(create_prompt(&dir, "standup").is_err());
    }
}
