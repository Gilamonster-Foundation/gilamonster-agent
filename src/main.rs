//! `gila` — the Gilamonster agent matrix binary.
//!
//! Thin shim over the [`gilamonster_agent`] library: parse argv, then either
//! hand off to newt-agent's inherited TUI (`gila code`) or print the matrix
//! scaffold report (`gila matrix`). All testable logic — the CLI shape, the
//! matrix rendering, the identity-path resolution — lives in `src/lib.rs` and
//! is covered by the unit tests there. The two lines this binary owns that the
//! library can't (launching the TUI, reading the real identity path) are the
//! only uncovered surface, by design.
//!
//! See the crate-level docs in `src/lib.rs` for the inherit/extend rationale.

use clap::Parser;
use gilamonster_agent::{code_path, matrix_report, Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().effective_command() {
        // Inherit: hand off to newt-agent's TUI directly. gilamonster's own
        // surfaces will wrap/extend this rather than reimplement it. `persona =
        // None` → newt's default persona; gila's personas land with the matrix
        // layer (#8-#11).
        Command::Code { path } => newt_tui::run_code(code_path(&path), false, None),
        // The matrix runs under the same inherited object-capability identity
        // as newt — surface where the operator key lives, then the scaffold
        // notice. The rendering is in `matrix_report` (unit-tested); here we
        // only resolve the real path and print.
        Command::Matrix => {
            print!("{}", matrix_report(newt_identity::default_key_path()));
            Ok(())
        }
    }
}
