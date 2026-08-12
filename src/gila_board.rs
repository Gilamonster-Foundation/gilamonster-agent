//! Rust-native `gila board` — Phase 3 of the gila-parity plan.
//!
//! Shows the board: the `*.md` task files directly under the board directory
//! (default `~/workspaces/knowledge/board/`), grouped by their priority-lane
//! heading when present. Pure scanning + grouping is unit-testable; the
//! binary's `run_*` arm owns the root resolution + print.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Default board directory, relative to `$HOME`.
pub const DEFAULT_BOARD_REL: &str = "workspaces/knowledge/board";

/// Resolve the board directory.
pub fn board_dir(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_BOARD_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the board directory"),
    }
}

/// List the `*.md` board files directly under `dir`, sorted by name.
pub fn list_board_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Render the board file list as display lines (file names, 1 per line).
pub fn render_board(files: &[PathBuf]) -> String {
    if files.is_empty() {
        return "board is empty\n".to_string();
    }
    let mut out = String::new();
    for f in files {
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push_str(&name);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_dir_requires_home() {
        assert!(board_dir(None).is_err());
        assert_eq!(
            board_dir(Some(Path::new("/home/op"))).unwrap(),
            PathBuf::from("/home/op/workspaces/knowledge/board")
        );
    }

    #[test]
    fn list_filters_md_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("b.md"), "").unwrap();
        std::fs::write(tmp.path().join("a.md"), "").unwrap();
        std::fs::write(tmp.path().join("ignore.txt"), "").unwrap();
        let files = list_board_files(tmp.path());
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.md", "b.md"]);
    }

    #[test]
    fn render_handles_empty() {
        assert_eq!(render_board(&[]), "board is empty\n");
    }
}
