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
use gilamonster_agent::fleet::{render_fleet_frame, FleetModel, Step};
use gilamonster_agent::follow::{
    config_from_backend, drive_comment, follow_tick, FollowTick, ObservationChannel, TypescriptTail,
};
use gilamonster_agent::hotseat::{compose_hotseat_config, hotseat_notice, triage_skill_name};
use gilamonster_agent::{
    capabilities, code_path, follow_no_target_report, follow_target, matrix_report,
    CapabilitiesCmd, Cli, Command,
};
use newt_core::agentic::{TurnDriver, TurnDriverConfig};
use newt_core::MemMessage;

/// Point newt-tui's brand seam at gila's own splash assets so `gila code` shows
/// the gilamonster silhouette (docs/logos/gilly-*.txt) and "gilamonster"
/// wordmark instead of newt's. Each var is set only if the operator hasn't
/// already, so an explicit environment override still wins.
///
/// Activation note: this consumes the runtime brand seam added in newt-agent
/// (PR #355 — NEWT_BRAND_LOGO_DIR/PREFIX/NAME/TAGLINE/PLUGINS). It takes visible
/// effect once the inherited `newt-tui` git dep is re-pinned to a rev that
/// includes that seam; until then these are inert env vars newt-tui ignores —
/// harmless, and compiles against the current pin.
#[allow(unused_unsafe)]
fn set_brand_defaults() {
    let set = |k: &str, v: &str| {
        if std::env::var_os(k).is_none() {
            // SAFETY: single-threaded — runs before any TUI/async work starts.
            unsafe { std::env::set_var(k, v) };
        }
    };
    set(
        "NEWT_BRAND_LOGO_DIR",
        concat!(env!("CARGO_MANIFEST_DIR"), "/docs/logos"),
    );
    set("NEWT_BRAND_LOGO_PREFIX", "gilly");
    set("NEWT_BRAND_NAME", "gilamonster");
    set("NEWT_BRAND_TAGLINE", "the agent matrix");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    set_brand_defaults();
    match Cli::parse().effective_command() {
        // Inherit: hand off to newt-agent's TUI directly. gilamonster's own
        // surfaces will wrap/extend this rather than reimplement it. `persona =
        // None` → newt's default persona; gila's personas land with the matrix
        // layer (#8-#11).
        Command::Code { path } => run_code_with_caps(path),
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
        // Hotseat: the on-call / triage cockpit. Compose the read-only floor
        // (#307 preset) + the triage skill + the modulex MCP search surface onto
        // the operator's config, write it to a session file, point newt at it,
        // and hand off to the inherited TUI. The operator engages the clamp with
        // `/mode hotseat`. All composition logic is in `hotseat.rs` (unit-tested);
        // this arm owns only the config write + TUI hand-off (the by-design-
        // uncovered surface, same carve-out as `gila code`).
        Command::Hotseat { path, skill } => run_hotseat(path, skill),
        // Capabilities: the host side of the capability framework. `list`
        // enumerates installed gila-cap-* packages, `check` connects to one over
        // newt's real MCP client and exercises it, `enable` prints the wiring
        // snippet. Logic lives in `capabilities.rs`; `check` owns the live MCP
        // round-trip (the by-design-uncovered subprocess surface).
        Command::Capabilities { cmd } => match cmd {
            CapabilitiesCmd::List => capabilities::list(),
            CapabilitiesCmd::Check { name } => capabilities::check(&name).await,
            CapabilitiesCmd::Enable { name } => capabilities::enable(&name),
            CapabilitiesCmd::Run { name, tool, args } => capabilities::run(&name, &tool, args),
            CapabilitiesCmd::Config => capabilities::config(),
        },
        // The matrix is the FleetView crew-monitor dashboard. `--mock` opens the
        // full-screen dashboard over a canned roster (Phase 1: the standalone
        // surface); bare `gila matrix` prints the scaffold notice. The render
        // path + roster are unit-tested in `fleet.rs`; this arm owns only the raw
        // terminal loop (the by-design-uncovered tty surface) and the print.
        Command::Matrix { mock } => run_matrix(mock),
        // Cockpit: the tmux-semantics multiplexer. This first slice renders the
        // tab/pane layout (composing cockpit.rs + layout.rs) and drives it with
        // the keys.rs prefix dispatcher; live per-pane drivers + the ambient
        // shell PTY land in the next ratchet. Raw terminal loop = the same
        // by-design-uncovered carve-out as run_cowork / run_fleet_dashboard.
        Command::Cockpit { path } => run_cockpit(path),
        // Chain: one question through a LangChain LLMChain against the
        // configured newt backend. All logic is in `chain.rs` (unit + mocked
        // end-to-end tests); this arm owns only the config resolve + print,
        // the same by-design-uncovered carve-out as the other run_* arms.
        Command::Chain { question } => run_chain(question).await,
        // Scrybe: a live Markdown session against a scrybe MCP peer — gila is
        // the client, scrybe owns the doc, edits flow both ways (see `scrybe`
        // and docs/design/scrybe-markdown-surface.md). Phase 1 prints the
        // resolved config and exits; the live MCP loop lands in later phases.
        Command::Scrybe { uri, doc_path } => {
            run_scrybe(&uri, doc_path.as_deref().and_then(|p| p.to_str()))
        }
        // Rust-native git (Phase 1 of the parity plan): commit via libgit2,
        // tend via profile steps that shell out to the git CLI (matching the
        // Python engine 1:1). Graduated out of the shell-delegate fallback.
        Command::Git { cmd } => run_git(cmd),
        // Phase 3 Rust-native commands (batch 1): version/daily/ideas/todos/
        // projects/board/cache. Logic lives in the `gila_*` lib modules
        // (unit-tested); these arms own only HOME resolution + file effect.
        Command::Version => {
            println!("{}", gilamonster_agent::gila_version::version_report());
            Ok(())
        }
        Command::Daily { date } => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let dir = gilamonster_agent::gila_daily::daily_dir(home.as_deref())?;
            let date = date.unwrap_or_else(today);
            let p = gilamonster_agent::gila_daily::ensure_daily_note(&dir, &date)?;
            println!("{}", p.display());
            Ok(())
        }
        Command::Ideas { idea, list } => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let path = gilamonster_agent::gila_ideas::ideas_path(home.as_deref())?;
            if list {
                match std::fs::read_to_string(&path) {
                    Ok(body) => print!("{body}"),
                    Err(_) => println!("no ideas captured yet"),
                }
                return Ok(());
            }
            let text = idea.join(" ");
            if text.is_empty() {
                anyhow::bail!("no idea given — pass text or `--list`");
            }
            gilamonster_agent::gila_ideas::append_idea(&path, &today(), &text)?;
            println!("captured: {text}");
            Ok(())
        }
        Command::Todos { text, list, done } => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let path = gilamonster_agent::gila_todos::todos_path(home.as_deref())?;
            if let Some(n) = done {
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                let out = gilamonster_agent::gila_todos::mark_done(&body, n)
                    .ok_or_else(|| anyhow::anyhow!("no open todo #{n}"))?;
                std::fs::write(&path, out)?;
                println!("done: #{n}");
                return Ok(());
            }
            if list {
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                let open = gilamonster_agent::gila_todos::list_open(&body);
                if open.is_empty() {
                    println!("no open todos");
                } else {
                    for l in open {
                        println!("{l}");
                    }
                }
                return Ok(());
            }
            let t = text.join(" ");
            if t.is_empty() {
                anyhow::bail!("no todo given — pass text, `--list`, or `--done N`");
            }
            gilamonster_agent::gila_todos::add_todo(&path, &t)?;
            println!("added: {t}");
            Ok(())
        }
        Command::Projects => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let root = gilamonster_agent::gila_projects::workspace_root(home.as_deref())?;
            let projs = gilamonster_agent::gila_projects::list_projects(&root);
            print!("{}", gilamonster_agent::gila_projects::render_projects(&projs));
            Ok(())
        }
        Command::Board => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let dir = gilamonster_agent::gila_board::board_dir(home.as_deref())?;
            let files = gilamonster_agent::gila_board::list_board_files(&dir);
            print!("{}", gilamonster_agent::gila_board::render_board(&files));
            Ok(())
        }
        Command::Cache { clear } => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let dir = gilamonster_agent::gila_cache::cache_dir(home.as_deref())?;
            if clear {
                let n = gilamonster_agent::gila_cache::clear(&dir)?;
                println!("cleared {n} entr(y/ies) from {}", dir.display());
            } else {
                let s = gilamonster_agent::gila_cache::status(&dir);
                print!("{}", gilamonster_agent::gila_cache::render_status(&dir, &s));
            }
            Ok(())
        }
        Command::Logs { limit } => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let dir = gilamonster_agent::gila_logs::logs_dir(home.as_deref())?;
            let entries = gilamonster_agent::gila_logs::recent_logs(&dir, limit);
            print!("{}", gilamonster_agent::gila_logs::render_logs(&entries));
            Ok(())
        }
        Command::Prompt { cmd } => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let dir = gilamonster_agent::gila_prompt::prompts_dir(home.as_deref())?;
            match cmd {
                gilamonster_agent::PromptCmd::List => {
                    let names = gilamonster_agent::gila_prompt::list_prompts(&dir);
                    if names.is_empty() {
                        println!("no prompts found");
                    } else {
                        for n in names {
                            println!("{n}");
                        }
                    }
                }
                gilamonster_agent::PromptCmd::Show { name } => {
                    let p = dir.join(format!("{name}.md"));
                    match std::fs::read_to_string(&p) {
                        Ok(body) => print!("{body}"),
                        Err(_) => anyhow::bail!("prompt `{name}` not found at {}", p.display()),
                    }
                }
                gilamonster_agent::PromptCmd::Create { name } => {
                    let p = gilamonster_agent::gila_prompt::create_prompt(&dir, &name)?;
                    println!("created {}", p.display());
                }
            }
            Ok(())
        }
        Command::CommitMsg { message, file } => {
            let msg = if file {
                std::fs::read_to_string(&message)
                    .map_err(|e| anyhow::anyhow!("reading message file {message}: {e}"))?
            } else {
                message
            };
            match gilamonster_agent::gila_commit_msg::validate(&msg) {
                Ok(()) => {
                    println!("commit message OK");
                    Ok(())
                }
                Err(reason) => {
                    eprintln!("invalid commit message: {reason}");
                    std::process::exit(1);
                }
            }
        }
        Command::Completion { shell } => {
            let sh = gilamonster_agent::gila_completion::Shell::parse(&shell).ok_or_else(|| {
                anyhow::anyhow!("unsupported shell `{shell}` (supported: bash, zsh)")
            })?;
            print!("{}", gilamonster_agent::gila_completion::completion_script(sh));
            Ok(())
        }
        Command::Init => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let created = gilamonster_agent::gila_init::init(home.as_deref())?;
            if created.is_empty() {
                println!("gila config already initialized");
            } else {
                for d in created {
                    println!("created {}", d.display());
                }
            }
            Ok(())
        }
        Command::Update { repo } => {
            let repo = repo.unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")));
            gilamonster_agent::gila_update::run_update(&repo)?;
            println!("updated {}", repo.display());
            Ok(())
        },
        // Shell-delegate fallback: any subcommand not yet ported from gilabot
        // (Python). clap's external_subcommand catch-all captured the name +
        // args; re-exec the real gilabot binary so every gilabot command works
        // from day one (Phase 1 of the parity plan). Commands graduate out of
        // this arm as they gain their own `Command` variant.
        Command::External(args) => run_delegate(&args),
    }
}

/// `gila <unported>` — shell-delegate to the Python gilabot binary.
///
/// Resolves the *other* `gila` on `PATH` (never our own binary, so we don't
/// recurse), warns that the command is delegated, and execs gilabot with the
/// original arguments. The exec itself is the by-design-uncovered subprocess
/// surface; the resolution + arg logic lives in [`gilamonster_agent::delegate`]
/// and is unit-tested.
fn run_delegate(args: &[std::ffi::OsString]) -> anyhow::Result<()> {
    use gilamonster_agent::delegate::{delegate_args, path_dirs, resolve_gilabot};

    let cmd_name = args
        .first()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown>".to_string());

    // Phase 2: route the high-complexity Python-vendored commands in-process
    // via pyo3 (no subprocess startup cost). Everything else falls through to
    // the shell-delegate subprocess below. If the in-process import fails (venv
    // not embedded), surface the error — don't silently double-fallback.
    if gilamonster_agent::python_bridge::is_pyo3_routed(&cmd_name) {
        let code = gilamonster_agent::python_bridge::run_python_command(&cmd_name, &args[1..])?;
        std::process::exit(code);
    }

    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let dirs = path_dirs(&path_var);
    let own_exe = std::env::current_exe().ok();

    let gilabot = resolve_gilabot(&dirs, own_exe.as_ref()).ok_or_else(|| {
        anyhow::anyhow!(
            "`gila {cmd_name}` is not yet implemented in gilamonster-agent, and no \
             Python gilabot (`gila`) was found on PATH to delegate to. Install \
             gilabot or file an issue to port `{cmd_name}`."
        )
    })?;

    eprintln!(
        "gila: `{cmd_name}` is delegated to Python gilabot ({}) — not yet \
         Rust-native.",
        gilabot.display()
    );

    let status = std::process::Command::new(gilabot)
        .args(delegate_args(args))
        .status()?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Today's date as `YYYY-MM-DD`, via the `date` binary (avoids a chrono dep;
/// the modules stay date-injected and hermetic, this is the effectful seam).
fn today() -> String {
    std::process::Command::new("date")
        .arg("+%F")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-date".to_string())
}

/// `gila git …` — the Rust-native Phase-1 slice (commit via libgit2, tend via
/// git-CLI profile steps). The lib-side logic is unit-tested in
/// [`gilamonster_agent::gila_git`]; this arm owns only argv → effect.
fn run_git(cmd: gilamonster_agent::GitCmd) -> anyhow::Result<()> {
    use gilamonster_agent::gila_git;
    use gilamonster_agent::GitCmd;

    match cmd {
        GitCmd::Commit { message, path } => {
            let path = path.unwrap_or_else(|| std::path::PathBuf::from("."));
            match gila_git::commit_all(&path, &message)? {
                Some(oid) => println!("{} committed {}", path.display(), &oid.to_string()[..7]),
                None => println!("{}: nothing to commit", path.display()),
            }
            Ok(())
        }
        GitCmd::Tend {
            config,
            dry_run,
            profile,
        } => {
            let config_path = match config {
                Some(p) => p,
                None => gila_git::default_config_path()?,
            };
            let cfg = gila_git::load_config(&config_path)?;
            let mut failed = 0usize;
            for repo in &cfg.repos {
                if let Some(pf) = &profile {
                    if !repo.profiles(&cfg.defaults).contains(pf) {
                        continue;
                    }
                }
                let report = gila_git::tend_repo(repo, &cfg, dry_run);
                if dry_run {
                    println!("{}: dry-run complete", report.path.display());
                } else if report.success {
                    println!("{}: ok", report.path.display());
                } else {
                    failed += 1;
                    eprintln!(
                        "{}: {}",
                        report.path.display(),
                        report.error.unwrap_or_else(|| "failed".to_string())
                    );
                }
            }
            if failed > 0 {
                anyhow::bail!("{failed} repo(s) failed to tend");
            }
            Ok(())
        }
    }
}

/// `gila chain` — run one question through the LangChain LLMChain.
///
/// Resolves the operator's first configured newt backend (the same seam
/// `gila follow` / `gila cowork` use), maps it through
/// [`chain::settings_from_backend`](gilamonster_agent::chain::settings_from_backend)
/// (fail-loud on an unset model), and prints the chain's reply.
async fn run_chain(question: Vec<String>) -> anyhow::Result<()> {
    use gilamonster_agent::chain::{ask, settings_from_backend};
    let cfg = newt_core::Config::resolve()?;
    let backend = cfg.backends.first().ok_or_else(|| {
        anyhow::anyhow!("no inference backend configured — set one up in newt's config first")
    })?;
    let settings = settings_from_backend(backend)?;
    let question = question.join(" ");
    eprintln!(
        "gila chain: LLMChain → {} [{}]",
        settings.model, settings.base_url
    );
    let reply = ask(&settings, &question).await?;
    println!("{reply}");
    Ok(())
}

/// `gila scrybe` — open a live Markdown session against a scrybe MCP peer.
///
/// Phase 1: resolve + validate the [`ScrybeConfig`](gilamonster_agent::scrybe::ScrybeConfig),
/// print it, and exit (the by-design-uncovered surface). The full MCP loop
/// lives in [`gilamonster_agent::scrybe`] and will be wired once the handshake
/// is implemented.
fn run_scrybe(server_uri: &str, doc_path: Option<&str>) -> anyhow::Result<()> {
    use gilamonster_agent::scrybe::{build_config, validate_config, ScrybeConfig};
    let mut config: ScrybeConfig = build_config(doc_path);
    config.server_uri = server_uri.to_string();
    if let Err(e) = validate_config(&config) {
        anyhow::bail!("{e}");
    }
    println!("scrybe session starting");
    println!("  server: {}", config.server_uri);
    println!("  doc:    {}", config.doc_path.display());
    println!("Phase 1 scaffold — the live MCP loop lands in later phases.");
    Ok(())
}

/// The cockpit raw render/event loop (binary-owned, the by-design-uncovered tty
/// surface). Wires the tested [`Cockpit`](gilamonster_agent::cockpit::Cockpit)
/// model + the [`keys`](gilamonster_agent::keys) dispatcher to a real terminal;
/// all decision logic (the action state machine, key routing, tab bar, pane
/// labels) is unit-tested in `cockpit.rs`.
fn run_cockpit(_path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    use crossterm::event::{self, Event, KeyEventKind};
    use gilamonster_agent::cockpit::{route_cockpit_key, tab_bar, Cockpit, CockpitKey};
    use gilamonster_agent::cowork::to_key_combo;
    use gilamonster_agent::keys::KeyDispatcher;
    use gilamonster_agent::layout::Rect as LRect;
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::{Alignment, Rect};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::widgets::{Block, Borders, Paragraph};
    use ratatui::Terminal;

    let mut cockpit = Cockpit::new();
    let mut dispatcher = KeyDispatcher::default();

    setup_terminal()?;
    let mut guard = TerminalGuard::new(restore_terminal);
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut quit = false;
    let loop_result: anyhow::Result<()> = (|| {
        while !quit {
            terminal.draw(|frame| {
                let area = frame.area();
                // Top row: the tab bar. Everything below: the panes.
                let bar = tab_bar(&cockpit.tab_titles(), cockpit.active_tab());
                frame.render_widget(
                    Paragraph::new(bar).style(Style::default().fg(Color::Cyan)),
                    Rect::new(area.x, area.y, area.width, 1),
                );
                let panes_area = LRect::new(0, 1, area.width, area.height.saturating_sub(1));
                let focused = cockpit.focused_pane();
                for (pane, r) in cockpit.rects(panes_area) {
                    let role = cockpit.pane_role(pane);
                    let is_focused = pane == focused;
                    let border = if is_focused {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let label = role
                        .map(|role| gilamonster_agent::cockpit::pane_label(role, pane, is_focused))
                        .unwrap_or_default();
                    let widget = Paragraph::new(label)
                        .alignment(Alignment::Center)
                        .block(Block::default().borders(Borders::ALL).border_style(border));
                    frame.render_widget(widget, Rect::new(r.x, r.y, r.w, r.h));
                }
            })?;

            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Release {
                        if let Some(combo) = to_key_combo(key.code, key.modifiers) {
                            match route_cockpit_key(
                                &mut dispatcher,
                                combo,
                                std::time::Instant::now(),
                            ) {
                                CockpitKey::Quit => quit = true,
                                CockpitKey::Do(action) => {
                                    cockpit.apply(action);
                                }
                                CockpitKey::Ignore => {}
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    })();

    guard.restore();
    loop_result
}

/// `gila matrix` — print the scaffold notice, or (`--mock`) open the FleetView
/// dashboard.
///
/// Bare `gila matrix` runs under the same inherited object-capability identity
/// as newt: it surfaces where the operator key lives ([`matrix_report`], pure +
/// unit-tested) and returns. `--mock` hands off to the full-screen dashboard
/// loop over the canned [`FleetModel::mock`] roster.
fn run_matrix(mock: bool) -> anyhow::Result<()> {
    if !mock {
        print!("{}", matrix_report(newt_identity::default_key_path()));
        return Ok(());
    }
    run_fleet_dashboard(FleetModel::mock())
}

/// The live FleetView dashboard loop (binary-owned, the by-design-uncovered tty
/// surface).
///
/// Puts the terminal into raw mode + the alternate screen under a
/// [`TerminalGuard`] so it is **always** restored (clean exit, error, or panic
/// mid-render), then draws the dashboard each frame via the tested
/// [`render_fleet_frame`] and folds each key press through the tested
/// [`FleetModel::apply_key`] navigation state machine (↑↓ select, →/Enter drill
/// in, Esc/← back, `f` freeze, `q`/`Ctrl-C` quit). All render/layout/navigation
/// logic lives in the tested `fleet.rs`; this function only wires it to a real
/// terminal — the same carve-out `run_cowork` uses.
fn run_fleet_dashboard(mut model: FleetModel) -> anyhow::Result<()> {
    use crossterm::event::{self, Event, KeyEventKind};
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;

    setup_terminal()?;
    // The guard restores the terminal on a clean return, on an error unwinding
    // out of this function, AND on a panic mid-render.
    let mut guard = TerminalGuard::new(restore_terminal);
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let loop_result: anyhow::Result<()> = (|| {
        loop {
            terminal.draw(|f| render_fleet_frame(&model, f))?;

            // Block until a key (short timeout keeps resize redraws responsive),
            // then fold it through the navigation state machine.
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Release && model.apply_key(key) == Step::Quit {
                        break;
                    }
                }
            }
        }
        Ok(())
    })();

    // Restore deterministically before any post-exit output; Drop is then a
    // no-op (it would still fire on the error/panic paths above).
    guard.restore();
    loop_result
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
    let driver_config = config_from_backend(backend, workspace)?;

    println!(
        "gila follow (read-only): watching {} via {} [{}]. The agent observes only — it never drives your shell. Ctrl-C to stop.",
        target.display(),
        driver_config.model,
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
/// The bottom pane hosts the human's **real** `$SHELL` on a pseudo-terminal
/// ([`PtyShell`]): `ssh`, `vim`, `less` — the full command suite — run natively.
/// Each frame this loop (1) refreshes the shell grid from the PTY's vt100 screen
/// into the app, (2) feeds the shell's new output into the SAME #8
/// [`ObservationChannel`] as `gila follow` (so the agent observes the human and
/// assists, redaction-gated by construction), and (3) routes keystrokes by focus
/// — to the chat input when the chat pane has focus, or, when `Ctrl-O` swaps
/// focus to the shell pane, encoded straight to the PTY. The PTY resizes with the
/// pane.
///
/// All decision logic (focus swap, input edit/submit, status transitions, the
/// vt100-grid → ratatui mapping, the key→PTY encoding, the resize math, the
/// channel feed, the layout, both RAII guards' teardown-on-drop) is unit-tested
/// in `cowork.rs` / `pty.rs`; this function only wires those tested units to a
/// real terminal + a real shell, which is the carve-out the coverage gate
/// excludes — the same shape `gila follow` uses for its live tail loop.
fn run_cowork(path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use gilamonster_agent::pty::{pty_shell_program, pty_size_for, screen_to_lines, PtyShell};
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
    // newt #1128 made `model`/`kind` optional ("probe at session start");
    // cowork has no probe step, so an unset model fails loud (same shape as
    // `gila follow` / `newt solve`) and an unset kind adopts newt's OpenAI
    // wire default.
    let model = backend.effective_model().ok_or_else(|| {
        anyhow::anyhow!(
            "backend `{}` has no model (set model = in the [[backends]] entry)",
            backend.name
        )
    })?;
    let kind = backend.kind.unwrap_or(newt_core::BackendKind::Openai);
    let mut driver_config = TurnDriverConfig::new(&backend.endpoint, model, kind, workspace);
    driver_config.api_key = backend.resolve_api_key();

    // Seed the cowork framing so the agent knows it shares a screen with the
    // human's shell.
    let driver = TurnDriver::with_transcript(
        driver_config,
        vec![MemMessage::system(COWORK_SYSTEM_PROMPT)],
    );
    let mut app = CoworkApp::new(ObservationChannel::new(driver));
    // #48: the tmux prefix dispatcher. Every keystroke passes through it before
    // reaching the PTY, so a bare Ctrl+B arms a prefix instead of leaking 0x02
    // into the user's shell.
    let mut dispatcher = gilamonster_agent::keys::KeyDispatcher::default();

    // --- terminal setup under an RAII guard (restored on EVERY exit) ---------
    setup_terminal()?;
    // The guard restores the terminal on a clean return, on an error unwinding
    // out of this function, AND on a panic mid-render. Nothing below may early-
    // return without the guard on the stack.
    let mut guard = TerminalGuard::new(restore_terminal);

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    // --- spawn the human's real shell on a PTY, sized to the shell pane -------
    // Compute the shell pane's size from the current terminal area, then spawn
    // the human's $SHELL (fallback /bin/bash) on a fresh pty. The PtyShell owns
    // an RAII guard that kills the child + joins the reader thread on drop, so a
    // clean quit, an error, or a panic never orphans the shell.
    let initial_area = terminal.size()?;
    let initial_rect = ratatui::layout::Rect::new(0, 0, initial_area.width, initial_area.height);
    let shell_area = gilamonster_agent::cowork::split_panes(initial_rect).shell;
    // The write half is split off into a non-`Clone` `PtyWriter` owned only here
    // in the input router; the agent-facing observation channel never sees it,
    // so the agent is structurally unable to type into the human's shell.
    let (mut shell, mut shell_writer) = PtyShell::spawn(
        &pty_shell_program(),
        pty_size_for(shell_area),
        path.as_deref(),
    )?;
    // The PTY's output also feeds the SAME #8 observation channel the chat side
    // uses, so the agent observes the human's shell. We drain it via the source.
    let mut pty_source = shell.observation_source();
    let shared_screen = shell.shared();
    // Track the last shell-pane size so we only resize the pty on a real change.
    let mut last_shell_area = shell_area;

    let loop_result: anyhow::Result<()> = (|| {
        while !app.should_quit() {
            // 1. Pump the driver without blocking — the chat updates as the turn
            //    progresses.
            app.pump();

            // 2. Drain the shell's new output into the #8 channel (redaction-gated
            //    by construction) so the agent observes the human's activity.
            gilamonster_agent::follow::follow_tick(app.channel(), &mut pty_source);

            // 3. Refresh the shell pane from the PTY's current vt100 grid.
            if let Ok(s) = shared_screen.lock() {
                app.set_shell_lines(screen_to_lines(s.screen()));
            }

            // 4. Draw the split. The whole render path lives in the tested
            //    `render_frame`; this loop owns only the terminal + input.
            terminal.draw(|f| render_frame(&mut app, f))?;

            // 5. Resize the pty if the shell pane changed size (terminal resize).
            let area = terminal.size()?;
            let rect = ratatui::layout::Rect::new(0, 0, area.width, area.height);
            let new_shell_area = gilamonster_agent::cowork::split_panes(rect).shell;
            if new_shell_area != last_shell_area {
                shell.resize(new_shell_area);
                last_shell_area = new_shell_area;
            }

            // 6. Poll input non-blocking (short timeout keeps the pump cadence).
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Release {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        // Ctrl-Q quits and Ctrl-O swaps focus, from EITHER pane —
                        // the cockpit's direct global keys. They are held while a
                        // prefix is armed, so a post-prefix key always resolves
                        // through the dispatcher (never as a stray global).
                        if !dispatcher.is_armed() && ctrl && key.code == KeyCode::Char('q') {
                            app.request_quit();
                        } else if !dispatcher.is_armed() && ctrl && key.code == KeyCode::Char('o') {
                            app.swap_focus();
                        } else {
                            // Everything else routes through the prefix dispatcher
                            // FIRST (kills the Ctrl+B → 0x02 leak); the pure router
                            // in cowork.rs decides what actually happens.
                            use gilamonster_agent::cowork::CoworkKey;
                            match gilamonster_agent::cowork::route_key(
                                &mut dispatcher,
                                key.code,
                                key.modifiers,
                                app.focus(),
                                std::time::Instant::now(),
                            ) {
                                CoworkKey::Pty(bytes) => {
                                    let _ = shell_writer.write_input(&bytes);
                                }
                                CoworkKey::Submit => {
                                    app.submit_input();
                                }
                                CoworkKey::Backspace => app.backspace(),
                                CoworkKey::Char(c) => app.push_char(c),
                                // Focus actions the scaffold already supports; the
                                // rest of the cockpit actions land in later phases.
                                CoworkKey::Action(
                                    gilamonster_agent::keys::Action::FocusNext
                                    | gilamonster_agent::keys::Action::FocusLast,
                                ) => app.swap_focus(),
                                CoworkKey::Action(_) | CoworkKey::Absorbed | CoworkKey::Ignore => {}
                            }
                        }
                    }
                }
            }

            // 7. If the human exited their shell, leave cowork too.
            if shell.has_exited() {
                app.request_quit();
            }
        }
        Ok(())
    })();

    // Restore deterministically before printing anything post-exit; Drop is then
    // a no-op. (Drop would still fire on an error/panic path above.)
    guard.restore();
    loop_result
}

/// Launch the **hotseat** on-call / triage cockpit (binary-owned, side-effecting).
///
/// Composition lives in [`compose_hotseat_config`] (pure, unit-tested); this arm
/// only performs the side effects the library can't:
///
/// 1. Resolve the operator's existing newt [`Config`](newt_core::Config) (their
///    backends, skill search path, any MCP servers).
/// 2. Resolve the triage skill name ([`triage_skill_name`]) and overlay the
///    hotseat preset + mode + modulex MCP entry ([`compose_hotseat_config`]).
/// 3. Serialize the composed config to a per-session file and point
///    `$NEWT_CONFIG` at it — the highest-precedence config source newt resolves —
///    so the inherited TUI sees the hotseat mode/preset/MCP alongside everything
///    the operator already had.
/// 4. Print the [`hotseat_notice`] (read-only contract + the `/mode hotseat`
///    engage step) and hand off to newt's TUI exactly as `gila code` does.
///
/// The read-only authority FLOOR is engaged in-session with `/mode hotseat`
/// (newt #307): the preset clamp is `meet`-ed into the session authority and
/// wins over `--yolo` / session grants. The config write + TUI hand-off are the
/// only by-design-uncovered lines (they need a real tty + a writable session
/// dir); the composition they wrap is fully unit-tested in `hotseat.rs`.
fn run_hotseat(path: Option<std::path::PathBuf>, skill: Option<String>) -> anyhow::Result<()> {
    let skill = triage_skill_name(skill.as_deref());

    // Start from the operator's resolved config so hotseat is an OVERLAY, not a
    // replacement — their backends, skills path, and MCP servers carry through.
    let base = newt_core::Config::resolve()?;
    let composed = compose_hotseat_config(base, &skill);

    // Serialize the composed config to a per-session file and point newt at it
    // via $NEWT_CONFIG (its highest-precedence config source). Keyed on the PID
    // so concurrent hotseat sessions don't collide.
    let session_path =
        std::env::temp_dir().join(format!("gila-hotseat-{}.toml", std::process::id()));
    let toml = toml::to_string(&composed)
        .map_err(|e| anyhow::anyhow!("failed to serialize hotseat session config: {e}"))?;
    std::fs::write(&session_path, toml).map_err(|e| {
        anyhow::anyhow!("failed to write hotseat session config {session_path:?}: {e}")
    })?;
    std::env::set_var("NEWT_CONFIG", &session_path);

    print!("{}", hotseat_notice(&skill));

    // Hand off to the inherited TUI exactly as `gila code` does; the operator
    // engages the read-only floor in-session with `/mode hotseat`.
    newt_tui::run_code(code_path(&path), false, None, None, None, None)
}

/// `gila code` — the inherited TUI, with the **opted-in capabilities auto-mounted**
/// as agent tools.
///
/// If the selection manifest (`~/.gila/capabilities.toml`, written by `gila cap
/// config`) exposes any capability to the agent surface (`expose = agent|both`),
/// compose those `[[mcp_servers]]` onto the operator's resolved newt config, write
/// a per-session file, and point `$NEWT_CONFIG` at it before handing off — the same
/// overlay pattern `gila hotseat` uses. With no agent-exposed caps (the default),
/// hand off to the inherited TUI unchanged, so plain `gila code` is untouched. The
/// config write + TUI launch are the by-design-uncovered surface (real tty +
/// filesystem), mirroring `run_hotseat` / `run_cowork`.
fn run_code_with_caps(path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let entries = capabilities::agent_mcp_entries();
    if entries.is_empty() {
        return newt_tui::run_code(code_path(&path), false, None, None, None, None);
    }
    let mounted = entries.len();
    let base = newt_core::Config::resolve()?;
    let composed = capabilities::compose_agent_mcp(base, entries);
    let session_path = std::env::temp_dir().join(format!("gila-caps-{}.toml", std::process::id()));
    let toml = toml::to_string(&composed)
        .map_err(|e| anyhow::anyhow!("failed to serialize capability session config: {e}"))?;
    std::fs::write(&session_path, toml).map_err(|e| {
        anyhow::anyhow!("failed to write capability session config {session_path:?}: {e}")
    })?;
    std::env::set_var("NEWT_CONFIG", &session_path);
    eprintln!("→ gila: mounted {mounted} capability MCP server(s) as agent tools");
    newt_tui::run_code(code_path(&path), false, None, None, None, None)
}
