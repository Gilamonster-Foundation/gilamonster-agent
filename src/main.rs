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

use std::io;
use std::time::Duration;

use clap::Parser;
use gilamonster_agent::cowork::{
    render_frame, restore_terminal, setup_terminal, CoworkApp, TerminalGuard, COWORK_SYSTEM_PROMPT,
};
use gilamonster_agent::follow::{
    config_from_backend, drive_comment, follow_tick, FollowTick, ObservationChannel, TypescriptTail,
};
use gilamonster_agent::{
    code_path, follow_no_target_report, follow_target, matrix_report, Cli, Command,
};
use newt_core::agentic::{TurnDriver, TurnDriverConfig};
use newt_core::MemMessage;

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
        // Cowork: the full-screen split-pane cockpit — agent chat on top, the
        // human's (placeholder, until #10) live shell below. Separate command so
        // the inherited inline REPL (`gila code`) never regresses. All the
        // testable logic lives in `cowork.rs`; this arm owns only the raw
        // terminal render/event loop, the by-design-uncovered tty surface.
        Command::Cowork { path } => run_cowork(path),
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

/// The live `gila cowork` cockpit (binary-owned, the by-design-uncovered tty
/// surface).
///
/// Builds the [`CoworkApp`] around an [`ObservationChannel`] (the same seam #10's
/// PTY source will feed), puts the terminal into raw mode + the alternate screen
/// under a [`TerminalGuard`] so it is **always** restored, then runs the
/// non-blocking render/event loop: each frame it pumps the [`TurnDriver`]
/// without blocking ([`CoworkApp::pump`]), draws the split via [`split_panes`],
/// and polls crossterm for input with a short timeout so the chat updates as the
/// turn progresses while the UI stays responsive.
///
/// All decision logic (focus swap, input edit/submit, status transitions, the
/// line mapping, the layout math, the guard's restore-on-drop) is unit-tested in
/// `cowork.rs`; this function only wires those tested units to a real terminal,
/// which is the carve-out the coverage gate excludes — the same shape `gila
/// follow` uses for its live tail loop.
fn run_cowork(path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;

    // Build the driver against the operator's first configured newt backend, so
    // cowork talks to the same inference endpoint as the rest of the airframe.
    let cfg = newt_core::Config::resolve()?;
    let backend = cfg.backends.first().ok_or_else(|| {
        anyhow::anyhow!("no inference backend configured — set one up in newt's config first")
    })?;
    let workspace = match &path {
        Some(p) => p.display().to_string(),
        None => std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string()),
    };
    let mut driver_config =
        TurnDriverConfig::new(&backend.endpoint, &backend.model, backend.kind, workspace);
    driver_config.api_key = backend.resolve_api_key();

    // Seed the cowork framing so the agent knows it shares a screen with the
    // human's shell.
    let driver = TurnDriver::with_transcript(
        driver_config,
        vec![MemMessage::system(COWORK_SYSTEM_PROMPT)],
    );
    let mut app = CoworkApp::new(ObservationChannel::new(driver));

    // --- terminal setup under an RAII guard (restored on EVERY exit) ---------
    setup_terminal()?;
    // The guard restores the terminal on a clean return, on an error unwinding
    // out of this function, AND on a panic mid-render. Nothing below may early-
    // return without the guard on the stack.
    let mut guard = TerminalGuard::new(restore_terminal);

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let loop_result: anyhow::Result<()> = (|| {
        while !app.should_quit() {
            // 1. Pump the driver without blocking — the chat updates as the turn
            //    progresses.
            app.pump();

            // 2. Draw the split. The whole render path lives in the tested
            //    `render_frame`; this loop owns only the terminal + input.
            terminal.draw(|f| render_frame(&mut app, f))?;

            // 3. Poll input non-blocking (short timeout keeps the pump cadence).
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Release {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        match key.code {
                            // Ctrl-Q quits.
                            KeyCode::Char('q') if ctrl => app.request_quit(),
                            // Ctrl-O swaps focus between the panes.
                            KeyCode::Char('o') if ctrl => app.swap_focus(),
                            // Enter submits the chat input (chat pane only).
                            KeyCode::Enter => {
                                app.submit_input();
                            }
                            KeyCode::Backspace => app.backspace(),
                            KeyCode::Char(c) if !ctrl => app.push_char(c),
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(())
    })();

    // Restore deterministically before printing anything post-exit; Drop is then
    // a no-op. (Drop would still fire on an error/panic path above.)
    guard.restore();
    loop_result
}
