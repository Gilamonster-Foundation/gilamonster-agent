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

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

pub mod authority;
pub mod capabilities;
pub mod chain;
pub mod cockpit;
pub mod cowork;
pub mod delegate;
pub mod fleet;
pub mod follow;
pub mod gila_board;
pub mod gila_cache;
pub mod gila_checkpoint;
pub mod gila_commit_msg;
pub mod gila_completion;
pub mod gila_daily;
pub mod gila_dev;
pub mod gila_git;
pub mod gila_ideas;
pub mod gila_init;
pub mod gila_insights;
pub mod gila_log;
pub mod gila_logs;
pub mod gila_meeting;
pub mod gila_projects;
pub mod gila_prompt;
pub mod gila_status;
pub mod gila_todos;
pub mod gila_update;
pub mod gila_version;
pub mod gila_worktree;
pub mod gila_wsl;
pub mod hotseat;
pub mod keys;
pub mod layout;
pub mod manifest;
pub mod pty;
pub mod python_bridge;
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
    /// Run one question through a **LangChain** `LLMChain` (system frame +
    /// human template) against the configured newt backend — the LangChain
    /// exploration surface (see [`chain`]). Same endpoint/model/token seam as
    /// `gila follow`/`gila cowork`: everything comes from newt's config,
    /// nothing from code. Tool-capable LangChain agents are out of scope for
    /// this slice; they would come back through the `authority` seam.
    Chain {
        /// The question to send through the chain (words are joined).
        #[arg(required = true)]
        question: Vec<String>,
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
    /// Rust-native git operations (Phase 1 of the gila-parity plan). Graduates
    /// `commit` and `tend` out of the shell-delegate fallback; every other
    /// `gila git …` still delegates to Python gilabot.
    Git {
        #[command(subcommand)]
        cmd: GitCmd,
    },
    /// Print the gilamonster-agent version + toolchain (Rust-native, Phase 3).
    Version,
    /// Create or open today's daily note (`YYYY-MM-DD-daily.md`).
    Daily {
        /// Date for the note (YYYY-MM-DD). Defaults to today.
        #[arg(long)]
        date: Option<String>,
    },
    /// Capture a one-line idea, or list captured ideas.
    Ideas {
        /// The idea text to capture. Omit (with `--list`) to list ideas.
        idea: Vec<String>,
        /// List captured ideas instead of appending.
        #[arg(long)]
        list: bool,
    },
    /// Manage the markdown todo list: add, list open, or mark done.
    Todos {
        /// Todo text to add. Omit (with `--list`) to list, or `--done N`.
        text: Vec<String>,
        /// List open todos instead of adding.
        #[arg(long)]
        list: bool,
        /// Mark the nth open todo done.
        #[arg(long)]
        done: Option<usize>,
    },
    /// List active projects (git repos) under the workspace root.
    Projects,
    /// Show the board (task files under the board directory).
    Board,
    /// Manage the gilabot cache (`status` default, `clear` to empty).
    Cache {
        /// Empty the cache instead of reporting status.
        #[arg(long)]
        clear: bool,
    },
    /// View the newest gila logs (most-recent first).
    Logs {
        /// Max number of logs to show.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Manage reusable prompt templates (`list` default, `show`/`create`).
    #[command(name = "prompt")]
    Prompt {
        #[command(subcommand)]
        cmd: PromptCmd,
    },
    /// Validate a commit message against the conventional-commit shape.
    #[command(name = "commit-msg")]
    CommitMsg {
        /// The commit message to validate (or a path to a message file).
        message: String,
        /// Treat `message` as a path to a file containing the message.
        #[arg(long)]
        file: bool,
    },
    /// Emit a shell completion script for gila.
    Completion {
        /// The shell to generate for (bash, zsh).
        shell: String,
    },
    /// Initialize the gila config directory (~/.gila + standard subdirs).
    Init,
    /// Self-update gilamonster-agent (git pull --ff-only + cargo build --release).
    Update {
        /// Repo path to update. Defaults to this build's manifest dir.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Create a meeting note from a template (`YYYY-MM-DD-<slug>.md`).
    Meeting {
        /// Meeting title.
        #[arg(long)]
        title: String,
        /// Date for the note (YYYY-MM-DD). Defaults to today.
        #[arg(long)]
        date: Option<String>,
    },
    /// Scaffold a Top-5 weekly-status document.
    Top5 {
        /// Date for the doc (YYYY-MM-DD). Defaults to today.
        #[arg(long)]
        date: Option<String>,
    },
    /// Scaffold a standup note (yesterday/today/blockers).
    Standup {
        /// Date for the note (YYYY-MM-DD). Defaults to today.
        #[arg(long)]
        date: Option<String>,
    },
    /// Snapshot/inspect workspace checkpoints.
    Checkpoint {
        #[command(subcommand)]
        cmd: CheckpointCmd,
    },
    /// Git activity analytics for a repo (commits by author + day).
    Insights {
        /// Repo path. Defaults to the current directory.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Max commits to scan.
        #[arg(long, default_value_t = 500)]
        max: usize,
    },
    /// Check the dev environment (required tools on PATH).
    Dev,
    /// Report WSL (Windows Subsystem for Linux) status.
    Wsl,
    /// Session/activity logging (`activity collect`, `prompt create`).
    Log {
        #[command(subcommand)]
        cmd: LogCmd,
    },
    /// Manage git worktrees for a repo (`list`/`add`/`remove`).
    Worktree {
        #[command(subcommand)]
        cmd: WorktreeCmd,
    },
    /// Shell-delegate fallback: any subcommand gilamonster-agent has not yet
    /// ported from gilabot (Python). clap's `external_subcommand` catch-all
    /// captures the unrecognized subcommand name + its args and hands them to
    /// [`delegate`], which re-execs the real gilabot binary so every gilabot
    /// command works from day one. Commands graduate out of this fallback as
    /// they gain their own `Command` variant (see the parity plan).
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// `gila git` subcommands (the Rust-native Phase-1 slice).
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum GitCmd {
    /// Stage all changes (`git add -A`) and commit with a message (libgit2).
    Commit {
        /// Commit message.
        #[arg(short, long)]
        message: String,
        /// Repository path. Defaults to the current directory.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Run git-tend profiles across the configured repos (`git-tend.yaml`).
    Tend {
        /// Path to the config file. Defaults to `~/.gila/git-tend.yaml`.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Preview what would run without executing.
        #[arg(long)]
        dry_run: bool,
        /// Only tend repos that use this profile.
        #[arg(long)]
        profile: Option<String>,
    },
}

/// `gila prompt` subcommands.
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum PromptCmd {
    /// List available prompt templates.
    List,
    /// Print a prompt template's body.
    Show {
        /// Template name (file stem).
        name: String,
    },
    /// Scaffold a new prompt template.
    Create {
        /// Template name (file stem).
        name: String,
    },
}

/// `gila checkpoint` subcommands.
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum CheckpointCmd {
    /// Snapshot the repos under a root into a named checkpoint.
    Create {
        /// Checkpoint name.
        name: String,
        /// Workspace root to scan for repos. Defaults to the workspace root.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// List checkpoints.
    List,
    /// Show a checkpoint's recorded snapshots.
    Show {
        /// Checkpoint name.
        name: String,
    },
}

/// `gila log` subcommands.
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum LogCmd {
    /// Collect the day's activity across workspace repos into a digest.
    Activity {
        #[command(subcommand)]
        cmd: LogActivityCmd,
    },
    /// Session-prompt logging (`gila log prompt create`).
    Prompt {
        #[command(subcommand)]
        cmd: LogPromptCmd,
    },
}

/// `gila log activity` subcommands.
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum LogActivityCmd {
    /// Collect the day's activity across workspace repos into a digest.
    Collect {
        /// Date (YYYY-MM-DD). Defaults to today.
        #[arg(long)]
        date: Option<String>,
        /// Workspace root to scan. Defaults to the workspace root.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Max commits to scan per repo.
        #[arg(long, default_value_t = 200)]
        max: usize,
    },
}

/// `gila log prompt` subcommands.
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum LogPromptCmd {
    /// Scaffold a session-log entry.
    Create {
        /// Session description (slugified into the filename).
        #[arg(short, long)]
        message: String,
        /// Session type (feature, bug, refactor, docs, test).
        #[arg(long, default_value = "feature")]
        log_type: String,
        /// Session duration (e.g. "~2 hours").
        #[arg(long, default_value = "")]
        duration: String,
        /// Date (YYYY-MM-DD). Defaults to today.
        #[arg(short, long)]
        date: Option<String>,
    },
}

/// `gila worktree` subcommands.
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum WorktreeCmd {
    /// List worktrees for a repo.
    List {
        /// Repo path. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Add a worktree (new branch `<name>` at `<repo>.worktrees/<name>`).
    Add {
        /// Worktree/branch name.
        name: String,
        /// Repo path. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Remove a worktree.
    Remove {
        /// Worktree name.
        name: String,
        /// Repo path. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
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
    fn chain_subcommand_parses() {
        assert_eq!(
            Cli::parse_from(["gila", "chain", "why", "is", "the", "sky", "blue"])
                .effective_command(),
            Command::Chain {
                question: ["why", "is", "the", "sky", "blue"]
                    .map(String::from)
                    .to_vec()
            }
        );
        // A question is required — bare `gila chain` is a usage error, not an
        // empty prompt on the wire.
        assert!(
            Cli::try_parse_from(["gila", "chain"]).is_err(),
            "question required"
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
