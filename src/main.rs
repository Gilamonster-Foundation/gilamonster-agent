//! `gila` — the Gilamonster agent matrix binary.
//!
//! Thin shim over the [`gilamonster_agent`] library: parse argv, then hand off
//! to newt-agent's inherited TUI (`gila code`), run the read-only follow loop
//! (`gila follow`), or print the matrix scaffold report (`gila matrix`). All
//! testable logic — the CLI shape, the matrix rendering, the identity-path
//! resolution, and the whole shell-observation channel — lives in `src/lib.rs`
//! / `src/follow.rs` and is covered by unit + CLI tests. The lines this binary
//! owns that the library can't (launching the TUI, reading the real identity
//! path, the live tail/print loop) are the only uncovered surface, by design.
//!
//! See the crate-level docs in `src/lib.rs` for the inherit/extend rationale.

use std::time::Duration;

use clap::Parser;
use gilamonster_agent::follow::{
    config_from_backend, drive_comment, follow_tick, FollowTick, ObservationChannel, TypescriptTail,
};
use gilamonster_agent::{
    code_path, follow_no_target_report, follow_target, matrix_report, Cli, Command,
};
use newt_core::agentic::TurnDriver;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().effective_command() {
        // Inherit: hand off to newt-agent's TUI directly. gilamonster's own
        // surfaces will wrap/extend this rather than reimplement it. `persona =
        // None` → newt's default persona; gila's personas land with the matrix
        // layer (#8-#11).
        Command::Code { path } => newt_tui::run_code(code_path(&path), false, None),
        // Read-only "follow me": tail the human's `script -F` typescript, feed
        // each burst of shell activity into the shared observation channel, and
        // let the agent comment. The agent NEVER drives the human's shell.
        Command::Follow { logpath, dir } => run_follow(logpath, dir).await,
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

/// The live `gila follow` loop (binary-owned, side-effecting).
///
/// Resolves the typescript to tail (pure: [`follow_target`]), loads the
/// operator's existing newt backend for the inference endpoint, builds the
/// read-only [`ObservationChannel`], then tails: each new chunk becomes a
/// redacted observation; after fresh activity, the agent is asked for a brief
/// comment which is printed. The channel is clamped read-only ([the agent can't
/// write/exec/net or call tools][gilamonster_agent::follow::read_only_caveats]),
/// so this loop can never touch the human's shell or the world.
async fn run_follow(
    logpath: Option<std::path::PathBuf>,
    dir: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let Some(target) = follow_target(&logpath, &dir) else {
        print!("{}", follow_no_target_report(&dir));
        return Ok(());
    };

    // Reuse the operator's first configured newt backend for the endpoint.
    let cfg = newt_core::Config::resolve()?;
    let backend = cfg.backends.first().ok_or_else(|| {
        anyhow::anyhow!("no inference backend configured — set one up in newt's config first")
    })?;
    let workspace = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let driver_config = config_from_backend(backend, workspace);

    println!(
        "gila follow (read-only): watching {} via {} [{}]. The agent observes only — it never drives your shell. Ctrl-C to stop.",
        target.display(),
        backend.model,
        backend.endpoint,
    );

    let mut channel = ObservationChannel::new(TurnDriver::new(driver_config));
    let mut tail = TypescriptTail::new(&target);
    let mut pending_since_comment = false;

    loop {
        match follow_tick(&mut channel, &mut tail) {
            FollowTick::Observed => pending_since_comment = true,
            FollowTick::Idle => {
                // No new activity. If something accumulated since the last
                // comment, ask the agent for one now, then print it.
                if pending_since_comment {
                    pending_since_comment = false;
                    if let Some(reply) =
                        drive_comment(&mut channel, Duration::from_millis(100), 1200).await?
                    {
                        let reply = reply.trim();
                        if !reply.is_empty() {
                            println!("gila> {reply}");
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            FollowTick::Exhausted => break,
        }
    }
    Ok(())
}
