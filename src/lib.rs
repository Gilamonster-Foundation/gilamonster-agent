//! gilamonster-agent — the Gilamonster agent matrix (library surface).
//!
//! **Inherits** newt-agent's "airframe" — the lean chat + agentic-coding TUI,
//! the object-capability identity (signed, attenuation-only `AgentKey`
//! caveats), the ACP worker and coder — over a git dependency on the
//! `newt-agent` repo, and **extends** it into a Hermes/Thoon-style multi-agent
//! matrix.
//!
//! newt is the cell; gilamonster-agent is the organism. The extension point is
//! a *separate binary* (`gila`), not a plugin slot — which is exactly why newt
//! stays "opinionated, not extensible."
//!
//! This module holds the testable, side-effect-free surface (CLI shape, the
//! `matrix` scaffold rendering, the identity-path resolution). The `gila`
//! binary (`src/main.rs`) is a thin shim that parses argv and either hands off
//! to newt's TUI or prints what these functions return. Keeping the logic here
//! is what lets the gate hold a real coverage floor on a near-empty crate.
//!
//! The matrix layer — many newt airframes over the agent-mesh airspace, drake
//! lifecycle, orchestration, and the rich settings/dashboard surfaces — lands
//! on top in the cowork / hotseat issues (#8-#11).

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

/// The `gila` command-line surface. Parsed in `main`, re-exported here so the
/// argv contract is unit-testable without launching the inherited TUI.
#[derive(Parser, Debug)]
#[command(
    name = "gila",
    version,
    about = "The Gilamonster agent matrix — inherits newt-agent, extends into a multi-agent matrix"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// `gila` subcommands. `Code` inherits newt's TUI; `Matrix` is the (not-yet
/// built) extension layer.
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum Command {
    /// Run the inherited newt chat + agentic-coding TUI (the airframe).
    Code {
        /// Optional working path.
        path: Option<PathBuf>,
    },
    /// The multi-agent matrix — the extension layer (scaffold: not yet built).
    Matrix,
}

impl Cli {
    /// The effective command: defaulting a bare `gila` invocation to `code` in
    /// the current directory, mirroring newt's "no subcommand → TUI" behaviour.
    pub fn effective_command(self) -> Command {
        self.command.unwrap_or(Command::Code { path: None })
    }
}

/// Render the line announcing where the inherited operator identity lives.
///
/// Takes the *result* of `newt_identity::default_key_path()` so both arms — a
/// resolved path and the `HOME`-unset fallback — are exercised without touching
/// the environment. Pure: returns the string instead of printing it.
pub fn identity_line<E>(key_path: Result<PathBuf, E>) -> String {
    match key_path {
        Ok(p) => format!(
            "operator identity (inherited from newt-identity): {}",
            p.display()
        ),
        Err(_) => "operator identity: ~/.newt/identity.pem (HOME unset)".to_string(),
    }
}

/// The full text `gila matrix` prints: the identity line followed by the
/// "not yet built" scaffold notice. Pure (returns the string) so the binary's
/// `Matrix` arm is a single `print!` of this and the content is unit-tested.
pub fn matrix_report<E>(key_path: Result<PathBuf, E>) -> String {
    let mut out = identity_line(key_path);
    out.push('\n');
    out.push('\n');
    out.push_str(
        "gilamonster matrix — the multi-agent extension layer — is not yet built.\n\
         It will compose newt airframes over the agent-mesh airspace, under one\n\
         attenuation-only capability model, with drake lifecycle + orchestration.\n",
    );
    out
}

/// Resolve the workspace path for `gila code`, mirroring how the binary threads
/// the optional positional path into `newt_tui::run_code`. Returns the borrow
/// the TUI entry expects (`Option<&Path>`).
pub fn code_path(path: &Option<PathBuf>) -> Option<&Path> {
    path.as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug)]
    struct DummyErr;

    #[test]
    fn bare_invocation_defaults_to_code_in_cwd() {
        let cli = Cli::parse_from(["gila"]);
        assert_eq!(cli.effective_command(), Command::Code { path: None });
    }

    #[test]
    fn code_subcommand_with_path_parses() {
        let cli = Cli::parse_from(["gila", "code", "/tmp/project"]);
        assert_eq!(
            cli.effective_command(),
            Command::Code {
                path: Some(PathBuf::from("/tmp/project")),
            }
        );
    }

    #[test]
    fn code_subcommand_without_path_parses() {
        let cli = Cli::parse_from(["gila", "code"]);
        assert_eq!(cli.effective_command(), Command::Code { path: None });
    }

    #[test]
    fn matrix_subcommand_parses() {
        let cli = Cli::parse_from(["gila", "matrix"]);
        assert_eq!(cli.effective_command(), Command::Matrix);
    }

    #[test]
    fn identity_line_ok_shows_path() {
        let line = identity_line::<DummyErr>(Ok(PathBuf::from("/home/op/.newt/identity.pem")));
        assert!(line.contains("inherited from newt-identity"));
        assert!(line.contains("/home/op/.newt/identity.pem"));
    }

    #[test]
    fn identity_line_err_shows_home_unset_fallback() {
        let line = identity_line(Err(DummyErr));
        assert!(line.contains("HOME unset"));
        assert!(line.contains("~/.newt/identity.pem"));
    }

    #[test]
    fn matrix_report_includes_identity_and_scaffold_notice() {
        let report = matrix_report::<DummyErr>(Ok(PathBuf::from("/home/op/.newt/identity.pem")));
        assert!(report.contains("/home/op/.newt/identity.pem"));
        assert!(report.contains("is not yet built"));
        assert!(report.contains("agent-mesh airspace"));
        // Identity line first, scaffold notice after a blank line.
        let idx_id = report.find("operator identity").unwrap();
        let idx_notice = report.find("not yet built").unwrap();
        assert!(idx_id < idx_notice);
    }

    #[test]
    fn matrix_report_err_branch_uses_fallback() {
        let report = matrix_report(Err(DummyErr));
        assert!(report.contains("HOME unset"));
        assert!(report.contains("is not yet built"));
    }

    #[test]
    fn code_path_borrows_inner() {
        let owned = Some(PathBuf::from("/tmp/x"));
        assert_eq!(code_path(&owned), Some(Path::new("/tmp/x")));
        assert_eq!(code_path(&None), None);
    }
}
