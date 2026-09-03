//! The cockpit's pane-backend seam (#54, phase 4 ratchet 3).
//!
//! [`cockpit`](crate::cockpit) is pure: it knows a pane's [`PaneRole`] and
//! [`PaneToken`], nothing about drivers, PTYs, or ratatui. This is the other
//! half, the live resource behind a token. The loop owns
//! `HashMap<PaneToken, Box<dyn PaneBackend>>` and performs [`Effect`]s as
//! inserts and removes.
//!
//! [`PaneRole`]: crate::cockpit::PaneRole
//! [`PaneToken`]: crate::cockpit::PaneToken
//! [`Effect`]: crate::cockpit::Effect
//!
//! | impl | hosts | authority |
//! |---|---|---|
//! | [`ChatPaneBackend`] | a `TurnDriver` chat pane | minted **only** via [`authority::driver_config`](crate::authority::driver_config), clamped by [`PaneKind`] |
//! | [`PtyPaneBackend`] | the human's `$SHELL` on a pty | none |
//! | [`FailedPaneBackend`] | an error message | none, inert text |
//!
//! A failed pane is a backend, not an error return. A pane whose driver or
//! shell dies must not take the cockpit down with it, and must not silently
//! become an empty pane. [`FailedPaneBackend`] keeps map and model in lockstep,
//! one backend per live pane, while showing why that pane is dead.

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use newt_core::agentic::{TurnDriver, TurnStatus};

use crate::authority::{self, PaneKind};
use crate::cowork::{transcript_to_lines, TurnState};
use crate::pty::{encode_key, pty_size_for, screen_to_lines, PtyShell, PtyWriter, SharedScreen};

/// The live resource behind one cockpit pane.
///
/// Object-safe on purpose: the raw loop stores `Box<dyn PaneBackend>` keyed by
/// [`PaneToken`](crate::cockpit::PaneToken), so a chat pane, a shell pane, and a
/// failed pane are indistinguishable to the render/event loop.
pub trait PaneBackend {
    /// The pane's title, drawn in its border.
    fn title(&self) -> String;

    /// The pane's content, rendered for a `width` × `height` **interior** (the
    /// caller has already subtracted the border).
    fn render_lines(&mut self, width: u16, height: u16) -> Vec<Line<'static>>;

    /// Handle one key the loop routed here (this pane has focus and the prefix
    /// dispatcher did not claim the key). Defaults to ignoring it, which is
    /// what an inert pane wants.
    fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) {}

    /// The pane's rect changed, resize whatever the backend owns (a pty's
    /// kernel size and parser grid). Defaults to nothing: a pane with no live
    /// resource re-wraps on its next render.
    fn resize(&mut self, _area: Rect) {}

    /// One non-blocking step per frame: poll the driver, notice a dead child.
    /// Must never block the render loop. Defaults to nothing.
    fn tick(&mut self) {}

    /// New output this pane produced since the last call, for the supervision
    /// channel. Defaults to `None`: a pane that consumes observations is never
    /// also a source, since feeding its own output back would be a loop.
    fn observation(&mut self) -> Option<String> {
        None
    }

    /// Whether the pane ended on its own (the human typed `exit`); the loop
    /// closes such panes on the next frame. Defaults to `false`, for a pane
    /// that only the operator can close.
    fn is_closed(&mut self) -> bool {
        false
    }
}

// ── chat panes ──────────────────────────────────────────────────────────────

/// Everything a chat pane needs to mint its driver: the operator's resolved
/// newt backend, flattened so the pane never touches `newt_core::Config`.
///
/// Lives here rather than in [`cockpit_app`](crate::cockpit_app) callers' hands
/// because minting is this module's job; the app only carries the profile from
/// the resolved config to each new pane.
#[derive(Debug, Clone)]
pub struct BackendProfile {
    /// The inference endpoint URL.
    pub endpoint: String,
    /// The model name (newt #1128 made this optional in config; the cockpit has
    /// no probe step, so the caller resolves it or fails loud).
    pub model: String,
    /// The wire protocol.
    pub kind: newt_core::BackendKind,
    /// The resolved API key, if the backend needs one.
    pub api_key: Option<String>,
    /// The workspace path the driver is rooted at.
    pub workspace: String,
}

/// The standing framing for a cockpit chat pane: it shares a screen with other
/// panes and should be a concise partner, not a narrator.
pub const COCKPIT_SYSTEM_PROMPT: &str =
    "We are working in a split-pane cockpit: your chat is one pane among \
     several, alongside the human's live shells. Be concise and practical.";

/// A chat pane: a [`TurnDriver`] clamped by its [`PaneKind`], plus the input
/// line the human is typing.
///
/// The driver is minted **only** through
/// [`authority::driver_config`](crate::authority::driver_config), there is no
/// constructor here that takes a `TurnDriverConfig`, so a pane cannot exist with
/// an unclamped driver.
pub struct ChatPaneBackend {
    kind: PaneKind,
    /// The clamp this pane's driver was minted with. Kept alongside the driver
    /// (which does not expose its config) so the posture is inspectable, the
    /// regression test that a pane's authority is exactly its kind's.
    caveats: newt_core::Caveats,
    driver: TurnDriver,
    input: String,
    status: TurnState,
}

impl ChatPaneBackend {
    /// Mint a chat pane clamped to `kind` against the operator's `profile`.
    ///
    /// # Errors
    /// Propagates [`AuthorityError`](crate::authority::AuthorityError) when the
    /// caveat lattice would not actually bind (`NEWT_DISABLE_OCAP`). Callers
    /// surface the failure as a [`FailedPaneBackend`] rather than minting an
    /// unclamped driver.
    pub fn new(
        kind: PaneKind,
        profile: &BackendProfile,
    ) -> Result<Self, authority::AuthorityError> {
        let mut config = authority::driver_config(
            kind,
            &profile.endpoint,
            &profile.model,
            profile.kind,
            &profile.workspace,
        )?;
        // The key rides the config, not the caveats: it is a backend credential,
        // never an authority grant. `driver_config` has already stamped the clamp.
        config.api_key = profile.api_key.clone();
        let caveats = config.caveats.clone();
        Ok(Self {
            kind,
            caveats,
            driver: TurnDriver::with_transcript(
                config,
                vec![newt_core::MemMessage::system(COCKPIT_SYSTEM_PROMPT)],
            ),
            input: String::new(),
            status: TurnState::Idle,
        })
    }

    /// The pane's authority posture.
    #[must_use]
    pub fn kind(&self) -> PaneKind {
        self.kind
    }

    /// The caveats the driver was minted with, always
    /// [`caveats_for`](crate::authority::caveats_for) of [`kind`](Self::kind).
    #[must_use]
    pub fn caveats(&self) -> &newt_core::Caveats {
        &self.caveats
    }

    /// The in-progress input line.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The sticky turn status shown in the pane title.
    #[must_use]
    pub fn status(&self) -> &TurnState {
        &self.status
    }

    /// The driver, for the supervision channel to feed observations into.
    pub fn driver(&mut self) -> &mut TurnDriver {
        &mut self.driver
    }

    /// Submit the input line as a turn and clear it. A blank line is a no-op; a
    /// rejected submit (a turn already in flight) preserves the input so the
    /// human can resubmit. Mirrors `CoworkApp::submit_input`.
    pub fn submit(&mut self) -> bool {
        let text = self.input.trim();
        if text.is_empty() {
            return false;
        }
        match self.driver.submit(text.to_string()) {
            Ok(()) => {
                self.input.clear();
                self.status = TurnState::Running;
                true
            }
            Err(_) => false,
        }
    }
}

impl PaneBackend for ChatPaneBackend {
    fn title(&self) -> String {
        let kind = match self.kind {
            PaneKind::Companion => "companion",
            PaneKind::Reader => "reader",
            PaneKind::Workbench => "workbench",
        };
        format!("{kind} chat — {}", self.status.label())
    }

    fn render_lines(&mut self, width: u16, height: u16) -> Vec<Line<'static>> {
        let width = width.max(1) as usize;
        let mut lines = transcript_to_lines(self.driver.transcript(), width);
        // Keep the tail visible: the pane is a scroller, and the newest turn is
        // what the operator is watching. One row is reserved for the prompt.
        let room = (height.max(1) as usize).saturating_sub(1);
        if lines.len() > room {
            lines.drain(..lines.len() - room);
        }
        lines.push(Line::from(vec![
            Span::styled("▸ ", Style::default().fg(Color::Cyan)),
            Span::raw(self.input.clone()),
        ]));
        lines
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match code {
            KeyCode::Enter => {
                self.submit();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            // A Ctrl chord is never text. The Root key table is empty, so every
            // unbound `Ctrl+<letter>` is forwarded here; without this guard the
            // operator's cowork muscle memory (Ctrl+O to swap focus, Ctrl+A /
            // Ctrl+E to move within the line) would silently type `o`/`a`/`e`
            // into the prompt. `cowork::forward` carries the same guard.
            KeyCode::Char(_) if mods.contains(KeyModifiers::CONTROL) => {}
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn tick(&mut self) {
        // The same non-blocking poll `CoworkApp::pump` runs, folding the one-shot
        // TurnStatus into the sticky state the title shows. An errored turn comes
        // back `Completed` carrying `error: Some` (newt's partial-trajectory
        // contract), so that case folds back to `Failed`.
        match self.driver.poll() {
            TurnStatus::Idle => {
                if self.status == TurnState::Running {
                    self.status = TurnState::Idle;
                }
            }
            TurnStatus::Running => self.status = TurnState::Running,
            TurnStatus::Completed(outcome) => {
                self.status = match outcome.error {
                    Some(why) => TurnState::Failed(why),
                    None => TurnState::Completed,
                }
            }
            TurnStatus::Failed(why) => self.status = TurnState::Failed(why),
        }
    }
}

// ── shell panes ─────────────────────────────────────────────────────────────

/// A shell pane: the human's real program on a pty, mirrored through `vt100`.
///
/// The write half is the non-`Clone` [`PtyWriter`], owned here and reachable
/// only from [`handle_key`](PaneBackend::handle_key), the human's own
/// keystrokes. No driver-facing type holds one, so an agent is *structurally*
/// unable to type into this pane.
pub struct PtyPaneBackend {
    program: String,
    shell: PtyShell,
    writer: PtyWriter,
    shared: Arc<Mutex<SharedScreen>>,
    /// Sticky: once the child has exited we stop asking the guard.
    exited: bool,
}

impl PtyPaneBackend {
    /// Spawn `program` on a pty sized for `area`, rooted at `cwd`.
    ///
    /// # Errors
    /// Propagates the spawn failure (no pty available, program not found);
    /// callers surface it as a [`FailedPaneBackend`].
    pub fn spawn(program: &str, area: Rect, cwd: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let (shell, writer) = PtyShell::spawn(program, pty_size_for(area), cwd)?;
        let shared = shell.shared();
        Ok(Self {
            program: program.to_string(),
            shell,
            writer,
            shared,
            exited: false,
        })
    }

    /// The program this pane is hosting.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }
}

impl PaneBackend for PtyPaneBackend {
    fn title(&self) -> String {
        let leaf = self
            .program
            .rsplit('/')
            .next()
            .unwrap_or(&self.program)
            .to_string();
        if self.exited {
            format!("{leaf} (exited)")
        } else {
            leaf
        }
    }

    fn render_lines(&mut self, _width: u16, _height: u16) -> Vec<Line<'static>> {
        // The vt100 grid is already sized to the pane (see `resize`), so the
        // screen IS the render, no re-wrapping here.
        match self.shared.lock() {
            Ok(s) => screen_to_lines(s.screen()),
            Err(_) => vec![Line::raw("shell screen unavailable")],
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        // `encode_key` returns None for keys with no PTY meaning (a bare
        // modifier press), in which case we simply write nothing.
        if let Some(bytes) = encode_key(code, mods) {
            let _ = self.writer.write_input(&bytes);
        }
    }

    fn resize(&mut self, area: Rect) {
        self.shell.resize(area);
    }

    fn tick(&mut self) {
        if !self.exited && self.shell.has_exited() {
            self.exited = true;
        }
    }

    fn observation(&mut self) -> Option<String> {
        self.shared
            .lock()
            .ok()
            .and_then(|mut s| s.drain_new_output())
    }

    fn is_closed(&mut self) -> bool {
        self.tick();
        self.exited
    }
}

// ── failed panes ────────────────────────────────────────────────────────────

/// An inert pane that shows why its real backend could not be created. No
/// driver, no pty, nothing to leak and nothing to drive.
pub struct FailedPaneBackend {
    reason: String,
}

impl FailedPaneBackend {
    /// A failed pane carrying `reason` (rendered verbatim).
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The failure text.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl PaneBackend for FailedPaneBackend {
    fn title(&self) -> String {
        "failed".to_string()
    }

    fn render_lines(&mut self, _width: u16, _height: u16) -> Vec<Line<'static>> {
        self.reason
            .lines()
            .map(|l| Line::styled(l.to_string(), Style::default().fg(Color::Red)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(endpoint: String) -> BackendProfile {
        BackendProfile {
            endpoint,
            model: "test-model".to_string(),
            kind: newt_core::BackendKind::Ollama,
            api_key: None,
            workspace: ".".to_string(),
        }
    }

    fn chat() -> ChatPaneBackend {
        ChatPaneBackend::new(PaneKind::Companion, &profile("http://localhost:1".into())).unwrap()
    }

    // --- chat panes ---------------------------------------------------------

    #[test]
    fn a_chat_pane_is_minted_with_its_pane_kind_clamp() {
        // The authority guarantee at the backend layer: whatever kind a pane is
        // created with, its driver carries exactly that kind's caveats, there
        // is no constructor here that takes a config, so no other clamp is
        // reachable.
        for kind in [PaneKind::Companion, PaneKind::Reader, PaneKind::Workbench] {
            let b = ChatPaneBackend::new(kind, &profile("http://localhost:1".into()))
                .expect("ocap enabled in the test env");
            assert_eq!(b.kind(), kind);
            assert_eq!(*b.caveats(), authority::caveats_for(kind), "{kind:?}");
        }
    }

    #[test]
    fn chat_pane_title_names_the_kind_and_status() {
        let b = chat();
        assert_eq!(b.title(), "companion chat — idle");
    }

    #[test]
    fn chat_keys_edit_the_input_line() {
        let mut b = chat();
        for c in "hi!".chars() {
            b.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(b.input(), "hi!");
        b.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(b.input(), "hi");
        // An unmapped key changes nothing.
        b.handle_key(KeyCode::F(5), KeyModifiers::NONE);
        assert_eq!(b.input(), "hi");
    }

    /// A Ctrl chord is never text. The Root key table is empty, so every
    /// unbound `Ctrl+<letter>` is forwarded to the focused pane, without the
    /// guard, cowork muscle memory (Ctrl+O / Ctrl+A / Ctrl+E) typed stray
    /// letters into the prompt. Mirrors `cowork::forward`'s CONTROL check.
    #[test]
    fn chat_ignores_ctrl_chords_instead_of_typing_them() {
        let mut b = chat();
        for c in ['o', 'a', 'e', 'k'] {
            b.handle_key(KeyCode::Char(c), KeyModifiers::CONTROL);
        }
        assert_eq!(
            b.input(),
            "",
            "a Ctrl chord must never reach the input line"
        );
        // The same letters without CONTROL are ordinary text.
        b.handle_key(KeyCode::Char('o'), KeyModifiers::NONE);
        assert_eq!(b.input(), "o");
    }

    #[test]
    fn a_blank_submit_is_a_noop() {
        let mut b = chat();
        b.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        b.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(b.status(), &TurnState::Idle, "no empty turns");
    }

    #[test]
    fn chat_render_reserves_the_last_row_for_the_prompt_and_shows_the_tail() {
        let mut b = chat();
        b.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        let lines = b.render_lines(40, 3);
        assert!(lines.len() <= 3, "render fits the pane interior");
        let last = lines.last().expect("the prompt row is always present");
        let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "▸ x", "the input line is the last row");
    }

    /// The chat pane's non-blocking submit→poll cycle against a mocked backend
    /// (the same wiremock stack `cowork` uses): `tick` reports the turn running
    /// without blocking, then folds the completion into a sticky state, and the
    /// reply lands in the transcript the next render reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn tick_runs_then_completes_against_a_mock_backend() {
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

        let mut b = ChatPaneBackend::new(PaneKind::Companion, &profile(server.uri())).unwrap();
        for c in "hi".chars() {
            b.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        b.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(b.status(), &TurnState::Running);
        assert_eq!(b.input(), "", "a started turn clears the input line");

        // Bounded so a wedge cannot hang the suite.
        for _ in 0..2000 {
            b.tick();
            if !matches!(b.status(), TurnState::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(b.status(), &TurnState::Completed);
        assert!(b
            .driver()
            .transcript()
            .iter()
            .any(|m| m.content.contains("hello back")));
        assert!(b.title().contains("completed"));
        // A chat pane never feeds the supervision channel, and never self-closes.
        assert_eq!(b.observation(), None);
        assert!(!b.is_closed());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tick_reports_failed_when_the_backend_errors() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let mut b = ChatPaneBackend::new(PaneKind::Companion, &profile(server.uri())).unwrap();
        b.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        b.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        for _ in 0..2000 {
            b.tick();
            if !matches!(b.status(), TurnState::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            matches!(b.status(), TurnState::Failed(_)),
            "an erroring backend must surface as Failed, got {:?}",
            b.status()
        );
    }

    // --- shell panes --------------------------------------------------------

    /// A real pty is a Unix-only fixture here, matching the existing `pty.rs`
    /// integration gating.
    #[cfg(unix)]
    #[test]
    fn a_shell_pane_renders_its_output_observes_it_and_closes_on_exit() {
        let area = Rect::new(0, 0, 40, 10);
        let mut b = PtyPaneBackend::spawn("/bin/sh", area, None).expect("spawn a shell");
        assert_eq!(b.program(), "/bin/sh");
        assert_eq!(b.title(), "sh", "the title is the program's leaf name");

        b.handle_key(KeyCode::Char('e'), KeyModifiers::NONE);
        for c in "cho gilamonster".chars() {
            b.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        b.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        // Bounded poll: the reader thread mirrors the output asynchronously.
        let mut seen = String::new();
        for _ in 0..200 {
            if let Some(chunk) = b.observation() {
                seen.push_str(&chunk);
            }
            if seen.contains("gilamonster") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            seen.contains("gilamonster"),
            "the shell's output must reach the observation drain, got {seen:?}"
        );
        // The same output is on the rendered grid.
        let lines = b.render_lines(40, 10);
        let screen: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(screen.contains("gilamonster"));

        // Resizing is best-effort and must not panic or close the pane.
        b.resize(Rect::new(0, 0, 20, 6));
        assert!(!b.is_closed());

        // `exit` ends the child → the pane reports closed and says so.
        for c in "exit".chars() {
            b.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        b.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let mut closed = false;
        for _ in 0..200 {
            if b.is_closed() {
                closed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(closed, "a shell that exits must report itself closed");
        assert_eq!(b.title(), "sh (exited)");
    }

    // --- failed panes -------------------------------------------------------

    #[test]
    fn a_failed_pane_is_inert_and_shows_the_reason() {
        let mut b = FailedPaneBackend::new("no shell: boom");
        assert_eq!(b.title(), "failed");
        assert_eq!(b.reason(), "no shell: boom");
        let lines = b.render_lines(40, 10);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert_eq!(text, "no shell: boom");
        // Nothing it can do: keys go nowhere, it never closes, it never speaks.
        b.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        b.resize(Rect::new(0, 0, 4, 4));
        b.tick();
        assert_eq!(b.observation(), None);
        assert!(!b.is_closed());
    }

    #[test]
    fn every_backend_is_object_safe_behind_the_trait() {
        // The raw loop stores these as `Box<dyn PaneBackend>` keyed by token;
        // this is the compile-time proof that all three fit that map.
        let panes: Vec<Box<dyn PaneBackend>> =
            vec![Box::new(FailedPaneBackend::new("a")), Box::new(chat())];
        assert_eq!(panes.len(), 2);
    }
}
