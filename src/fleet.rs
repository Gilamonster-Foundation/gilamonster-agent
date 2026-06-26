//! `gila matrix` — the **FleetView** crew-monitor dashboard (Phase 1: the
//! standalone mock surface).
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
//! - [`header_lines`] / [`rail_lines`] / [`panel_lines`] — the pure
//!   `model -> ratatui::Line` builders.
//! - [`render_fleet_frame`] — the whole render path, snapshot-tested with a
//!   ratatui [`TestBackend`](ratatui::backend::TestBackend).
//!
//! Phase 1 renders the rail and panel as static [`Paragraph`]s over a mock
//! roster ([`FleetModel::mock`]); navigation ([`Focus`]-driven selection) and a
//! stateful `List`/`Table` land in Phase 2, live data sources in Phase 3+.

use std::time::Duration;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// The dim placeholder for a metric the producer cannot report — the visible
/// face of the honest-metrics principle. Never substitute a fabricated `0`.
pub const NA: &str = "—";

/// The fixed width (in columns) of the left "Phases" rail.
pub const RAIL_WIDTH: u16 = 24;

/// The footer key hint, matching the target display.
pub const FOOTER_HINT: &str = "↑↓ select · f freeze · esc back · s save";

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
        }
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
}

impl FleetModel {
    /// Build a model over a plan header and its phases, with the rail cursor on
    /// the first phase and no agent selected.
    #[must_use]
    pub fn new(plan: PlanHeader, phases: Vec<Phase>) -> Self {
        Self {
            plan,
            phases,
            sel_phase: 0,
            sel_agent: None,
        }
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
                ),
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

/// Build the panel lines: one row per agent in the selected phase, each headed
/// by the selection cursor (`❯` on the selected row, else a space) and the
/// state glyph, then the label, model, and honest metrics cell. Pure over the
/// model, so the panel is unit-testable.
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
        .enumerate()
        .map(|(i, a)| {
            let selected = model.sel_agent() == Some(i);
            let cursor = if selected { "❯" } else { " " };
            Line::from(vec![
                Span::styled(
                    cursor,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
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

/// The footer key-hint line.
#[must_use]
pub fn footer_line() -> Line<'static> {
    Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(Color::DarkGray),
    ))
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

    // Left rail: the phase list.
    let rail = Paragraph::new(Text::from(rail_lines(model))).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" Phases "),
    );
    frame.render_widget(rail, layout.rail);

    // Main panel: the selected phase's agent rows.
    let panel_title = match model.selected_phase() {
        Some(p) => format!(" {} · {} agents ", p.name, p.total()),
        None => " agents ".to_string(),
    };
    let panel = Paragraph::new(Text::from(panel_lines(model))).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(panel_title),
    );
    frame.render_widget(panel, layout.panel);

    // Footer key hint.
    frame.render_widget(Paragraph::new(footer_line()), layout.footer);
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
    fn panel_lines_render_rows_with_cursor_and_glyph() {
        let m = FleetModel::mock();
        let lines = panel_lines(&m);
        assert_eq!(lines.len(), 7);
        // The selected (last) row carries the `❯` cursor; the first does not.
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let last: String = lines[6].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            first.starts_with(' '),
            "unselected row has no cursor: {first:?}"
        );
        assert!(
            last.starts_with('❯'),
            "selected row has the cursor: {last:?}"
        );
        assert!(first.contains('●'), "a running row's glyph: {first}");
        assert!(last.contains('✔'), "a done row's glyph: {last}");
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
}
