//! gilamonster-agent — the Gilamonster agent matrix.
//!
//! **Inherits** newt-agent's "airframe" — the lean chat + agentic-coding TUI,
//! the object-capability identity (signed, attenuation-only `AgentKey`
//! caveats), the ACP worker and coder — from the published `newt-*` crates,
//! and **extends** it into a Hermes/Thoon-style multi-agent matrix.
//!
//! newt is the cell; gilamonster-agent is the organism. The extension point is
//! this *separate binary*, not a plugin slot — which is exactly why newt stays
//! "opinionated, not extensible."
//!
//! This is the v0.1 scaffold: it proves the inheritance compiles and runs by
//! delegating to newt's TUI (`gila code`) and surfacing the inherited ocap
//! identity (`gila matrix`). The matrix layer — many newt airframes over the
//! agent-mesh airspace, drake lifecycle, orchestration, and the rich
//! settings/dashboard surfaces ported from newt's git history — lands on top.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "gila",
    version,
    about = "The Gilamonster agent matrix — inherits newt-agent, extends into a multi-agent matrix"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the inherited newt chat + agentic-coding TUI (the airframe).
    Code {
        /// Optional working path.
        path: Option<std::path::PathBuf>,
    },
    /// The multi-agent matrix — the extension layer (scaffold: not yet built).
    Matrix,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command.unwrap_or(Command::Code { path: None }) {
        // Inherit: hand off to newt-agent's TUI directly. gilamonster's own
        // surfaces will wrap/extend this rather than reimplement it.
        Command::Code { path } => newt_tui::run_code(path.as_deref(), false),
        Command::Matrix => {
            // The matrix runs under the same inherited object-capability
            // identity as newt — surface where the operator key lives.
            match newt_identity::default_key_path() {
                Ok(p) => println!(
                    "operator identity (inherited from newt-identity): {}",
                    p.display()
                ),
                Err(_) => println!("operator identity: ~/.newt/identity.pem (HOME unset)"),
            }
            println!();
            println!("gilamonster matrix — the multi-agent extension layer — is not yet built.");
            println!("It will compose newt airframes over the agent-mesh airspace, under one");
            println!("attenuation-only capability model, with drake lifecycle + orchestration.");
            Ok(())
        }
    }
}
