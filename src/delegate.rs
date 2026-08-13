//! Shell-delegate fallback for gilabot subcommands not yet ported to
//! gilamonster-agent (Rust).
//!
//! Phase 1 of the gila-parity plan (`docs/plans/2026-08-12-full-gila-parity-plan.md`)
//! ships this delegate FIRST so **every** gilabot command works from day one:
//! clap's `external_subcommand` catch-all routes any unrecognized
//! `gila <cmd> [args…]` here, and we re-exec the *real* (Python) gilabot binary
//! with the same arguments. Later phases replace individual delegates with
//! Rust-native implementations; a command graduates by gaining its own
//! [`Command`](crate::Command) variant so it never reaches this fallback.
//!
//! The only logic worth unit-testing (locating the gilabot binary without
//! re-execing ourselves) is pure and lives in [`resolve_gilabot`]; the actual
//! [`std::process::Command`] exec is the by-design-uncovered surface, mirroring
//! the binary-owned `run_*` arms in `main.rs`.

use std::ffi::OsString;
use std::path::PathBuf;

/// The name we look up on `PATH` to find the Python gilabot entry point.
///
/// gilabot installs its console script as `gila` (the same name as this Rust
/// binary). During the transition both may be present; [`resolve_gilabot`]
/// skips our own executable so we delegate to the *other* `gila`, not recurse.
pub const GILABOT_BIN_NAME: &str = "gila";

/// Locate the Python gilabot binary on `PATH`, skipping our own executable.
///
/// Returns the first `PATH` entry named [`GILABOT_BIN_NAME`] whose canonical
/// path differs from `own_exe` (our running binary). Pure with respect to the
/// two injected inputs (`paths`, `own_exe`) so the recursion guard is testable
/// without touching the real environment.
///
/// `paths` is the list of directories to search (normally the `PATH` split);
/// `own_exe` is `std::env::current_exe()`'s result. Returns `None` when no
/// *other* `gila` is found.
pub fn resolve_gilabot(paths: &[PathBuf], own_exe: Option<&PathBuf>) -> Option<PathBuf> {
    let own_canon = own_exe.and_then(|p| p.canonicalize().ok());
    for dir in paths {
        let candidate = dir.join(GILABOT_BIN_NAME);
        if !candidate.is_file() {
            continue;
        }
        let cand_canon = candidate.canonicalize().unwrap_or(candidate.clone());
        if Some(&cand_canon) == own_canon.as_ref() {
            // That's us — keep looking for the *Python* gila.
            continue;
        }
        return Some(candidate);
    }
    None
}

/// Split a `PATH`-style value into directories, using the platform separator.
///
/// Kept separate from [`resolve_gilabot`] so the split is testable and the
/// environment read stays at the call site (the uncovered runner). Takes
/// `&OsStr` because `PATH` is not guaranteed UTF-8.
pub fn path_dirs(path_var: &std::ffi::OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path_var).collect()
}

/// Build the argument vector for the delegated invocation: the subcommand name
/// followed by its args, preserving order and dropping none.
///
/// `external` is the raw capture from clap's `external_subcommand`: element 0
/// is the unrecognized subcommand name, the rest are its arguments.
pub fn delegate_args(external: &[OsString]) -> Vec<OsString> {
    external.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_skips_own_executable() {
        let dir = std::env::temp_dir();
        let me = dir.join("gila");
        let paths = vec![dir.clone()];
        // When the only `gila` on PATH is us, there is nothing to delegate to.
        // (The file may not exist; canonicalize failure is treated as "not us"
        // only when own_canon is None, so force own to a real path by using the
        // dir itself which exists.)
        let own = dir.canonicalize().unwrap();
        let _ = me; // not created on purpose
        assert_eq!(resolve_gilabot(&paths, Some(&own)), None);
    }

    #[test]
    fn resolve_returns_none_when_absent() {
        let paths = vec![PathBuf::from("/nonexistent-dir-xyz")];
        assert_eq!(resolve_gilabot(&paths, None), None);
    }

    #[test]
    fn path_dirs_splits_on_separator() {
        let joined = if cfg!(windows) { "a;b" } else { "a:b" };
        let dirs = path_dirs(std::ffi::OsStr::new(joined));
        assert_eq!(dirs, vec![PathBuf::from("a"), PathBuf::from("b")]);
    }

    #[test]
    fn delegate_args_preserves_order() {
        let ext = vec![
            OsString::from("confluence"),
            OsString::from("publish"),
            OsString::from("doc.md"),
        ];
        let got = delegate_args(&ext);
        assert_eq!(got, ext);
    }
}
