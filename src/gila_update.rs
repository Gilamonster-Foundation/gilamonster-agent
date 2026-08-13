//! Rust-native `gila update` — Phase 3 of the gila-parity plan.
//!
//! Self-update: pull + rebuild gilamonster-agent in place. The build steps
//! are the effectful seam (they shell out to `git` / `cargo`, matching how an
//! operator updates by hand); the command list is unit-testable data.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// The update steps, in order, as `(program, args)` pairs. Pure data so the
/// plan is unit-testable; `run_update` executes them.
pub fn update_steps(repo: &Path) -> Vec<(String, Vec<String>)> {
    let r = repo.display().to_string();
    vec![
        (
            "git".into(),
            vec!["-C".into(), r.clone(), "pull".into(), "--ff-only".into()],
        ),
        (
            "cargo".into(),
            vec![
                "build".into(),
                "--release".into(),
                "--manifest-path".into(),
                repo.join("Cargo.toml").display().to_string(),
            ],
        ),
    ]
}

/// Execute the update plan, streaming each step's output. Stops on the first
/// failed step and reports which one. The subprocess spawns are the
/// by-design effectful seam.
pub fn run_update(repo: &Path) -> Result<()> {
    for (prog, args) in update_steps(repo) {
        let status = Command::new(&prog)
            .args(&args)
            .status()
            .with_context(|| format!("spawning {prog}"))?;
        if !status.success() {
            anyhow::bail!("update step `{prog} {}` failed", args.join(" "));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_plan_pulls_then_builds() {
        let steps = update_steps(Path::new("/repo"));
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].0, "git");
        assert!(steps[0].1.contains(&"pull".to_string()));
        assert!(steps[0].1.contains(&"--ff-only".to_string()));
        assert_eq!(steps[1].0, "cargo");
        assert!(steps[1].1.contains(&"--release".to_string()));
        // The pull targets the repo; the build targets its Cargo.toml.
        assert!(steps[0].1.contains(&"/repo".to_string()));
        assert!(steps[1].1.iter().any(|a| a.ends_with("Cargo.toml")));
    }
}
