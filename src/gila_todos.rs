//! Rust-native `gila todos` — Phase 3 of the gila-parity plan.
//!
//! Manage a plain-markdown todo list (default `~/.gila/todos.md`): add an item,
//! list open items, or mark one done. Items are `- [ ]` / `- [x]` checkbox
//! lines so the file stays editable by hand. Pure parsing/toggling is
//! unit-testable; the binary's `run_*` arm owns the file I/O.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default todo file, relative to `$HOME`.
pub const DEFAULT_TODOS_REL: &str = ".gila/todos.md";

/// Resolve the todo file path.
pub fn todos_path(home: Option<&Path>) -> Result<PathBuf> {
    match home {
        Some(h) => Ok(h.join(DEFAULT_TODOS_REL)),
        None => anyhow::bail!("HOME unset; cannot resolve the todos file"),
    }
}

/// The checkbox line for a new (open) todo.
pub fn todo_line(text: &str) -> String {
    format!("- [ ] {text}\n")
}

/// Append a new open todo, creating the file (with a header) if absent.
pub fn add_todo(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating todos dir {}", parent.display()))?;
    }
    if !path.exists() {
        std::fs::write(path, "# Todos\n\n")
            .with_context(|| format!("writing todos file {}", path.display()))?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("opening todos file {}", path.display()))?;
    f.write_all(todo_line(text).as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

/// List the open (`- [ ]`) todos, 1-indexed, as display lines.
pub fn list_open(body: &str) -> Vec<String> {
    body.lines()
        .filter(|l| l.trim_start().starts_with("- [ ]"))
        .enumerate()
        .map(|(i, l)| format!("{}: {}", i + 1, l.trim_start_matches("- [ ] ").trim()))
        .collect()
}

/// Mark the nth open todo done (`- [ ]` → `- [x]`). Returns the new body, or
/// `None` if `n` is out of range. `n` is 1-indexed over *open* items.
pub fn mark_done(body: &str, n: usize) -> Option<String> {
    if n == 0 {
        return None;
    }
    let mut seen = 0usize;
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("- [ ]") {
            seen += 1;
            if seen == n {
                out.push_str(&line.replacen("- [ ]", "- [x]", 1));
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    (seen >= n).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "# Todos\n\n- [ ] a\n- [x] done\n- [ ] b\n- [ ] c\n";

    #[test]
    fn line_is_open_checkbox() {
        assert_eq!(todo_line("x"), "- [ ] x\n");
    }

    #[test]
    fn todos_path_requires_home() {
        assert!(todos_path(None).is_err());
    }

    #[test]
    fn list_open_indexes_only_open() {
        assert_eq!(list_open(BODY), vec!["1: a", "2: b", "3: c"]);
    }

    #[test]
    fn mark_done_toggles_nth_open() {
        let out = mark_done(BODY, 2).unwrap();
        assert!(out.contains("- [ ] a"));
        assert!(out.contains("- [x] b"));
        assert!(out.contains("- [ ] c"));
        // Already-done items untouched; out-of-range rejected.
        assert!(out.contains("- [x] done"));
        assert!(mark_done(BODY, 0).is_none());
        assert!(mark_done(BODY, 9).is_none());
    }

    #[test]
    fn add_creates_then_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("todos.md");
        add_todo(&p, "first").unwrap();
        add_todo(&p, "second").unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.starts_with("# Todos\n"));
        assert!(body.contains("- [ ] first\n"));
        assert!(body.contains("- [ ] second\n"));
    }
}
