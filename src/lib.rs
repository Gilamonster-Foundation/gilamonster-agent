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

pub mod authority;
pub mod capabilities;
pub mod cockpit;
pub mod cowork;
pub mod fleet;
pub mod follow;
pub mod hotseat;
pub mod keys;
pub mod layout;
pub mod manifest;
pub mod pty;
pub mod scrybe;
pub mod venv;

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

/// `gila` subcommands. `Code` inherits newt's TUI; `Follow` is the read-only
/// "follow me" shell observer; `Matrix` is the (not-yet built) extension layer.
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum Command {
    /// Run the inherited newt chat + agentic-coding TUI (the airframe).
    Code {
        /// Optional working path.
        path: Option<PathBuf>,
    },
    /// Read-only "follow me": watch the human's own shell (a `script -F`
    /// typescript) and let the agent comment, never driving the shell.
    Follow {
        /// Path to the `script -F` typescript to tail. When omitted, the newest
        /// file in the watch directory is followed (see `--dir`).
        logpath: Option<PathBuf>,
        /// Directory to search for the newest typescript when `logpath` is
        /// omitted. Defaults to the current directory.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Open the **cowork** split-pane cockpit: agent chat (top) + the human's
    /// live shell (bottom). Tier B/1 ships the non-blocking full-screen scaffold
    /// with a placeholder shell pane; the PTY shell lands in #10. Separate from
    /// `gila code` so the inherited SSH-safe inline REPL never regresses.
    Cowork {
        /// Optional working path the cowork session runs against.
        path: Option<PathBuf>,
    },
    /// Open the **hotseat** on-call / triage cockpit: launch the inherited TUI
    /// under a read-only / ack-only posture (newt's #307 named-permission-preset
    /// FLOOR), with a triage runbook skill preloaded and **authenticated MCP
    /// search via the modulex proxy** wired as a stdio tool surface. gila ships
    /// the generic composition only — the enterprise search targets + credentials
    /// live in the operator's private modulex config. Engage the floor with
    /// `/mode hotseat` once the TUI is up.
    Hotseat {
        /// Optional working path the hotseat session runs against.
        path: Option<PathBuf>,
        /// Triage skill name to preload. Defaults to the generic
        /// [`hotseat::DEFAULT_TRIAGE_SKILL`]; the skill *body* is operator config
        /// on the newt skill search path. Also settable via
        /// [`hotseat::TRIAGE_SKILL_ENV`].
        #[arg(long)]
        skill: Option<String>,
    },
    /// Discover and exercise installed Gilamonster **capabilities** — pip-installed
    /// `gila-cap-*` packages that expose tools over MCP. The host side of the
    /// capability framework (see `Gilamonster-Foundation/gilamonster-capabilities`).
    #[command(alias = "cap")]
    Capabilities {
        #[command(subcommand)]
        cmd: CapabilitiesCmd,
    },
    /// The multi-agent matrix — the **FleetView** crew-monitor dashboard.
    ///
    /// With `--mock` it opens the full-screen FleetView dashboard over a canned
    /// demo roster (Phase 1: the standalone surface, no live crew yet — see
    /// [`fleet`] and `docs/decisions/fleetview_full_screen_dashboard.md`). Bare
    /// `gila matrix` prints the scaffold notice (a side-effect-free path that
    /// never surprises the operator with an alternate screen).
    Matrix {
        /// Open the FleetView dashboard over the canned demo roster.
        #[arg(long)]
        mock: bool,
    },
    /// Open the **cockpit** — the tmux-semantics multiplexer: tabs + panes with
    /// prefix keybindings (`Ctrl+B c` new chat tab, `Ctrl+B "` shell pane, arrows
    /// to move focus, `z` zoom). This first slice renders the tab/pane layout and
    /// responds to the keys; live per-pane drivers + the ambient shell PTY land
    /// in the next ratchet. Companion chat panes are clamped by `authority`.
    Cockpit {
        /// Optional working path the cockpit session runs against.
        path: Option<PathBuf>,
    },
    /// Open a live **scrybe** Markdown session: gila is the MCP *client*, a
    /// scrybe MCP server (local or an agent-mesh peer) owns the document; the
    /// agent opens/updates it and the human edits it back, edits flowing both
    /// ways. Shape #1 (MCP peer) of `docs/design/scrybe-markdown-surface.md`.
    /// Phase 1 accepts the connection + doc and prints the config; the live MCP
    /// loop lands in later phases.
    Scrybe {
        /// URI of the scrybe MCP server to connect to.
        #[arg(long)]
        uri: String,
        /// Path of the Markdown document to open/update. Defaults to
        /// `./scrybe.md`.
        #[arg(long)]
        doc_path: Option<PathBuf>,
    },
}

/// `gila capabilities` subcommands.
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum CapabilitiesCmd {
    /// List installed capabilities (the `gilamonster.capabilities` entry points).
    List,
    /// Connect to a capability's MCP server and exercise its tools (the
    /// end-to-end check that it works through gila).
    Check {
        /// Capability name, e.g. `mogul`.
        name: String,
    },
    /// Print the `[[mcp_servers]]` snippet that wires a capability into sessions.
    Enable {
        /// Capability name, e.g. `mogul`.
        name: String,
    },
    /// Invoke one capability tool through the `gilacap` multiplexer, e.g.
    /// `gila capabilities run confluence blog --args '{"source_file":"p.md"}'`.
    Run {
        /// Capability name, e.g. `confluence`.
        name: String,
        /// Tool name, e.g. `blog`.
        tool: String,
        /// JSON object of arguments for the tool (forwarded as `--args`).
        #[arg(long)]
        args: Option<String>,
    },
    /// Interactively choose which capabilities load as agent tools (and which run
    /// confined), writing `~/.gila/capabilities.toml`. The opt-in selector.
    Config,
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
        "gilamonster matrix — the multi-agent extension layer.\n\n\
         FleetView, the live crew-monitor dashboard, is landing here (Phase 1).\n\
         Preview the dashboard now:  gila matrix --mock\n\n\
         It composes newt airframes over the agent-mesh airspace, under one\n\
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

/// Resolve the typescript `gila follow` will tail.
///
/// Threads the optional positional `logpath` and `--dir` into
/// [`follow::locate_typescript`]: an explicit `logpath` wins (it need not exist
/// yet — the tail waits for `script -F` to create it); otherwise the newest file
/// in `dir` (defaulting to the current directory) is chosen. Pure resolution so
/// the binary's `Follow` arm only has to act on the result.
pub fn follow_target(logpath: &Option<PathBuf>, dir: &Option<PathBuf>) -> Option<PathBuf> {
    let cwd = PathBuf::from(".");
    let search_dir = dir.as_deref().unwrap_or(&cwd);
    follow::locate_typescript(logpath.as_deref(), search_dir)
}

/// The text `gila follow` prints when it cannot locate a typescript to tail —
/// the only side-effect-free arm of the command (the tailing loop itself is the
/// binary's live, side-effecting work). Pure so the "no typescript found"
/// message is unit-tested. The message states the read-only contract so the
/// human knows the agent is a passenger, never a pilot.
pub fn follow_no_target_report(dir: &Option<PathBuf>) -> String {
    let cwd = PathBuf::from(".");
    let search_dir = dir.as_deref().unwrap_or(&cwd);
    format!(
        "gila follow (read-only): no typescript found in {}.\n\
         Start one with `script -F <file>` in another pane, then re-run\n\
         `gila follow <file>` (or `gila follow --dir <dir>`). The agent only\n\
         observes — it never drives your shell.\n",
        search_dir.display()
    )
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
        assert_eq!(cli.effective_command(), Command::Matrix { mock: false });
    }

    #[test]
    fn matrix_mock_flag_parses() {
        let cli = Cli::parse_from(["gila", "matrix", "--mock"]);
        assert_eq!(cli.effective_command(), Command::Matrix { mock: true });
    }

    #[test]
    fn matrix_report_points_at_the_mock_preview() {
        let report = matrix_report::<DummyErr>(Err(DummyErr));
        assert!(report.contains("gila matrix --mock"));
        assert!(report.contains("FleetView"));
    }

    #[test]
    fn capabilities_config_parses_via_cap_alias() {
        let cli = Cli::parse_from(["gila", "cap", "config"]);
        assert_eq!(
            cli.effective_command(),
            Command::Capabilities {
                cmd: CapabilitiesCmd::Config,
            }
        );
    }

    #[test]
    fn capabilities_run_subcommand_parses_with_args() {
        let cli = Cli::parse_from([
            "gila",
            "capabilities",
            "run",
            "confluence",
            "blog",
            "--args",
            "{\"space\":\"~me\"}",
        ]);
        assert_eq!(
            cli.effective_command(),
            Command::Capabilities {
                cmd: CapabilitiesCmd::Run {
                    name: "confluence".to_string(),
                    tool: "blog".to_string(),
                    args: Some("{\"space\":\"~me\"}".to_string()),
                },
            }
        );
    }

    #[test]
    fn scrybe_subcommand_parses() {
        let cli = Cli::parse_from([
            "gila",
            "scrybe",
            "--uri",
            "http://localhost:3001",
            "--doc-path",
            "/tmp/notes.md",
        ]);
        assert_eq!(
            cli.effective_command(),
            Command::Scrybe {
                uri: "http://localhost:3001".to_string(),
                doc_path: Some(PathBuf::from("/tmp/notes.md")),
            }
        );
        // doc-path is optional; uri is required.
        let cli = Cli::parse_from(["gila", "scrybe", "--uri", "http://x"]);
        assert_eq!(
            cli.effective_command(),
            Command::Scrybe {
                uri: "http://x".to_string(),
                doc_path: None,
            }
        );
        assert!(
            Cli::try_parse_from(["gila", "scrybe"]).is_err(),
            "uri required"
        );
    }

    #[test]
    fn cockpit_subcommand_parses() {
        assert_eq!(
            Cli::parse_from(["gila", "cockpit"]).effective_command(),
            Command::Cockpit { path: None }
        );
        assert_eq!(
            Cli::parse_from(["gila", "cockpit", "/tmp/p"]).effective_command(),
            Command::Cockpit {
                path: Some(PathBuf::from("/tmp/p"))
            }
        );
    }

    #[test]
    fn cowork_subcommand_bare_parses() {
        let cli = Cli::parse_from(["gila", "cowork"]);
        assert_eq!(cli.effective_command(), Command::Cowork { path: None });
    }

    #[test]
    fn cowork_subcommand_with_path_parses() {
        let cli = Cli::parse_from(["gila", "cowork", "/tmp/project"]);
        assert_eq!(
            cli.effective_command(),
            Command::Cowork {
                path: Some(PathBuf::from("/tmp/project")),
            }
        );
    }

    #[test]
    fn hotseat_subcommand_bare_parses() {
        let cli = Cli::parse_from(["gila", "hotseat"]);
        assert_eq!(
            cli.effective_command(),
            Command::Hotseat {
                path: None,
                skill: None,
            }
        );
    }

    #[test]
    fn hotseat_subcommand_with_path_and_skill_parses() {
        let cli = Cli::parse_from(["gila", "hotseat", "/tmp/incident", "--skill", "my-runbook"]);
        assert_eq!(
            cli.effective_command(),
            Command::Hotseat {
                path: Some(PathBuf::from("/tmp/incident")),
                skill: Some("my-runbook".to_string()),
            }
        );
    }

    #[test]
    fn follow_subcommand_bare_parses() {
        let cli = Cli::parse_from(["gila", "follow"]);
        assert_eq!(
            cli.effective_command(),
            Command::Follow {
                logpath: None,
                dir: None,
            }
        );
    }

    #[test]
    fn follow_subcommand_with_logpath_and_dir_parses() {
        let cli = Cli::parse_from(["gila", "follow", "/tmp/ts", "--dir", "/var/scripts"]);
        assert_eq!(
            cli.effective_command(),
            Command::Follow {
                logpath: Some(PathBuf::from("/tmp/ts")),
                dir: Some(PathBuf::from("/var/scripts")),
            }
        );
    }

    #[test]
    fn follow_target_prefers_explicit_logpath() {
        let logpath = Some(PathBuf::from("/explicit/typescript"));
        let got = follow_target(&logpath, &None).unwrap();
        assert_eq!(got, PathBuf::from("/explicit/typescript"));
    }

    #[test]
    fn follow_target_finds_newest_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ts = dir.path().join("session.typescript");
        std::fs::write(&ts, b"x").unwrap();
        let got = follow_target(&None, &Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(got, ts);
    }

    #[test]
    fn follow_target_none_when_dir_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(follow_target(&None, &Some(dir.path().to_path_buf())).is_none());
    }

    #[test]
    fn follow_no_target_report_states_read_only_and_dir() {
        let report = follow_no_target_report(&Some(PathBuf::from("/var/scripts")));
        assert!(report.contains("read-only"));
        assert!(report.contains("/var/scripts"));
        assert!(report.contains("script -F"));
        assert!(report.contains("never drives your shell"));
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
        assert!(report.contains("extension layer"));
        assert!(report.contains("agent-mesh airspace"));
        // Identity line first, scaffold notice after a blank line.
        let idx_id = report.find("operator identity").unwrap();
        let idx_notice = report.find("extension layer").unwrap();
        assert!(idx_id < idx_notice);
    }

    #[test]
    fn matrix_report_err_branch_uses_fallback() {
        let report = matrix_report(Err(DummyErr));
        assert!(report.contains("HOME unset"));
        assert!(report.contains("extension layer"));
    }

    #[test]
    fn code_path_borrows_inner() {
        let owned = Some(PathBuf::from("/tmp/x"));
        assert_eq!(code_path(&owned), Some(Path::new("/tmp/x")));
        assert_eq!(code_path(&None), None);
    }
}
