//! `gila cowork` — the **non-blocking full-screen split-pane scaffold** (Tier B,
//! part 1).
//!
//! # What this is
//!
//! Tier B is the cowork target: a tmux-style split — the **agent chat** on top,
//! the **human's live interactive shell** on the bottom. This module lands the
//! full-screen, non-blocking event-loop scaffold: the split layout, a focus
//! indicator, a focus-swap hotkey, and a *placeholder* shell pane. The
//! PTY-hosted shell that fills the bottom pane lands in the follow-up issue
//! (#10); it plugs into the very same [`ObservationChannel`](crate::follow)
//! seam `gila follow` already feeds, so #10 only has to add a PTY
//! [`ObservationSource`](crate::follow::ObservationSource) — it does not
//! restructure this app.
//!
//! ```text
//!   ┌──────────────────────────────────────────────┐
//!   │ agent chat  (TurnDriver → transcript_lines)   │  ← top pane (focusable)
//!   │ ...                                           │
//!   ├──────────────────────────────────────────────┤
//!   │ shell pane (PTY lands in #10)                 │  ← bottom pane (focusable)
//!   ├──────────────────────────────────────────────┤
//!   │ status / help line                            │  ← one row
//!   └──────────────────────────────────────────────┘
//! ```
//!
//! # Why the rich TUI lives here, not in newt
//!
//! newt deliberately keeps its chat a *plain scroller*
//! (`docs/decisions/plain_scroller_tui.md`) and exposes render **data** only:
//! [`newt_core::agentic::transcript_lines`] turns the driver's transcript into
//! width-wrapped, role-tagged [`TranscriptLine`](newt_core::agentic::TranscriptLine)s.
//! gila is the rich-TUI home, so the ratatui mapping
//! ([`transcript_to_lines`]) and the split layout
//! ([`split_panes`]) live here. We consume newt's
//! [`TurnDriver`](newt_core::agentic::TurnDriver) — we never reimplement it.
//!
//! # The three load-bearing, *testable* pieces
//!
//! TUI render/event loops resist coverage, so the logic is factored into pure
//! units the gate can exercise:
//!
//! - [`split_panes`] — the layout split math (top chat / bottom shell / status).
//! - [`Focus`] + [`CoworkApp::swap_focus`] — the focus-swap state machine.
//! - [`transcript_to_lines`] — the `TranscriptLine → ratatui::Line` mapping.
//! - [`TerminalGuard`] — the **RAII teardown**: its `Drop` restores the terminal
//!   (disable raw mode, leave alt screen, show cursor) on *every* exit — normal
//!   quit, error, and panic — so a panic mid-render never wrecks the user's
//!   terminal. The guard is built around an injectable restore closure so a test
//!   can prove `Drop` fires the restore.
//! - [`CoworkApp::pump`] — the non-blocking driver step (submit / poll /
//!   transition status) the live loop calls each frame.
//!
//! The only by-design-uncovered surface is the raw render + crossterm event
//! loop in [`run_cowork`] (terminal I/O against a real tty), mirroring the
//! carve-out `gila follow` uses for its live tail loop.

use std::io::{self, Write};

use crossterm::{
    cursor::Show,
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use newt_core::agentic::{transcript_lines, TranscriptLine, TranscriptRole, TurnStatus};

use crate::follow::ObservationChannel;

/// Placeholder text the bottom pane shows until the PTY shell lands in #10.
pub const SHELL_PLACEHOLDER: &str = "shell pane (PTY lands in #10)";

/// The standing system framing for the cowork agent: it shares a screen with the
/// human's live shell and should be a concise pair-programming partner.
pub const COWORK_SYSTEM_PROMPT: &str =
    "We are pair-programming in a split screen: your chat is on top, my live \
     shell is below. Be concise and practical.";

/// Which pane currently has keyboard focus. Keystrokes route to the focused
/// pane; the focus-swap hotkey toggles between the two. In Tier B/1 only the
/// chat pane is interactive (the shell pane is a placeholder); #10 makes the
/// shell pane route keys to the PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The top agent-chat pane has focus (typing submits to the [`TurnDriver`](newt_core::agentic::TurnDriver)).
    Chat,
    /// The bottom shell pane has focus (#10: keys go to the PTY).
    Shell,
}

impl Focus {
    /// The focus that results from pressing the focus-swap hotkey: a pure toggle
    /// between the two panes.
    #[must_use]
    pub fn swapped(self) -> Self {
        match self {
            Self::Chat => Self::Shell,
            Self::Shell => Self::Chat,
        }
    }

    /// A short label for the status line ("chat" / "shell").
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Shell => "shell",
        }
    }
}

/// The three regions a cowork frame splits into, in screen order.
///
/// Returned by [`split_panes`] so the split math is unit-testable without a
/// terminal: the renderer draws the chat transcript into `chat`, the shell
/// placeholder (later the PTY) into `shell`, and the help/status line into
/// `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneLayout {
    /// Top pane: the agent chat transcript.
    pub chat: Rect,
    /// Bottom pane: the shell placeholder (#10 fills it with a PTY).
    pub shell: Rect,
    /// One-row status / help line at the very bottom.
    pub status: Rect,
}

/// Split a full-screen [`Rect`] into the cowork layout: top chat pane, bottom
/// shell pane, and a one-row status line.
///
/// The chat and shell panes split the remaining height evenly (chat takes the
/// extra row on an odd height, so the human's shell is never *taller* than the
/// chat by rounding); the status line is always exactly one row. Pure geometry
/// over the input rect — no terminal needed — so the regions are unit-testable.
///
/// Degenerate areas (height < 2) collapse gracefully: the status row is taken
/// first, and whatever is left is split between the panes, so the function never
/// panics or produces out-of-bounds rects.
#[must_use]
pub fn split_panes(area: Rect) -> PaneLayout {
    // The status line claims the bottom row when there is height for it.
    let status_h: u16 = if area.height >= 1 { 1 } else { 0 };
    let body_h = area.height.saturating_sub(status_h);

    // Split the body evenly; chat keeps the odd row so the shell is never taller.
    let shell_h = body_h / 2;
    let chat_h = body_h - shell_h;

    let chat = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: chat_h,
    };
    let shell = Rect {
        x: area.x,
        y: area.y.saturating_add(chat_h),
        width: area.width,
        height: shell_h,
    };
    let status = Rect {
        x: area.x,
        y: area.y.saturating_add(chat_h).saturating_add(shell_h),
        width: area.width,
        height: status_h,
    };
    PaneLayout {
        chat,
        shell,
        status,
    }
}

/// Map a newt [`TranscriptLine`] to a styled ratatui [`Line`].
///
/// This is the renderer-agnostic-data → rich-widget bridge: newt hands us
/// role-tagged, width-wrapped lines and we choose the colors and the speaker
/// gutter. The *first* physical line of a message gets a colored `you ▸` /
/// `newt ▸` label; continuation lines get a blank, same-width gutter so the text
/// stays aligned. Pure (no terminal), so the mapping is unit-testable.
#[must_use]
pub fn line_to_ratatui(line: &TranscriptLine) -> Line<'static> {
    let (label, color) = match line.role {
        TranscriptRole::User => ("you", Color::Cyan),
        TranscriptRole::Assistant => ("newt", Color::Green),
        TranscriptRole::Tool => ("tool", Color::Yellow),
        TranscriptRole::System => ("sys", Color::DarkGray),
    };
    // A fixed-width gutter keeps wrapped lines aligned under the label.
    let gutter = if line.is_first {
        Span::styled(
            format!("{label:>4} \u{25b8} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("       ")
    };
    Line::from(vec![gutter, Span::raw(line.text.clone())])
}

/// Render a transcript into ratatui [`Line`]s sized to a `width`-column pane.
///
/// Wraps [`newt_core::agentic::transcript_lines`] (the renderer-agnostic data)
/// and maps each row through [`line_to_ratatui`]. The whole rich-render path is
/// thus a pure function of `(messages, width)`, unit-tested without a terminal;
/// the live loop just calls this each frame with the chat pane's inner width.
#[must_use]
pub fn transcript_to_lines(messages: &[newt_core::MemMessage], width: usize) -> Vec<Line<'static>> {
    transcript_lines(messages, width)
        .iter()
        .map(line_to_ratatui)
        .collect()
}

/// The cowork app's pure UI state — everything the render reads and the event
/// loop mutates, with **no** terminal handles. Holding the state separate from
/// the raw render/event loop is what lets the gate cover focus, input, and the
/// driver-pump transitions while the untestable tty I/O stays in [`run_cowork`].
pub struct CoworkApp {
    /// The shared shell-observation channel (owns the read-only-or-not
    /// [`TurnDriver`](newt_core::agentic::TurnDriver)). #10's PTY source feeds this same channel.
    channel: ObservationChannel,
    /// Which pane has keyboard focus.
    focus: Focus,
    /// The chat pane's in-progress input line (what the human is typing).
    input: String,
    /// The last status the driver pump observed, shown on the status line.
    status: TurnState,
    /// Set once the human asks to quit; the live loop breaks on it.
    should_quit: bool,
}

/// A render-friendly snapshot of the turn driver's state for the status line.
///
/// Distinct from newt's [`TurnStatus`](newt_core::agentic::TurnStatus) (which carries the one-shot
/// `Completed`/`Failed` payloads): this is the *sticky* status the UI displays
/// between polls, so a `Completed` turn shows "completed" until the next submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnState {
    /// No turn in flight; ready for input.
    Idle,
    /// A turn is running.
    Running,
    /// The last turn completed.
    Completed,
    /// The last turn failed, with the reason.
    Failed(String),
}

impl TurnState {
    /// The status-line label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Idle => "idle".to_string(),
            Self::Running => "running".to_string(),
            Self::Completed => "completed".to_string(),
            Self::Failed(why) => format!("failed: {why}"),
        }
    }
}

impl CoworkApp {
    /// Build the app around an [`ObservationChannel`]. Focus starts on the chat
    /// pane (the only interactive pane in Tier B/1).
    #[must_use]
    pub fn new(channel: ObservationChannel) -> Self {
        Self {
            channel,
            focus: Focus::Chat,
            input: String::new(),
            status: TurnState::Idle,
            should_quit: false,
        }
    }

    /// The pane that currently has focus.
    #[must_use]
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// Toggle keyboard focus between the chat and shell panes — the focus-swap
    /// hotkey's effect.
    pub fn swap_focus(&mut self) {
        self.focus = self.focus.swapped();
    }

    /// The chat pane's current input buffer.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The sticky turn status shown on the status line.
    #[must_use]
    pub fn status(&self) -> &TurnState {
        &self.status
    }

    /// Whether the live loop should exit.
    #[must_use]
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Request that the live loop exit at the next iteration.
    pub fn request_quit(&mut self) {
        self.should_quit = true;
    }

    /// Append a typed character to the chat input (only meaningful while the
    /// chat pane has focus — the caller routes keys by focus).
    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
    }

    /// Backspace one character off the chat input.
    pub fn backspace(&mut self) {
        self.input.pop();
    }

    /// Submit the current chat input as a turn and clear the input line.
    ///
    /// A blank line is a no-op (no empty turns). If a turn is already running
    /// the driver rejects the submit ([`TurnDriver`](newt_core::agentic::TurnDriver) runs one turn at a time);
    /// the input is preserved so the human can resubmit once it settles.
    /// Returns whether a turn was actually started.
    pub fn submit_input(&mut self) -> bool {
        let text = self.input.trim();
        if text.is_empty() {
            return false;
        }
        match self.channel.driver().submit(text.to_string()) {
            Ok(()) => {
                self.input.clear();
                self.status = TurnState::Running;
                true
            }
            // Busy: a turn is already in flight. Keep the input for a resubmit.
            Err(_) => false,
        }
    }

    /// Non-blocking driver step the live loop calls each frame.
    ///
    /// Polls the [`TurnDriver`](newt_core::agentic::TurnDriver) once (never blocks) and folds the one-shot
    /// [`TurnStatus`](newt_core::agentic::TurnStatus) into the sticky [`TurnState`] the UI shows. The chat pane
    /// updates as the turn progresses because the driver appends the assistant
    /// reply to the transcript on completion — the next render picks it up.
    /// Returns the sticky state after the poll.
    pub fn pump(&mut self) -> TurnState {
        match self.channel.driver().poll() {
            TurnStatus::Idle => {
                // Don't clobber a just-finished status with Idle: only fall back
                // to Idle if we weren't mid-run.
                if self.status == TurnState::Running {
                    self.status = TurnState::Idle;
                }
            }
            TurnStatus::Running => self.status = TurnState::Running,
            TurnStatus::Completed(_) => self.status = TurnState::Completed,
            TurnStatus::Failed(why) => self.status = TurnState::Failed(why),
        }
        self.status.clone()
    }

    /// Borrow the channel — for rendering the transcript and (in #10) for the
    /// PTY source to feed observations into.
    pub fn channel(&mut self) -> &mut ObservationChannel {
        &mut self.channel
    }

    /// The chat transcript rendered to ratatui lines for a `width`-column pane.
    /// Convenience over [`transcript_to_lines`] that reaches the driver's
    /// transcript for the live loop.
    #[must_use]
    pub fn chat_lines(&mut self, width: usize) -> Vec<Line<'static>> {
        transcript_to_lines(self.channel.driver().transcript(), width)
    }

    /// The status / help line text.
    #[must_use]
    pub fn status_line(&self) -> String {
        format!(
            "[{}] focus: {} | Ctrl-O swap focus | Enter send | Ctrl-Q quit",
            self.status.label(),
            self.focus.label()
        )
    }
}

/// The border style for a pane, highlighted when it holds focus.
///
/// The focused pane gets a bold cyan border (the focus indicator); the unfocused
/// pane gets a dim gray one. Pure — keyed only on whether `pane == focus` — so
/// the focus indicator is unit-testable and the render is just a lookup.
#[must_use]
fn border_style(pane: Focus, focus: Focus) -> Style {
    if pane == focus {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Draw one full cowork frame into `frame` from the app's current state.
///
/// This is the **whole render path**, lifted out of the binary's event loop so
/// it is testable with ratatui's [`TestBackend`](ratatui::backend::TestBackend):
/// it splits the area ([`split_panes`]), draws the chat transcript (top, focus-
/// highlighted border), the shell placeholder (bottom, focus-highlighted
/// border), and the status/help line (with the live input echoed). The binary's
/// loop calls `terminal.draw(|f| render_frame(&mut app, f))` and owns only the
/// crossterm setup/teardown + input polling around it.
///
/// The focused pane's border is the **focus indicator** — bold cyan vs. dim gray
/// (see [`border_style`]). #10 fills the shell pane's interior with a real PTY;
/// the frame structure here does not change.
pub fn render_frame(app: &mut CoworkApp, frame: &mut Frame) {
    let layout = split_panes(frame.area());
    let focus = app.focus();
    let status_text = app.status_line();
    let input = app.input().to_string();

    // Top: the agent chat transcript, rendered from newt's renderer-agnostic
    // transcript data. Inner width = pane width minus the two border columns.
    let chat_inner = layout.chat.width.saturating_sub(2).max(1) as usize;
    let chat_lines = app.chat_lines(chat_inner);
    let chat = Paragraph::new(Text::from(chat_lines)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style(Focus::Chat, focus))
            .title(" agent chat "),
    );
    frame.render_widget(chat, layout.chat);

    // Bottom: the shell placeholder (#10 swaps in a real PTY here).
    let shell = Paragraph::new(Line::from(Span::styled(
        SHELL_PLACEHOLDER,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )))
    .alignment(Alignment::Center)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style(Focus::Shell, focus))
            .title(" shell "),
    );
    frame.render_widget(shell, layout.shell);

    // Status / help line, with the live input echoed.
    let status = Paragraph::new(Line::from(vec![
        Span::styled(status_text, Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::raw(input),
    ]));
    frame.render_widget(status, layout.status);
}

/// RAII guard that **restores the terminal on every exit path** — normal quit,
/// `?`-propagated error, and panic.
///
/// `gila cowork` puts the terminal into raw mode + the alternate screen and
/// hides the cursor. If any of those are left set when the program exits, the
/// user's shell is wrecked (no echo, stuck on the alt buffer). A guard whose
/// `Drop` runs the restore is the only leak-proof shape: `Drop` fires on a clean
/// return, on an error unwinding out of `run_cowork`, and on a panic mid-render
/// (Rust runs destructors while unwinding). This mirrors the leak class called
/// out in `newt-agent#302`.
///
/// The restore action is an injectable `FnMut`, so a unit test can install a
/// closure that records it ran and assert the guard fires it on `Drop` — proving
/// the teardown without touching a real tty.
pub struct TerminalGuard<F: FnMut()> {
    restore: F,
    /// Set after a manual [`restore`](Self::restore) so `Drop` doesn't run it
    /// twice (idempotent teardown).
    done: bool,
}

impl<F: FnMut()> TerminalGuard<F> {
    /// Wrap a restore action. The action runs exactly once — on the first of
    /// an explicit [`restore`](Self::restore) or `Drop`.
    pub fn new(restore: F) -> Self {
        Self {
            restore,
            done: false,
        }
    }

    /// Run the restore now (idempotent). Lets the caller tear down deterministically
    /// before printing a post-exit message; `Drop` then becomes a no-op.
    pub fn restore(&mut self) {
        if !self.done {
            (self.restore)();
            self.done = true;
        }
    }
}

impl<F: FnMut()> Drop for TerminalGuard<F> {
    fn drop(&mut self) {
        self.restore();
    }
}

/// The real terminal-restore action: leave the alternate screen, disable raw
/// mode, and show the cursor. Used by [`run_cowork`] to build its
/// [`TerminalGuard`]; factored out so the guard wiring is one line and the
/// effectful crossterm calls are isolated. Best-effort — a failed restore step
/// must not panic during teardown, so errors are swallowed.
pub fn restore_terminal() {
    let mut out = io::stdout();
    let _ = execute!(out, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// Put the terminal into the cowork rendering mode: raw mode + alternate screen.
/// The matching teardown is [`restore_terminal`], wired through a
/// [`TerminalGuard`] so it always runs. Returns an error without leaving the
/// terminal half-configured if either step fails.
pub fn setup_terminal() -> io::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::agentic::{TurnDriver, TurnDriverConfig};
    use newt_core::{BackendKind, MemMessage};
    use std::cell::Cell;
    use std::rc::Rc;

    fn test_channel() -> ObservationChannel {
        // A driver pointed at a dead port — we never start a turn in the pure
        // tests, so the endpoint is never dialed.
        let cfg =
            TurnDriverConfig::new("http://127.0.0.1:1", "test-model", BackendKind::Ollama, ".");
        ObservationChannel::new(TurnDriver::new(cfg))
    }

    fn app() -> CoworkApp {
        CoworkApp::new(test_channel())
    }

    // --- split_panes geometry ----------------------------------------------

    #[test]
    fn split_panes_stacks_chat_shell_status_full_width() {
        let area = Rect::new(0, 0, 80, 25);
        let l = split_panes(area);
        // Full width everywhere.
        assert_eq!(l.chat.width, 80);
        assert_eq!(l.shell.width, 80);
        assert_eq!(l.status.width, 80);
        // Status is exactly one row, pinned to the bottom.
        assert_eq!(l.status.height, 1);
        assert_eq!(l.status.y, 24);
        // Panes stack with no gap and no overlap: chat, then shell, then status.
        assert_eq!(l.chat.y, 0);
        assert_eq!(l.shell.y, l.chat.y + l.chat.height);
        assert_eq!(l.status.y, l.shell.y + l.shell.height);
        // The three heights tile the whole area exactly.
        assert_eq!(l.chat.height + l.shell.height + l.status.height, 25);
    }

    #[test]
    fn split_panes_chat_takes_the_odd_row_so_shell_is_never_taller() {
        // 25 rows: 1 status, 24 body → 12/12. 24 rows: 1 status, 23 body → 12/11.
        let l = split_panes(Rect::new(0, 0, 80, 24));
        assert_eq!(l.status.height, 1);
        assert!(
            l.chat.height >= l.shell.height,
            "chat {} should be >= shell {}",
            l.chat.height,
            l.shell.height
        );
        assert_eq!(l.chat.height, 12);
        assert_eq!(l.shell.height, 11);
    }

    #[test]
    fn split_panes_degenerate_heights_dont_panic_or_overflow() {
        for h in 0u16..=3 {
            let l = split_panes(Rect::new(0, 0, 10, h));
            // Always tiles exactly and never exceeds the area.
            assert_eq!(l.chat.height + l.shell.height + l.status.height, h);
            assert!(l.status.y + l.status.height <= h);
        }
        // Zero-height: status collapses to zero rows too.
        let l0 = split_panes(Rect::new(0, 0, 10, 0));
        assert_eq!(l0.status.height, 0);
        assert_eq!(l0.chat.height, 0);
        assert_eq!(l0.shell.height, 0);
    }

    #[test]
    fn split_panes_respects_a_nonzero_origin() {
        let l = split_panes(Rect::new(5, 7, 40, 11));
        assert_eq!(l.chat.x, 5);
        assert_eq!(l.chat.y, 7);
        assert_eq!(l.shell.x, 5);
        // Body = 10 rows → chat 5, shell 5; status one row at the bottom.
        assert_eq!(l.chat.height, 5);
        assert_eq!(l.shell.height, 5);
        assert_eq!(l.status.y, 7 + 10);
    }

    // --- focus state machine ------------------------------------------------

    #[test]
    fn focus_swapped_toggles_both_ways() {
        assert_eq!(Focus::Chat.swapped(), Focus::Shell);
        assert_eq!(Focus::Shell.swapped(), Focus::Chat);
        // Two swaps is identity.
        assert_eq!(Focus::Chat.swapped().swapped(), Focus::Chat);
    }

    #[test]
    fn focus_labels() {
        assert_eq!(Focus::Chat.label(), "chat");
        assert_eq!(Focus::Shell.label(), "shell");
    }

    #[test]
    fn app_starts_focused_on_chat_and_swaps() {
        let mut a = app();
        assert_eq!(a.focus(), Focus::Chat);
        a.swap_focus();
        assert_eq!(a.focus(), Focus::Shell);
        a.swap_focus();
        assert_eq!(a.focus(), Focus::Chat);
    }

    // --- input handling -----------------------------------------------------

    #[test]
    fn input_push_and_backspace() {
        let mut a = app();
        assert_eq!(a.input(), "");
        a.push_char('h');
        a.push_char('i');
        assert_eq!(a.input(), "hi");
        a.backspace();
        assert_eq!(a.input(), "h");
        a.backspace();
        a.backspace(); // backspace on empty is a no-op
        assert_eq!(a.input(), "");
    }

    #[test]
    fn submit_blank_input_is_a_noop() {
        let mut a = app();
        assert!(!a.submit_input(), "blank submit starts no turn");
        a.push_char(' ');
        a.push_char('\t');
        assert!(!a.submit_input(), "whitespace-only submit starts no turn");
    }

    #[test]
    fn submit_nonblank_input_starts_a_turn_and_clears_input() {
        let mut a = app();
        a.push_char('h');
        a.push_char('i');
        assert!(a.submit_input(), "a non-blank line starts a turn");
        assert_eq!(a.input(), "", "input is cleared after submit");
        assert_eq!(a.status(), &TurnState::Running);
        // The user message is now in the transcript.
        assert!(a
            .channel()
            .driver()
            .transcript()
            .iter()
            .any(|m| m.content.contains("hi")));
        // Clean up the in-flight turn so the worker thread is joined.
        a.channel().driver().cancel();
    }

    #[test]
    fn submit_while_running_preserves_input() {
        let mut a = app();
        a.push_char('o');
        a.push_char('n');
        a.push_char('e');
        assert!(a.submit_input());
        // A second submit while the first turn is in flight is rejected; the
        // text stays so the human can resend.
        a.push_char('t');
        a.push_char('w');
        a.push_char('o');
        assert!(!a.submit_input(), "busy driver rejects a second submit");
        assert_eq!(a.input(), "two");
        a.channel().driver().cancel();
    }

    // --- pump (non-blocking driver step) ------------------------------------

    #[test]
    fn pump_when_idle_stays_idle() {
        let mut a = app();
        assert_eq!(a.pump(), TurnState::Idle);
        assert_eq!(a.status(), &TurnState::Idle);
    }

    /// THE non-blocking submit→poll integration test: a real (mocked) turn is
    /// submitted, `pump` reports `Running` while it is in flight without blocking,
    /// then folds the completion into a sticky `Completed` state — proving the
    /// chat updates as the turn progresses while the loop stays responsive. The
    /// turn appends the assistant reply to the transcript, which the next render
    /// would pick up. A wiremock backend stands in for the model (same stack
    /// `follow` uses) so the test is deterministic and fast.
    #[tokio::test(flavor = "multi_thread")]
    async fn pump_runs_then_completes_against_a_mock_backend() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "hello back" }
            })))
            .mount(&server)
            .await;

        let cfg = TurnDriverConfig::new(server.uri(), "test-model", BackendKind::Ollama, ".");
        let mut a = CoworkApp::new(ObservationChannel::new(TurnDriver::new(cfg)));

        a.push_char('h');
        a.push_char('i');
        assert!(a.submit_input());
        assert_eq!(a.status(), &TurnState::Running);

        // Pump non-blocking until the turn settles. Bounded so a wedge can't hang.
        let mut final_state = TurnState::Running;
        for _ in 0..2000 {
            final_state = a.pump();
            if !matches!(final_state, TurnState::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            final_state,
            TurnState::Completed,
            "a successful turn must surface as Completed"
        );
        // The assistant reply landed in the transcript — the next render shows it.
        assert!(a
            .channel()
            .driver()
            .transcript()
            .iter()
            .any(|m| m.content.contains("hello back")));
        // Sticky: a subsequent idle pump does not clobber the Completed status.
        assert_eq!(a.pump(), TurnState::Completed);
    }

    /// The failure transition: a backend that 500s makes the turn fail, and
    /// `pump` folds that into a sticky `Failed` state (with the reason) without
    /// blocking. Uses a mock 500 rather than a dead port so it fails fast and
    /// deterministically (a dead TCP port triggers connection-retry backoff).
    #[tokio::test(flavor = "multi_thread")]
    async fn pump_reports_failed_when_the_backend_errors() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let cfg = TurnDriverConfig::new(server.uri(), "test-model", BackendKind::Ollama, ".");
        let mut a = CoworkApp::new(ObservationChannel::new(TurnDriver::new(cfg)));

        a.push_char('x');
        assert!(a.submit_input());
        assert_eq!(a.status(), &TurnState::Running);

        let mut final_state = TurnState::Running;
        for _ in 0..2000 {
            final_state = a.pump();
            if !matches!(final_state, TurnState::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            matches!(final_state, TurnState::Failed(_)),
            "an erroring backend must surface as Failed, got {final_state:?}"
        );
        // Sticky: a subsequent idle pump does not clobber the Failed status.
        assert!(matches!(a.pump(), TurnState::Failed(_)));
    }

    #[test]
    fn turn_state_labels() {
        assert_eq!(TurnState::Idle.label(), "idle");
        assert_eq!(TurnState::Running.label(), "running");
        assert_eq!(TurnState::Completed.label(), "completed");
        assert_eq!(TurnState::Failed("boom".into()).label(), "failed: boom");
    }

    // --- transcript → ratatui line mapping ----------------------------------

    #[test]
    fn line_to_ratatui_labels_first_line_by_role() {
        let user_first = TranscriptLine {
            role: TranscriptRole::User,
            is_first: true,
            text: "hello".to_string(),
        };
        let line = line_to_ratatui(&user_first);
        // First span is the colored gutter label; second is the text.
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("you"));
        assert!(rendered.contains("hello"));

        let asst_cont = TranscriptLine {
            role: TranscriptRole::Assistant,
            is_first: false,
            text: "wrapped".to_string(),
        };
        let line2 = line_to_ratatui(&asst_cont);
        let rendered2: String = line2.spans.iter().map(|s| s.content.as_ref()).collect();
        // Continuation lines have no label, just the aligned gutter + text.
        assert!(!rendered2.contains("newt"));
        assert!(rendered2.contains("wrapped"));
    }

    #[test]
    fn line_to_ratatui_colors_roles_distinctly() {
        let mk = |role| {
            line_to_ratatui(&TranscriptLine {
                role,
                is_first: true,
                text: "t".into(),
            })
            .spans[0]
                .style
                .fg
        };
        assert_eq!(mk(TranscriptRole::User), Some(Color::Cyan));
        assert_eq!(mk(TranscriptRole::Assistant), Some(Color::Green));
        assert_eq!(mk(TranscriptRole::Tool), Some(Color::Yellow));
        assert_eq!(mk(TranscriptRole::System), Some(Color::DarkGray));
    }

    #[test]
    fn transcript_to_lines_maps_a_dialogue() {
        let msgs = [MemMessage::user("what is 2+2"), MemMessage::assistant("4")];
        let lines = transcript_to_lines(&msgs, 40);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("you"));
        assert!(joined.contains("what is 2+2"));
        assert!(joined.contains("newt"));
        assert!(joined.contains('4'));
    }

    #[test]
    fn chat_lines_renders_the_drivers_transcript() {
        let mut a = app();
        // Seed a benign observation so the transcript is non-empty without a turn.
        a.channel().feed("pty", "$ echo hi\nhi\n");
        let lines = a.chat_lines(60);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("echo hi"), "transcript rendered: {joined}");
    }

    // --- status line --------------------------------------------------------

    #[test]
    fn status_line_shows_focus_and_help_keys() {
        let mut a = app();
        let s = a.status_line();
        assert!(s.contains("focus: chat"));
        assert!(s.contains("Ctrl-O"));
        assert!(s.contains("Ctrl-Q"));
        assert!(s.contains("idle"));
        a.swap_focus();
        assert!(a.status_line().contains("focus: shell"));
    }

    #[test]
    fn quit_request_is_observable() {
        let mut a = app();
        assert!(!a.should_quit());
        a.request_quit();
        assert!(a.should_quit());
    }

    #[test]
    fn shell_placeholder_announces_the_followup() {
        assert!(SHELL_PLACEHOLDER.contains("#10"));
        assert!(SHELL_PLACEHOLDER.to_lowercase().contains("shell"));
    }

    // --- border focus indicator --------------------------------------------

    #[test]
    fn border_style_highlights_the_focused_pane_only() {
        // Focused pane: bold cyan. Unfocused: dim gray. The contrast is the
        // on-screen focus indicator.
        let chat_focused = border_style(Focus::Chat, Focus::Chat);
        assert_eq!(chat_focused.fg, Some(Color::Cyan));
        assert!(chat_focused.add_modifier.contains(Modifier::BOLD));

        let shell_unfocused = border_style(Focus::Shell, Focus::Chat);
        assert_eq!(shell_unfocused.fg, Some(Color::DarkGray));
        assert!(!shell_unfocused.add_modifier.contains(Modifier::BOLD));
    }

    // --- full-frame render via ratatui TestBackend --------------------------

    /// THE snapshot test the issue's test plan calls for: render a full cowork
    /// frame to a `TestBackend` and assert the split is present (both pane
    /// titles), the shell placeholder shows, the status/help line shows, and the
    /// focus indicator distinguishes the panes. Exercises the whole render path
    /// (`render_frame` → `split_panes` → the widget composition) without a tty.
    #[test]
    fn render_frame_draws_the_split_with_focus_indicator() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut a = app();
        // Seed a benign observation so the chat pane has content to draw.
        a.channel().feed("pty", "$ echo hi\nhi\n");

        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|f| render_frame(&mut a, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // Flatten the rendered cells into one string to assert on visible text.
        let rendered: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            rendered.contains("agent chat"),
            "chat pane title: {rendered}"
        );
        assert!(rendered.contains("shell"), "shell pane title/placeholder");
        assert!(
            rendered.contains("PTY lands in #10"),
            "placeholder text shown"
        );
        assert!(rendered.contains("focus: chat"), "status line shows focus");
        assert!(rendered.contains("Ctrl-O"), "help keys shown");
        assert!(rendered.contains("echo hi"), "chat transcript drawn");

        // The focus indicator: the chat pane's top-left border cell is cyan+bold
        // (focused), the shell pane's is gray (unfocused).
        let layout = split_panes(ratatui::layout::Rect::new(0, 0, 60, 16));
        let chat_corner = &buf[(layout.chat.x, layout.chat.y)];
        assert_eq!(chat_corner.style().fg, Some(Color::Cyan));
        assert!(chat_corner.style().add_modifier.contains(Modifier::BOLD));
        let shell_corner = &buf[(layout.shell.x, layout.shell.y)];
        assert_eq!(shell_corner.style().fg, Some(Color::DarkGray));
    }

    #[test]
    fn render_frame_moves_focus_indicator_on_swap() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut a = app();
        a.swap_focus(); // focus now on the shell pane
        let mut terminal = Terminal::new(TestBackend::new(50, 12)).unwrap();
        terminal.draw(|f| render_frame(&mut a, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let rendered: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(rendered.contains("focus: shell"));

        let layout = split_panes(ratatui::layout::Rect::new(0, 0, 50, 12));
        // Now the SHELL pane corner is the highlighted one.
        let shell_corner = &buf[(layout.shell.x, layout.shell.y)];
        assert_eq!(shell_corner.style().fg, Some(Color::Cyan));
        let chat_corner = &buf[(layout.chat.x, layout.chat.y)];
        assert_eq!(chat_corner.style().fg, Some(Color::DarkGray));
    }

    // --- RAII teardown guard ------------------------------------------------

    #[test]
    fn guard_runs_restore_exactly_once_on_drop() {
        let calls = Rc::new(Cell::new(0));
        {
            let c = calls.clone();
            let _guard = TerminalGuard::new(move || c.set(c.get() + 1));
            assert_eq!(calls.get(), 0, "restore must not run before drop");
        } // guard dropped here
        assert_eq!(calls.get(), 1, "Drop must run restore exactly once");
    }

    #[test]
    fn guard_manual_restore_makes_drop_a_noop() {
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let mut guard = TerminalGuard::new(move || c.set(c.get() + 1));
        guard.restore();
        assert_eq!(calls.get(), 1, "explicit restore runs the action");
        guard.restore();
        assert_eq!(calls.get(), 1, "a second explicit restore is idempotent");
        drop(guard);
        assert_eq!(calls.get(), 1, "Drop after a manual restore is a no-op");
    }

    /// THE load-bearing teardown test: a panic mid-"render" still restores the
    /// terminal, because `Drop` runs while the stack unwinds. We install a guard
    /// whose restore records it ran, panic inside the scope, catch the unwind,
    /// and assert the restore fired.
    #[test]
    fn guard_restores_on_panic_unwind() {
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = TerminalGuard::new(move || c.set(c.get() + 1));
            // Simulate a panic mid-render with the guard live on the stack.
            panic!("render blew up");
        }));
        assert!(result.is_err(), "the panic propagated out of the scope");
        assert_eq!(
            calls.get(),
            1,
            "the guard's Drop restored the terminal during unwind"
        );
    }
}
