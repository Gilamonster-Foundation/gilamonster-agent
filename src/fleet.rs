//! `gila matrix` — the **FleetView** crew-monitor dashboard (Phase 2:
//! navigation + drill-in over the standalone mock surface).
//!
//! # What this is
//!
//! FleetView is the live, navigable answer to the one operator question a crew
//! raises: **"what is each agent doing right now — and is any of them stuck?"**
//! It renders a plan as a four-region full-screen dashboard:
//!
//! ```text
//!   authentik-idp-design                                            ← header
//!   4/7 agents · 1m57s  —  Design a phased plan to deploy Authentik
//!   ┌ Phases ───┬ Design · 7 agents ───────────────────────────┐
//!   │ 1 Design  │ ● design:deploy   Opus 4.8 (1M)  34.1k · 14   │  ← rail + panel
//!   │ 2 Review  │ ✔ design:secrets  Opus 4.8       31.7k · 1m37 │
//!   │ 3 Synth   │ ❯✔ design:security …                          │
//!   └───────────┴──────────────────────────────────────────────┘
//!   ↑↓ select · f freeze · esc back · s save                       ← footer
//! ```
//!
//! # Where this lives, and why `gila matrix` (not newt, not `gila fleet`)
//!
//! This **realizes the reserved `gila matrix` extension layer** (the
//! [`Command::Matrix`](crate::Command) arm) as a *standalone, alternate-screen
//! sibling subcommand* — it does **not** embed newt's chat surface. newt
//! deliberately keeps its chat a *plain scroller*
//! (`newt:docs/decisions/plain_scroller_tui.md`) with **no** alternate screen,
//! panes, or live dashboards; that doc's own tier table sends "Advanced TUI:
//! panes, live status, dashboards" to gilamonster-agent. FleetView is exactly
//! that surface, so it is built here on the **cowork scaffold gila already
//! ships** ([`crate::cowork`]: [`TerminalGuard`](crate::cowork::TerminalGuard),
//! [`setup_terminal`](crate::cowork::setup_terminal),
//! [`restore_terminal`](crate::cowork::restore_terminal)) — needing **zero**
//! newt-tui changes. The full rationale is recorded in
//! `docs/decisions/fleetview_full_screen_dashboard.md`.
//!
//! # The honest-metrics principle (load-bearing)
//!
//! Every per-agent metric is an [`Option`]. A metric the producer cannot report
//! — a remote mesh peer carries no token/tool count over the wire, a long local
//! turn exposes no sub-turn idle clock — renders as a dim [`NA`] (`—`), **never**
//! a fabricated `0`. The instrument must not smudge the lens: a metric we don't
//! have must look like one we don't have. This is why [`fmt_tokens`] /
//! [`fmt_tools`] / [`fmt_idle`] all take `Option`s and the mock's degraded rows
//! show `—`.
//!
//! # The testable units (the raw loop stays in the binary)
//!
//! TUI render/event loops resist coverage, so — exactly as [`crate::cowork`]
//! does — the logic is factored into pure units the gate exercises, leaving only
//! the raw crossterm loop (in the binary's `run_matrix`) as the by-design-
//! uncovered tty surface:
//!
//! - [`fleet_layout`] — the header / rail / panel / footer split math.
//! - [`AgentState::glyph`] / [`Locality::badge`] — the row decoration lookups.
//! - [`fmt_tokens`] / [`fmt_tools`] / [`fmt_duration`] / [`fmt_idle`] — the
//!   honest formatters (incl. the `—` degradation path).
//! - [`header_lines`] / [`rail_lines`] / [`panel_lines`] / [`detail_lines`] /
//!   [`footer_line_for`] — the pure `model -> ratatui::Line` builders.
//! - [`FleetModel::apply_key`] — the ↑↓/Enter/Esc navigation state machine,
//!   driven by synthetic [`KeyEvent`]s (no terminal).
//! - [`render_fleet_frame`] — the whole render path, snapshot-tested with a
//!   ratatui [`TestBackend`](ratatui::backend::TestBackend).
//!
//! Phase 2 adds the [`Focus`]-driven navigation: the agent panel is a stateful
//! [`List`] (cursor + viewport scroll), and Enter drills into a [`Detail`](Focus::Detail)
//! view of the selected agent's (mock) transcript. Live data sources replace the
//! mock roster ([`FleetModel::mock`]) in Phase 3+.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use newt_core::MemMessage;

use crate::cowork::transcript_to_lines;

/// The dim placeholder for a metric the producer cannot report — the visible
/// face of the honest-metrics principle. Never substitute a fabricated `0`.
pub const NA: &str = "—";

/// The fixed width (in columns) of the left "Phases" rail.
pub const RAIL_WIDTH: u16 = 24;

/// The footer key hint for the rail/panel, matching the target display.
pub const FOOTER_HINT: &str = "↑↓ select · → drill in · f freeze · esc back · s save";

/// The footer key hint while drilled into an agent's detail view.
pub const DETAIL_FOOTER_HINT: &str = "↑↓ scroll · esc back · f freeze · q quit";

/// Which pane currently has keyboard focus, and so where ↑↓ and Enter route.
/// The navigation state machine ([`FleetModel::apply_key`]) moves between these:
/// `Rail` (pick a phase) → `Panel` (pick an agent) → `Detail` (read its
/// transcript).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The left "Phases" rail has focus; ↑↓ moves the phase cursor.
    Rail,
    /// The agent panel has focus; ↑↓ moves the agent cursor, Enter drills in.
    Panel,
    /// Drilled into the selected agent; ↑↓ scrolls its transcript, Esc backs out.
    Detail,
}

/// The outcome of feeding one key to [`FleetModel::apply_key`]: keep looping, or
/// quit the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Continue the render/event loop.
    Continue,
    /// The operator asked to quit (`q` / `Ctrl-C`).
    Quit,
}

/// Where a crew member runs: a local in-process subagent, or a remote peer bound
/// over the agent-mesh (co-equal in the roster, but degraded in what it can
/// report — see the honest-metrics note).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locality {
    /// An in-process subagent on this host.
    Local,
    /// A remote peer reached over `newt-mesh` (Phase 5).
    Mesh,
}

impl Locality {
    /// A short rail/row badge (`"local"` / `"mesh"`). Only `Mesh` is surfaced in
    /// a row today — a local peer is the unremarkable default.
    #[must_use]
    pub fn badge(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Mesh => "mesh",
        }
    }
}

/// One agent's lifecycle state, as the panel renders it. The row's status glyph
/// and colour are keyed on this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    /// Queued, not yet started.
    Pending,
    /// Currently executing.
    Running,
    /// Finished successfully.
    Done,
    /// Held by an OCAP attest gate (`crew_authz` needs a `Presence`) — surfaced
    /// from the orchestration layer, never inferred here (Phase 4c/5).
    Blocked,
    /// Failed, with the reason.
    Failed(String),
}

impl AgentState {
    /// The status glyph drawn at the head of the row: `●` running, `✔` done,
    /// `◦` pending, `⏳` blocked, `✖` failed.
    #[must_use]
    pub fn glyph(&self) -> &'static str {
        match self {
            Self::Pending => "◦",
            Self::Running => "●",
            Self::Done => "✔",
            Self::Blocked => "⏳",
            Self::Failed(_) => "✖",
        }
    }

    /// The glyph colour: running green, done cyan, pending gray, blocked yellow,
    /// failed red — the at-a-glance "is any agent stuck?" signal.
    #[must_use]
    pub fn color(&self) -> Color {
        match self {
            Self::Pending => Color::DarkGray,
            Self::Running => Color::Green,
            Self::Done => Color::Cyan,
            Self::Blocked => Color::Yellow,
            Self::Failed(_) => Color::Red,
        }
    }

    /// Whether this state counts as finished, for the `done/total` roll-ups.
    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }

    /// A short status word for the drill-in header (`failed` carries its reason).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Pending => "pending".to_string(),
            Self::Running => "running".to_string(),
            Self::Done => "done".to_string(),
            Self::Blocked => "blocked".to_string(),
            Self::Failed(why) => format!("failed: {why}"),
        }
    }
}

/// Format a token total honestly: `Some(34_100)` → `"34.1k tok"`,
/// `Some(950)` → `"950 tok"`, `None` → [`NA`] (`—`). Never `0` for "unknown".
#[must_use]
pub fn fmt_tokens(tokens: Option<u64>) -> String {
    match tokens {
        None => NA.to_string(),
        Some(n) if n >= 1000 => format!("{:.1}k tok", n as f64 / 1000.0),
        Some(n) => format!("{n} tok"),
    }
}

/// Format a tool-call count honestly: `Some(14)` → `"14 tools"`, `Some(1)` →
/// `"1 tool"`, `None` → [`NA`].
#[must_use]
pub fn fmt_tools(count: Option<usize>) -> String {
    match count {
        None => NA.to_string(),
        Some(1) => "1 tool".to_string(),
        Some(n) => format!("{n} tools"),
    }
}

/// Format a duration as `"1m 37s"` (or `"48s"` under a minute) for a row's
/// elapsed/idle cell.
#[must_use]
pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (m, s) = (secs / 60, secs % 60);
    if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Format a duration compactly as `"1m57s"` (or `"48s"`) for the header's plan
/// elapsed, matching the target display's `4/7 agents · 1m57s`.
#[must_use]
pub fn fmt_duration_compact(d: Duration) -> String {
    let secs = d.as_secs();
    let (m, s) = (secs / 60, secs % 60);
    if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Format an optional idle time: `Some(59s)` → `"idle 59s"`, `None` → [`NA`].
#[must_use]
pub fn fmt_idle(idle: Option<Duration>) -> String {
    match idle {
        None => NA.to_string(),
        Some(d) => format!("idle {}", fmt_duration(d)),
    }
}

/// One row in the agent panel: a single crew member's identity, model, live
/// state, and (honestly optional) metrics.
#[derive(Debug, Clone)]
pub struct AgentRow {
    /// The agent label, `role:subtask` (e.g. `design:deploy`).
    pub label: String,
    /// The model the agent runs on (e.g. `Opus 4.8 (1M context)`).
    pub model: String,
    /// The agent's lifecycle state.
    pub state: AgentState,
    /// Total tokens spent, if known. `None` → `—` (e.g. a mesh peer pre-reply).
    pub tokens: Option<u64>,
    /// Tool calls made, if known. `None` → `—` (e.g. the local `TurnDriver`
    /// path, which exposes no tool count — Phase 4b instruments it).
    pub tool_count: Option<usize>,
    /// Wall-clock elapsed, if the agent has finished. `None` while running.
    pub elapsed: Option<Duration>,
    /// Time since the agent's last observable activity, if a per-activity
    /// timestamp exists. `None` → `—` (most paths have no sub-turn signal yet).
    pub idle: Option<Duration>,
    /// Local subagent or remote mesh peer.
    pub locality: Locality,
    /// The agent's transcript, shown in the drill-in Detail view. Phase 2 carries
    /// a canned mock; live transcripts arrive in Phase 3+. Empty → an honest
    /// "no transcript yet" state in the detail view.
    pub transcript: Vec<MemMessage>,
}

impl AgentRow {
    /// A convenience constructor for a local agent with full metrics.
    #[must_use]
    pub fn local(
        label: impl Into<String>,
        model: impl Into<String>,
        state: AgentState,
        tokens: Option<u64>,
        tool_count: Option<usize>,
        elapsed: Option<Duration>,
        idle: Option<Duration>,
    ) -> Self {
        Self {
            label: label.into(),
            model: model.into(),
            state,
            tokens,
            tool_count,
            elapsed,
            idle,
            locality: Locality::Local,
            transcript: Vec::new(),
        }
    }

    /// Attach a transcript to this row (builder style), for the drill-in view.
    #[must_use]
    pub fn with_transcript(mut self, transcript: Vec<MemMessage>) -> Self {
        self.transcript = transcript;
        self
    }

    /// The trailing metrics cell: `tokens · tools · (elapsed | idle)`, each
    /// honestly `—` when unknown, with a `[mesh]` badge for a remote peer.
    #[must_use]
    pub fn metrics_text(&self) -> String {
        let mut parts = vec![fmt_tokens(self.tokens), fmt_tools(self.tool_count)];
        // A finished agent shows its elapsed; a running one shows idle (if known).
        if let Some(e) = self.elapsed {
            parts.push(fmt_duration(e));
        } else if self.idle.is_some() {
            parts.push(fmt_idle(self.idle));
        }
        let mut text = parts.join(" · ");
        if self.locality == Locality::Mesh {
            text.push_str(&format!("  [{}]", self.locality.badge()));
        }
        text
    }
}

/// One phase of the plan (one `Subtask` of the crew) and the agent rows under
/// it. On today's sequential engine at most one row runs at a time; the rest are
/// done or pending.
#[derive(Debug, Clone)]
pub struct Phase {
    /// The 1-based phase number shown in the rail.
    pub index: usize,
    /// The phase name (e.g. `Design`).
    pub name: String,
    /// The agent rows under this phase.
    pub agents: Vec<AgentRow>,
}

impl Phase {
    /// How many of this phase's agents have finished.
    #[must_use]
    pub fn done(&self) -> usize {
        self.agents.iter().filter(|a| a.state.is_done()).count()
    }

    /// The total agent count in this phase.
    #[must_use]
    pub fn total(&self) -> usize {
        self.agents.len()
    }
}

/// The plan-level header data: the slug, its one-line description, and the
/// wall-clock elapsed since the run began.
#[derive(Debug, Clone)]
pub struct PlanHeader {
    /// The plan slug (e.g. `authentik-idp-design`).
    pub slug: String,
    /// The one-line plan description.
    pub description: String,
    /// Wall-clock elapsed since the plan started.
    pub elapsed: Duration,
}

/// The whole view-model FleetView renders: the plan header, its phases, and the
/// current rail/panel selection. Phase 1 holds the selection as plain indices;
/// the [`Focus`]-driven navigation state machine lands in Phase 2.
#[derive(Debug, Clone)]
pub struct FleetModel {
    plan: PlanHeader,
    phases: Vec<Phase>,
    /// The rail cursor: which phase the panel shows.
    sel_phase: usize,
    /// The panel cursor: which agent row carries the `❯` marker, if any.
    sel_agent: Option<usize>,
    /// Which pane has keyboard focus (the navigation state machine).
    focus: Focus,
    /// Whether the view fold is frozen (the `f` key) — a *view* freeze only; the
    /// underlying agents keep running. Phase 1/2 has no live source, so this is
    /// just the indicator + state.
    frozen: bool,
    /// Vertical scroll offset of the drill-in Detail transcript.
    detail_scroll: u16,
}

impl FleetModel {
    /// Build a model over a plan header and its phases, with the rail cursor on
    /// the first phase, no agent selected, and focus on the rail.
    #[must_use]
    pub fn new(plan: PlanHeader, phases: Vec<Phase>) -> Self {
        let mut m = Self {
            plan,
            phases,
            sel_phase: 0,
            sel_agent: None,
            focus: Focus::Rail,
            frozen: false,
            detail_scroll: 0,
        };
        m.clamp_agent_to_phase();
        m
    }

    /// The plan header.
    #[must_use]
    pub fn plan(&self) -> &PlanHeader {
        &self.plan
    }

    /// All phases.
    #[must_use]
    pub fn phases(&self) -> &[Phase] {
        &self.phases
    }

    /// The currently-selected phase index (the rail cursor).
    #[must_use]
    pub fn sel_phase(&self) -> usize {
        self.sel_phase
    }

    /// The currently-selected agent index within the selected phase, if any.
    #[must_use]
    pub fn sel_agent(&self) -> Option<usize> {
        self.sel_agent
    }

    /// The phase the panel currently shows, if the model has any phases.
    #[must_use]
    pub fn selected_phase(&self) -> Option<&Phase> {
        self.phases.get(self.sel_phase)
    }

    /// Total agents that have finished, across the whole plan (the header's
    /// cumulative `done` roll-up).
    #[must_use]
    pub fn agents_done(&self) -> usize {
        self.phases.iter().map(Phase::done).sum()
    }

    /// Total agents across the whole plan (the header's `total`).
    #[must_use]
    pub fn agents_total(&self) -> usize {
        self.phases.iter().map(Phase::total).sum()
    }

    /// Which pane has keyboard focus.
    #[must_use]
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// Whether the view is frozen (the `f` toggle).
    #[must_use]
    pub fn frozen(&self) -> bool {
        self.frozen
    }

    /// The drill-in transcript's scroll offset.
    #[must_use]
    pub fn detail_scroll(&self) -> u16 {
        self.detail_scroll
    }

    /// The currently-selected agent row, if any.
    #[must_use]
    pub fn selected_agent(&self) -> Option<&AgentRow> {
        let phase = self.selected_phase()?;
        phase.agents.get(self.sel_agent?)
    }

    /// Keep `sel_agent` valid for the selected phase: `None` when the phase is
    /// empty, otherwise clamped into range (defaulting to the first row).
    fn clamp_agent_to_phase(&mut self) {
        let n = self.selected_phase().map_or(0, |p| p.agents.len());
        self.sel_agent = if n == 0 {
            None
        } else {
            Some(self.sel_agent.unwrap_or(0).min(n - 1))
        };
    }

    /// Move the rail (phase) cursor by `delta`, clamped to the phase list. The
    /// agent cursor resets to the new phase's first row and the detail scroll
    /// resets.
    pub fn move_phase(&mut self, delta: isize) {
        let n = self.phases.len();
        if n == 0 {
            return;
        }
        let next = (self.sel_phase as isize + delta).clamp(0, n as isize - 1);
        self.sel_phase = next as usize;
        self.sel_agent = Some(0);
        self.detail_scroll = 0;
        self.clamp_agent_to_phase();
    }

    /// Move the panel (agent) cursor by `delta`, clamped to the selected phase's
    /// rows. A no-op on an empty phase.
    pub fn move_agent(&mut self, delta: isize) {
        let n = self.selected_phase().map_or(0, |p| p.agents.len());
        if n == 0 {
            self.sel_agent = None;
            return;
        }
        let cur = self.sel_agent.unwrap_or(0) as isize;
        self.sel_agent = Some((cur + delta).clamp(0, n as isize - 1) as usize);
    }

    /// Scroll the drill-in transcript by `delta` rows (clamped at the top).
    pub fn scroll_detail(&mut self, delta: i32) {
        self.detail_scroll = (self.detail_scroll as i32 + delta).max(0) as u16;
    }

    /// Toggle the view freeze (the `f` key). View-only — agents keep running.
    pub fn toggle_freeze(&mut self) {
        self.frozen = !self.frozen;
    }

    /// The navigation state machine: fold one key press into the model, returning
    /// whether the loop should continue or quit. Pure (no terminal), so the whole
    /// state machine is unit-testable with synthetic [`KeyEvent`]s — the
    /// `lean_input.rs` discipline.
    ///
    /// Global keys (any focus): `q` / `Ctrl-C` quit, `f` toggles freeze, `s` is
    /// reserved for save (Phase 6, currently inert). Otherwise keys route by
    /// focus: Rail ↑↓ moves the phase, →/Enter enters the Panel; Panel ↑↓ moves
    /// the agent, Enter drills into Detail, ←/Esc backs to the Rail; Detail ↑↓
    /// scrolls the transcript, ←/Esc backs to the Panel.
    pub fn apply_key(&mut self, key: KeyEvent) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => return Step::Quit,
            KeyCode::Char('c') if ctrl => return Step::Quit,
            KeyCode::Char('f') => self.toggle_freeze(),
            // `s` save lands in Phase 6 — reserved, inert for now.
            KeyCode::Char('s') => {}
            _ => match self.focus {
                Focus::Rail => match key.code {
                    KeyCode::Up => self.move_phase(-1),
                    KeyCode::Down => self.move_phase(1),
                    KeyCode::Right | KeyCode::Enter => self.focus = Focus::Panel,
                    _ => {}
                },
                Focus::Panel => match key.code {
                    KeyCode::Up => self.move_agent(-1),
                    KeyCode::Down => self.move_agent(1),
                    KeyCode::Enter => {
                        if self.sel_agent.is_some() {
                            self.focus = Focus::Detail;
                            self.detail_scroll = 0;
                        }
                    }
                    KeyCode::Left | KeyCode::Esc => self.focus = Focus::Rail,
                    _ => {}
                },
                Focus::Detail => match key.code {
                    KeyCode::Up => self.scroll_detail(-1),
                    KeyCode::Down => self.scroll_detail(1),
                    KeyCode::Left | KeyCode::Esc => self.focus = Focus::Panel,
                    _ => {}
                },
            },
        }
        Step::Continue
    }

    /// The canned demo roster, matching the target display: the
    /// `authentik-idp-design` plan with a seven-agent `Design` phase (four done,
    /// three running) and two not-yet-populated downstream phases, so the header
    /// reads `4/7 agents`. Used by `gila matrix --mock` and the snapshot test.
    #[must_use]
    pub fn mock() -> Self {
        let opus = "Opus 4.8 (1M context)";
        let design = Phase {
            index: 1,
            name: "Design".to_string(),
            agents: vec![
                AgentRow::local(
                    "design:deploy",
                    opus,
                    AgentState::Running,
                    Some(34_100),
                    Some(14),
                    None,
                    None,
                ),
                AgentRow::local(
                    "design:secrets",
                    opus,
                    AgentState::Done,
                    Some(31_700),
                    Some(7),
                    Some(Duration::from_secs(97)),
                    None,
                ),
                AgentRow::local(
                    "design:google-linking",
                    opus,
                    AgentState::Running,
                    Some(39_100),
                    Some(16),
                    None,
                    None,
                ),
                AgentRow::local(
                    "design:apps",
                    opus,
                    AgentState::Running,
                    Some(38_600),
                    Some(12),
                    None,
                    Some(Duration::from_secs(59)),
                ),
                AgentRow::local(
                    "design:ingress-dns",
                    opus,
                    AgentState::Done,
                    Some(39_500),
                    Some(16),
                    Some(Duration::from_secs(117)),
                    None,
                ),
                AgentRow::local(
                    "design:migration",
                    opus,
                    AgentState::Done,
                    Some(33_700),
                    Some(8),
                    Some(Duration::from_secs(112)),
                    None,
                ),
                AgentRow::local(
                    "design:security",
                    opus,
                    AgentState::Done,
                    Some(27_500),
                    Some(4),
                    Some(Duration::from_secs(101)),
                    None,
                )
                .with_transcript(vec![
                    MemMessage::system(
                        "You are design:security — review the Authentik deployment for \
                         OCAP / secret-handling risks.",
                    ),
                    MemMessage::user(
                        "Audit the phased plan's handling of the Google OAuth client secret \
                         and the Authentik bootstrap token.",
                    ),
                    MemMessage::assistant(
                        "Findings:\n\
                         1. The OAuth client secret must land in the target (Vault), never \
                         the git tree.\n\
                         2. The bootstrap token should be a one-time, Presence-gated step-up \
                         — not a static env var.\n\
                         3. The account-linking flow must enforce the email-verified claim \
                         before merge, to avoid account takeover.",
                    ),
                ]),
            ],
        };
        let review = Phase {
            index: 2,
            name: "Review".to_string(),
            agents: vec![],
        };
        let synthesize = Phase {
            index: 3,
            name: "Synthesize".to_string(),
            agents: vec![],
        };
        let plan = PlanHeader {
            slug: "authentik-idp-design".to_string(),
            description:
                "Design a phased plan to deploy Authentik as the homelab IdP with Google social \
                 login + account linking, wiring chat/mattermost first"
                    .to_string(),
            elapsed: Duration::from_secs(117),
        };
        Self {
            plan,
            phases: vec![design, review, synthesize],
            sel_phase: 0,
            // The target display selects the last Design row (design:security).
            sel_agent: Some(6),
            // Focus starts on the panel so the agent cursor is live (the target
            // display shows a selected agent row).
            focus: Focus::Panel,
            frozen: false,
            detail_scroll: 0,
        }
    }
}

/// The four regions a FleetView frame splits into, in screen order. Returned by
/// [`fleet_layout`] so the split math is unit-testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetLayout {
    /// Two-row plan header at the top.
    pub header: Rect,
    /// Left "Phases" rail (fixed [`RAIL_WIDTH`]).
    pub rail: Rect,
    /// Main agent panel (the rest of the body width).
    pub panel: Rect,
    /// One-row footer key hint at the very bottom.
    pub footer: Rect,
}

/// Split a full-screen [`Rect`] into the FleetView layout: a two-row header, a
/// one-row footer, and a body split into the fixed-width rail and the remaining
/// panel.
///
/// Pure geometry over the input rect — no terminal needed — so the regions are
/// unit-testable. Degenerate areas collapse gracefully (header/footer claim
/// height only when present; the rail never exceeds the body width), so the
/// function never panics or produces out-of-bounds rects. Mirrors the discipline
/// of [`crate::cowork::split_panes`].
#[must_use]
pub fn fleet_layout(area: Rect) -> FleetLayout {
    // Header claims up to two rows; footer claims one — each only if there is
    // height to spare, header first.
    let header_h = area.height.min(2);
    let footer_h = area.height.saturating_sub(header_h).min(1);
    let body_h = area
        .height
        .saturating_sub(header_h)
        .saturating_sub(footer_h);

    // The rail is fixed-width but never wider than the body.
    let rail_w = RAIL_WIDTH.min(area.width);
    let panel_w = area.width.saturating_sub(rail_w);

    let body_y = area.y.saturating_add(header_h);

    let header = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: header_h,
    };
    let rail = Rect {
        x: area.x,
        y: body_y,
        width: rail_w,
        height: body_h,
    };
    let panel = Rect {
        x: area.x.saturating_add(rail_w),
        y: body_y,
        width: panel_w,
        height: body_h,
    };
    let footer = Rect {
        x: area.x,
        y: body_y.saturating_add(body_h),
        width: area.width,
        height: footer_h,
    };
    FleetLayout {
        header,
        rail,
        panel,
        footer,
    }
}

/// Build the two header lines for a `width`-column header region.
///
/// Line 0 is the bold plan slug. Line 1 puts the cumulative `N/M agents ·
/// elapsed` progress on the left and the plan description filling the rest, so
/// the progress is always visible even when the description is long. Pure over
/// `(model, width)`, so the header is unit-testable.
#[must_use]
pub fn header_lines(model: &FleetModel, width: usize) -> Vec<Line<'static>> {
    let plan = model.plan();
    let slug = Line::from(Span::styled(
        plan.slug.clone(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    let progress = format!(
        "{}/{} agents · {}",
        model.agents_done(),
        model.agents_total(),
        fmt_duration_compact(plan.elapsed),
    );
    // Truncate the description to whatever space is left after the progress so
    // the line never overflows the header width.
    let sep = "  —  ";
    let used = progress.chars().count() + sep.chars().count();
    let desc: String = if width > used {
        plan.description.chars().take(width - used).collect()
    } else {
        String::new()
    };
    let info = Line::from(vec![
        Span::styled(progress, Style::default().fg(Color::Green)),
        Span::styled(sep, Style::default().fg(Color::DarkGray)),
        Span::styled(desc, Style::default().fg(Color::Gray)),
    ]);

    vec![slug, info]
}

/// Build the rail lines: one entry per phase (`1 Design 4/7`), the selected
/// phase highlighted bold cyan. Pure over the model, so the rail is
/// unit-testable.
#[must_use]
pub fn rail_lines(model: &FleetModel) -> Vec<Line<'static>> {
    model
        .phases()
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let text = format!("{} {} {}/{}", p.index, p.name, p.done(), p.total());
            let style = if i == model.sel_phase() {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(Span::styled(text, style))
        })
        .collect()
}

/// Build the panel rows: one per agent in the selected phase — the state glyph,
/// label, model, and honest metrics cell. The selection cursor (`❯`) is **not**
/// here: it is the [`List`]'s `highlight_symbol`, driven by `sel_agent`, so the
/// cursor and viewport scrolling come from the widget. Pure over the model, so
/// the row content is unit-testable.
#[must_use]
pub fn panel_lines(model: &FleetModel) -> Vec<Line<'static>> {
    let Some(phase) = model.selected_phase() else {
        return vec![Line::from(Span::styled(
            "no phase selected",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    if phase.agents.is_empty() {
        return vec![Line::from(Span::styled(
            format!("{} — no agents yet", phase.name),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ))];
    }
    phase
        .agents
        .iter()
        .map(|a| {
            Line::from(vec![
                Span::styled(
                    a.state.glyph(),
                    Style::default()
                        .fg(a.state.color())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    a.label.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(a.model.clone(), Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::styled(a.metrics_text(), Style::default().fg(Color::Gray)),
            ])
        })
        .collect()
}

/// Build the drill-in Detail lines for the selected agent: a one-line header
/// (glyph · label · model · status) then a blank line, then the transcript
/// rendered through [`transcript_to_lines`] (the same newt-data → ratatui bridge
/// `gila cowork` uses). An agent with no transcript yet shows an honest
/// empty-state. Pure over `(model, width)`, so the detail body is unit-testable.
#[must_use]
pub fn detail_lines(model: &FleetModel, width: usize) -> Vec<Line<'static>> {
    let Some(agent) = model.selected_agent() else {
        return vec![Line::from(Span::styled(
            "no agent selected",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                agent.state.glyph(),
                Style::default()
                    .fg(agent.state.color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                agent.label.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(agent.model.clone(), Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(
                agent.state.label(),
                Style::default().fg(agent.state.color()),
            ),
        ]),
        Line::from(""),
    ];
    if agent.transcript.is_empty() {
        lines.push(Line::from(Span::styled(
            "— no transcript yet (live drill-in lands in Phase 3) —",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    } else {
        lines.extend(transcript_to_lines(&agent.transcript, width));
    }
    lines
}

/// The focus-dependent footer hint, with a frozen indicator appended when the
/// view is frozen. Pure over the model.
#[must_use]
pub fn footer_line_for(model: &FleetModel) -> Line<'static> {
    let hint = if model.focus() == Focus::Detail {
        DETAIL_FOOTER_HINT
    } else {
        FOOTER_HINT
    };
    let mut spans = vec![Span::styled(hint, Style::default().fg(Color::DarkGray))];
    if model.frozen() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "❄ FROZEN",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

/// A pane's border style: bold cyan when the pane holds focus, dim gray
/// otherwise — the on-screen focus indicator (mirrors `cowork::border_style`).
#[must_use]
fn pane_border(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Draw one full FleetView frame into `frame` from the model's current state.
///
/// This is the **whole render path**, lifted out of the binary's event loop so
/// it is testable with ratatui's [`TestBackend`](ratatui::backend::TestBackend):
/// it splits the area ([`fleet_layout`]), draws the two-row header, the bordered
/// "Phases" rail, the bordered agent panel (titled with the selected phase), and
/// the footer hint. Phase 1 renders the rail and panel as [`Paragraph`]s; the
/// stateful `List`/`Table` and live selection land in Phase 2. The binary's loop
/// calls `terminal.draw(|f| render_fleet_frame(&model, f))` and owns only the
/// crossterm setup/teardown + input polling around it.
pub fn render_fleet_frame(model: &FleetModel, frame: &mut Frame) {
    let layout = fleet_layout(frame.area());

    // Header (two rows): slug + cumulative progress + description.
    let header = Paragraph::new(Text::from(header_lines(
        model,
        layout.header.width as usize,
    )));
    frame.render_widget(header, layout.header);

    // Left rail: the phase list (border highlighted when the rail has focus).
    let rail = Paragraph::new(Text::from(rail_lines(model))).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(pane_border(model.focus() == Focus::Rail))
            .title(" Phases "),
    );
    frame.render_widget(rail, layout.rail);

    // Main area: the agent panel, or — when drilled in — the Detail view.
    if model.focus() == Focus::Detail {
        let inner_w = layout.panel.width.saturating_sub(2).max(1) as usize;
        let title = match model.selected_agent() {
            Some(a) => format!(" {} ", a.label),
            None => " detail ".to_string(),
        };
        let detail = Paragraph::new(Text::from(detail_lines(model, inner_w)))
            .scroll((model.detail_scroll(), 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(pane_border(true))
                    .title(title),
            );
        frame.render_widget(detail, layout.panel);
    } else {
        let panel_title = match model.selected_phase() {
            Some(p) => format!(" {} · {} agents ", p.name, p.total()),
            None => " agents ".to_string(),
        };
        let focused = model.focus() == Focus::Panel;
        let items: Vec<ListItem> = panel_lines(model).into_iter().map(ListItem::new).collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(pane_border(focused))
                    .title(panel_title),
            )
            .highlight_symbol("❯ ")
            .highlight_style(if focused {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            });
        // Drive selection + viewport scroll from the model — but only when the
        // phase actually has agents (an empty phase shows its one info row with
        // no highlight).
        let mut state = ListState::default();
        if model.selected_phase().is_some_and(|p| !p.agents.is_empty()) {
            state.select(model.sel_agent());
        }
        frame.render_stateful_widget(list, layout.panel, &mut state);
    }

    // Footer: focus-dependent key hint + the frozen indicator.
    frame.render_widget(Paragraph::new(footer_line_for(model)), layout.footer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // --- honest formatters --------------------------------------------------

    #[test]
    fn fmt_tokens_is_honest_about_unknown() {
        assert_eq!(fmt_tokens(None), NA);
        assert_eq!(fmt_tokens(Some(34_100)), "34.1k tok");
        assert_eq!(fmt_tokens(Some(950)), "950 tok");
        assert_eq!(fmt_tokens(Some(1_000)), "1.0k tok");
    }

    #[test]
    fn fmt_tools_pluralizes_and_blanks_unknown() {
        assert_eq!(fmt_tools(None), NA);
        assert_eq!(fmt_tools(Some(1)), "1 tool");
        assert_eq!(fmt_tools(Some(14)), "14 tools");
        assert_eq!(fmt_tools(Some(0)), "0 tools");
    }

    #[test]
    fn fmt_duration_and_compact_render_minutes() {
        assert_eq!(fmt_duration(Duration::from_secs(97)), "1m 37s");
        assert_eq!(fmt_duration(Duration::from_secs(48)), "48s");
        assert_eq!(fmt_duration_compact(Duration::from_secs(117)), "1m57s");
        assert_eq!(fmt_duration_compact(Duration::from_secs(9)), "9s");
    }

    #[test]
    fn fmt_idle_blanks_unknown() {
        assert_eq!(fmt_idle(None), NA);
        assert_eq!(fmt_idle(Some(Duration::from_secs(59))), "idle 59s");
    }

    // --- state glyphs / colours (constructs every variant: no dead code) ----

    #[test]
    fn every_state_has_a_distinct_glyph_and_colour() {
        let states = [
            AgentState::Pending,
            AgentState::Running,
            AgentState::Done,
            AgentState::Blocked,
            AgentState::Failed("boom".into()),
        ];
        let glyphs: Vec<&str> = states.iter().map(AgentState::glyph).collect();
        // All five glyphs are distinct.
        for (i, g) in glyphs.iter().enumerate() {
            assert!(
                glyphs.iter().skip(i + 1).all(|o| o != g),
                "glyph {g} repeats"
            );
        }
        assert_eq!(AgentState::Running.glyph(), "●");
        assert_eq!(AgentState::Done.glyph(), "✔");
        assert_eq!(AgentState::Running.color(), Color::Green);
        assert_eq!(AgentState::Failed("x".into()).color(), Color::Red);
        // Only Done counts as finished.
        assert!(AgentState::Done.is_done());
        assert!(!AgentState::Running.is_done());
        assert!(!AgentState::Blocked.is_done());
    }

    #[test]
    fn locality_badges() {
        assert_eq!(Locality::Local.badge(), "local");
        assert_eq!(Locality::Mesh.badge(), "mesh");
    }

    // --- the honest-metrics path: a degraded mesh row shows `—` -------------

    #[test]
    fn mesh_row_blanks_metrics_it_cannot_have() {
        // A remote peer pre-reply: no tokens, no tool count, no idle clock.
        let row = AgentRow {
            label: "review:security".into(),
            model: "Qwen3-Coder-Next".into(),
            state: AgentState::Running,
            tokens: None,
            tool_count: None,
            elapsed: None,
            idle: None,
            locality: Locality::Mesh,
            transcript: Vec::new(),
        };
        let text = row.metrics_text();
        // Two dim dashes (tokens, tools) and the mesh badge — never a fake 0.
        assert!(text.contains(NA), "unknown metrics show `—`: {text}");
        assert!(!text.contains('0'), "no fabricated zero: {text}");
        assert!(text.contains("[mesh]"), "remote peer is badged: {text}");
    }

    #[test]
    fn local_done_row_shows_elapsed_running_row_shows_idle() {
        let done = AgentRow::local(
            "design:secrets",
            "Opus 4.8 (1M context)",
            AgentState::Done,
            Some(31_700),
            Some(7),
            Some(Duration::from_secs(97)),
            None,
        );
        let m = done.metrics_text();
        assert!(m.contains("31.7k tok"));
        assert!(m.contains("7 tools"));
        assert!(m.contains("1m 37s"));
        assert!(!m.contains("[mesh]"));

        let running = AgentRow::local(
            "design:apps",
            "Opus 4.8 (1M context)",
            AgentState::Running,
            Some(38_600),
            Some(12),
            None,
            Some(Duration::from_secs(59)),
        );
        assert!(running.metrics_text().contains("idle 59s"));
    }

    // --- fleet_layout geometry (mirrors cowork::split_panes discipline) ------

    #[test]
    fn fleet_layout_tiles_header_body_footer_full_width() {
        let l = fleet_layout(Rect::new(0, 0, 100, 30));
        // Full width on header and footer.
        assert_eq!(l.header.width, 100);
        assert_eq!(l.footer.width, 100);
        // Header two rows at the top; footer one row at the bottom.
        assert_eq!(l.header.height, 2);
        assert_eq!(l.header.y, 0);
        assert_eq!(l.footer.height, 1);
        assert_eq!(l.footer.y, 29);
        // Rail is the fixed width; panel takes the rest; together they span it.
        assert_eq!(l.rail.width, RAIL_WIDTH);
        assert_eq!(l.panel.width, 100 - RAIL_WIDTH);
        assert_eq!(l.rail.x, 0);
        assert_eq!(l.panel.x, RAIL_WIDTH);
        // Body sits between header and footer, same height for rail and panel.
        assert_eq!(l.rail.y, 2);
        assert_eq!(l.panel.y, 2);
        assert_eq!(l.rail.height, 27);
        assert_eq!(l.panel.height, 27);
        // The vertical regions tile the whole area exactly.
        assert_eq!(l.header.height + l.rail.height + l.footer.height, 30);
    }

    #[test]
    fn fleet_layout_degenerate_sizes_dont_panic_or_overflow() {
        for h in 0u16..=4 {
            for w in 0u16..=30 {
                let l = fleet_layout(Rect::new(0, 0, w, h));
                // Header + body + footer tile the height exactly.
                assert_eq!(l.header.height + l.rail.height + l.footer.height, h);
                // Rail + panel tile the width exactly.
                assert_eq!(l.rail.width + l.panel.width, w);
                // Nothing escapes the area.
                assert!(l.footer.y + l.footer.height <= h);
                assert!(l.panel.x + l.panel.width <= w);
            }
        }
    }

    #[test]
    fn fleet_layout_respects_a_nonzero_origin() {
        let l = fleet_layout(Rect::new(5, 7, 50, 20));
        assert_eq!(l.header.x, 5);
        assert_eq!(l.header.y, 7);
        assert_eq!(l.rail.x, 5);
        assert_eq!(l.rail.y, 9);
        assert_eq!(l.footer.y, 7 + 20 - 1);
    }

    // --- model roll-ups ------------------------------------------------------

    #[test]
    fn mock_roster_is_four_of_seven_in_one_design_phase() {
        let m = FleetModel::mock();
        assert_eq!(m.agents_total(), 7, "only Design is populated in the mock");
        assert_eq!(m.agents_done(), 4, "four Design agents are done");
        assert_eq!(m.phases().len(), 3);
        let design = m.selected_phase().expect("a selected phase");
        assert_eq!(design.name, "Design");
        assert_eq!(design.done(), 4);
        assert_eq!(design.total(), 7);
        assert_eq!(m.sel_agent(), Some(6), "the last Design row is selected");
    }

    #[test]
    fn new_model_starts_on_the_first_phase_unselected() {
        let m = FleetModel::new(
            PlanHeader {
                slug: "p".into(),
                description: "d".into(),
                elapsed: Duration::from_secs(0),
            },
            vec![Phase {
                index: 1,
                name: "Only".into(),
                agents: vec![],
            }],
        );
        assert_eq!(m.sel_phase(), 0);
        assert_eq!(m.sel_agent(), None);
    }

    // --- pure line builders --------------------------------------------------

    #[test]
    fn header_lines_carry_slug_progress_and_description() {
        let m = FleetModel::mock();
        let lines = header_lines(&m, 120);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("authentik-idp-design"), "slug: {text}");
        assert!(text.contains("4/7 agents · 1m57s"), "progress: {text}");
        assert!(text.contains("Authentik"), "description: {text}");
    }

    #[test]
    fn header_description_is_truncated_to_width() {
        let m = FleetModel::mock();
        // A narrow header must not overflow: each line's content fits the width.
        let width = 40usize;
        for line in header_lines(&m, width) {
            let len: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(len <= width, "line of {len} chars overflows width {width}");
        }
    }

    #[test]
    fn rail_lines_list_phases_with_counts() {
        let m = FleetModel::mock();
        let lines = rail_lines(&m);
        assert_eq!(lines.len(), 3);
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(first, "1 Design 4/7");
        // The selected (first) phase is bold cyan; the others are not.
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Gray));
    }

    #[test]
    fn panel_lines_render_rows_with_glyph_and_metrics() {
        let m = FleetModel::mock();
        let lines = panel_lines(&m);
        assert_eq!(lines.len(), 7);
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let last: String = lines[6].spans.iter().map(|s| s.content.as_ref()).collect();
        // The cursor is the List's highlight_symbol now, not baked into the row —
        // each row leads with its state glyph.
        assert!(
            !first.contains('❯'),
            "no manual cursor in the row: {first:?}"
        );
        assert!(
            first.starts_with('●'),
            "a running row leads with its glyph: {first:?}"
        );
        assert!(
            last.starts_with('✔'),
            "a done row leads with its glyph: {last:?}"
        );
        assert!(first.contains("design:deploy"));
        assert!(first.contains("Opus 4.8 (1M context)"));
        assert!(first.contains("34.1k tok"));
        assert!(first.contains("14 tools"));
    }

    #[test]
    fn panel_lines_show_empty_state_for_an_unpopulated_phase() {
        let mut m = FleetModel::mock();
        m.sel_phase = 1; // Review — no agents yet
        let lines = panel_lines(&m);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("Review"), "names the empty phase: {text}");
        assert!(text.contains("no agents yet"), "honest empty state: {text}");
    }

    // --- full-frame render via ratatui TestBackend --------------------------

    /// THE snapshot test: render a full FleetView frame to a `TestBackend` and
    /// assert the header progress, the slug, the rail phases, an agent row with
    /// its model + honest metrics, the selection cursor, and the footer hint are
    /// all present — exercising the whole render path
    /// (`render_fleet_frame` → `fleet_layout` → the line builders) without a tty.
    #[test]
    fn render_fleet_frame_draws_the_dashboard() {
        let m = FleetModel::mock();
        let mut terminal = Terminal::new(TestBackend::new(110, 18)).unwrap();
        terminal.draw(|f| render_fleet_frame(&m, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let rendered: String = buf.content().iter().map(|c| c.symbol()).collect();

        // Header: slug + cumulative progress.
        assert!(rendered.contains("authentik-idp-design"), "slug drawn");
        assert!(rendered.contains("4/7 agents · 1m57s"), "progress drawn");
        // Rail: the phase list with counts, and the box title.
        assert!(rendered.contains("Phases"), "rail title");
        assert!(rendered.contains("1 Design 4/7"), "rail phase entry");
        assert!(rendered.contains("2 Review"), "rail second phase");
        // Panel: the selected phase title + an agent row with model + metrics.
        assert!(rendered.contains("Design · 7 agents"), "panel title");
        assert!(rendered.contains("design:deploy"), "an agent label");
        assert!(rendered.contains("Opus 4.8 (1M context)"), "the model");
        assert!(rendered.contains("34.1k tok"), "honest token count");
        assert!(rendered.contains("14 tools"), "tool count");
        // The selection cursor on the last Design row.
        assert!(rendered.contains("❯"), "selection cursor drawn");
        // Footer hint.
        assert!(rendered.contains("↑↓ select"), "footer hint");
        assert!(rendered.contains("freeze"), "footer freeze key");
    }

    #[test]
    fn render_fleet_frame_survives_a_tiny_terminal() {
        // The render path must not panic on a degenerate size (no room for the
        // body) — the layout collapses and the widgets clip.
        let m = FleetModel::mock();
        let mut terminal = Terminal::new(TestBackend::new(8, 2)).unwrap();
        terminal
            .draw(|f| render_fleet_frame(&m, f))
            .expect("render must not panic on a tiny area");
    }

    // --- Phase 2: navigation state machine ----------------------------------

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn agent_state_label_words() {
        assert_eq!(AgentState::Pending.label(), "pending");
        assert_eq!(AgentState::Running.label(), "running");
        assert_eq!(AgentState::Done.label(), "done");
        assert_eq!(AgentState::Blocked.label(), "blocked");
        assert_eq!(AgentState::Failed("boom".into()).label(), "failed: boom");
    }

    #[test]
    fn apply_key_walks_rail_panel_detail() {
        let mut m = FleetModel::mock();
        // The mock starts on the Panel with the last Design row selected.
        assert_eq!(m.focus(), Focus::Panel);
        assert_eq!(m.sel_agent(), Some(6));

        // Esc backs out to the Rail.
        assert_eq!(m.apply_key(key(KeyCode::Esc)), Step::Continue);
        assert_eq!(m.focus(), Focus::Rail);

        // Down moves to the (empty) Review phase; the agent cursor goes None.
        m.apply_key(key(KeyCode::Down));
        assert_eq!(m.sel_phase(), 1);
        assert_eq!(m.sel_agent(), None, "an empty phase has no agent cursor");

        // Up back to Design; the agent cursor resets to the first row.
        m.apply_key(key(KeyCode::Up));
        assert_eq!(m.sel_phase(), 0);
        assert_eq!(m.sel_agent(), Some(0));

        // Enter/Right moves focus into the Panel; Down moves the agent cursor.
        m.apply_key(key(KeyCode::Enter));
        assert_eq!(m.focus(), Focus::Panel);
        m.apply_key(key(KeyCode::Down));
        assert_eq!(m.sel_agent(), Some(1));

        // Enter drills into Detail; Down scrolls; Up clamps at the top.
        m.apply_key(key(KeyCode::Enter));
        assert_eq!(m.focus(), Focus::Detail);
        m.apply_key(key(KeyCode::Down));
        assert_eq!(m.detail_scroll(), 1);
        m.apply_key(key(KeyCode::Up));
        m.apply_key(key(KeyCode::Up));
        assert_eq!(m.detail_scroll(), 0, "scroll clamps at the top");

        // Esc backs out of Detail to the Panel.
        m.apply_key(key(KeyCode::Esc));
        assert_eq!(m.focus(), Focus::Panel);
    }

    #[test]
    fn move_phase_and_agent_clamp_at_ends() {
        let mut m = FleetModel::mock();
        m.move_phase(-5);
        assert_eq!(m.sel_phase(), 0);
        m.move_phase(99);
        assert_eq!(m.sel_phase(), 2, "3 phases → max index 2");

        m.move_phase(-99); // back to Design (7 agents)
        m.move_agent(-99);
        assert_eq!(m.sel_agent(), Some(0));
        m.move_agent(99);
        assert_eq!(m.sel_agent(), Some(6));
    }

    #[test]
    fn apply_key_quits_on_q_and_ctrl_c_only() {
        let mut m = FleetModel::mock();
        assert_eq!(m.apply_key(key(KeyCode::Char('q'))), Step::Quit);
        assert_eq!(
            m.apply_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Step::Quit
        );
        // A plain 'c' is not a quit.
        assert_eq!(m.apply_key(key(KeyCode::Char('c'))), Step::Continue);
    }

    #[test]
    fn apply_key_freeze_toggles_from_any_focus() {
        let mut m = FleetModel::mock();
        assert!(!m.frozen());
        m.apply_key(key(KeyCode::Char('f')));
        assert!(m.frozen());
        m.apply_key(key(KeyCode::Char('f')));
        assert!(!m.frozen());
    }

    // --- Phase 2: drill-in detail view --------------------------------------

    #[test]
    fn detail_lines_show_the_selected_agents_transcript() {
        let m = FleetModel::mock(); // design:security (idx 6) carries a transcript
        let text: String = detail_lines(&m, 80)
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains("design:security"),
            "header names the agent: {text}"
        );
        assert!(text.contains("done"), "header shows the status");
        assert!(
            text.contains("OAuth client secret"),
            "transcript body rendered: {text}"
        );
    }

    #[test]
    fn detail_lines_honest_empty_state_without_a_transcript() {
        let mut m = FleetModel::mock();
        m.move_agent(-99); // design:deploy (idx 0) has no transcript
        let text: String = detail_lines(&m, 80)
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("design:deploy"));
        assert!(
            text.contains("no transcript yet"),
            "honest empty state: {text}"
        );
    }

    // --- Phase 2: focus-aware rendering -------------------------------------

    #[test]
    fn footer_is_focus_aware_and_shows_frozen() {
        let mut m = FleetModel::mock();
        let panel: String = footer_line_for(&m)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(panel.contains("drill in"), "panel footer: {panel}");
        assert!(!panel.contains("FROZEN"));

        m.toggle_freeze();
        let frozen: String = footer_line_for(&m)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(frozen.contains("FROZEN"), "frozen indicator: {frozen}");

        m.apply_key(key(KeyCode::Enter)); // drill into Detail
        let detail: String = footer_line_for(&m)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(detail.contains("scroll"), "detail footer: {detail}");
    }

    #[test]
    fn render_detail_view_draws_the_transcript() {
        let mut m = FleetModel::mock();
        m.apply_key(key(KeyCode::Enter)); // Panel → Detail on design:security
        assert_eq!(m.focus(), Focus::Detail);

        let mut terminal = Terminal::new(TestBackend::new(110, 18)).unwrap();
        terminal.draw(|f| render_fleet_frame(&m, f)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("design:security"), "detail box title");
        assert!(rendered.contains("Findings"), "transcript content drawn");
        assert!(rendered.contains("↑↓ scroll"), "detail footer hint");
        // The rail is still drawn alongside the detail view.
        assert!(rendered.contains("Phases"));
    }

    #[test]
    fn render_panel_focus_highlights_the_panel_border() {
        // With focus on the Panel, the panel's border corner is cyan; the rail's
        // is gray. (The mock starts focused on the Panel.)
        let m = FleetModel::mock();
        let mut terminal = Terminal::new(TestBackend::new(110, 18)).unwrap();
        terminal.draw(|f| render_fleet_frame(&m, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let layout = fleet_layout(ratatui::layout::Rect::new(0, 0, 110, 18));
        let panel_corner = &buf[(layout.panel.x, layout.panel.y)];
        assert_eq!(
            panel_corner.style().fg,
            Some(Color::Cyan),
            "focused panel border"
        );
        let rail_corner = &buf[(layout.rail.x, layout.rail.y)];
        assert_eq!(
            rail_corner.style().fg,
            Some(Color::DarkGray),
            "unfocused rail border"
        );
    }
}
