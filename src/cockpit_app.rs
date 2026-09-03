//! The cockpit runtime (#54, phase 4 ratchet 3).
//!
//! [`CockpitApp`] is where the pure model meets live resources: a [`Cockpit`]
//! (tabs, layout, focus, no I/O), a `HashMap<PaneToken, Box<dyn PaneBackend>>`
//! (drivers and PTYs), and the prefix [`KeyDispatcher`]. `main.rs` owns only
//! the ratatui frame and the crossterm event source.
//!
//! [`PaneId`](crate::layout::PaneId)s are per-tab and alias across tabs (every
//! tab's first pane is id `0`), so the map is keyed by [`PaneToken`]: globally
//! unique, retired with its pane, never reused. An [`Effect`] is then a
//! one-line insert or remove, and a stale token simply misses.
//!
//! A pane whose driver or shell cannot be created gets a
//! [`FailedPaneBackend`], so the map stays in lockstep with the model and the
//! operator sees the reason rather than losing every other pane to an early
//! return.

use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;

use crate::authority::PaneKind;
use crate::cockpit::{Cockpit, Effect, PaneToken};
use crate::keys::{self, Action, KeyCombo, KeyDispatcher, KeyDisposition};
use crate::pane_backend::{ChatPaneBackend, FailedPaneBackend, PaneBackend, PtyPaneBackend};

pub use crate::pane_backend::BackendProfile;

/// What the raw loop should do with one key.
///
/// The distinction that matters is [`Forward`](AppKey::Forward) vs
/// [`Absorbed`](AppKey::Absorbed): a forwarded key reaches the focused pane's
/// backend (and, for a shell pane, its PTY), while an absorbed one reaches
/// **nothing**. A bare prefix and a post-prefix miss must both be absorbed , 
/// that is the leak guard `cowork::route_key` established, kept here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKey {
    /// Leave the cockpit.
    Quit,
    /// Apply a cockpit [`Action`] to the model.
    Do(Action),
    /// Hand the key to the focused pane's backend.
    Forward,
    /// The key reaches nothing (a bare prefix armed, or a swallowed miss).
    Absorbed,
}

/// Route one key through the prefix `dispatcher`. `Ctrl+Q` (only when a prefix
/// is not armed) quits; a completed prefix binding becomes [`AppKey::Do`]; an
/// unclaimed key is forwarded to the focused pane; a pending prefix or a
/// swallowed post-prefix miss is absorbed. Pure, so the routing is tested off
/// the terminal loop.
#[must_use]
pub fn route_app_key(
    dispatcher: &mut KeyDispatcher,
    combo: KeyCombo,
    now: std::time::Instant,
) -> AppKey {
    if !dispatcher.is_armed() && combo == KeyCombo::ctrl('q') {
        return AppKey::Quit;
    }
    match dispatcher.on_key(combo, now) {
        KeyDisposition::Consumed(action) => AppKey::Do(action),
        KeyDisposition::Forward => AppKey::Forward,
        KeyDisposition::Pending | KeyDisposition::Swallow => AppKey::Absorbed,
    }
}

/// Convert a dispatcher [`KeyCombo`] back into the crossterm key that would
/// have produced it, the inverse of [`cowork::to_key_combo`](crate::cowork::to_key_combo),
/// used to deliver a literal prefix keystroke to a pane. `None` for a combo with
/// no crossterm spelling.
#[must_use]
fn combo_to_crossterm(combo: KeyCombo) -> Option<(KeyCode, KeyModifiers)> {
    let code = match combo.code {
        keys::KeyCode::Char(c) => KeyCode::Char(c),
        keys::KeyCode::F(n) => KeyCode::F(n),
        keys::KeyCode::Left => KeyCode::Left,
        keys::KeyCode::Right => KeyCode::Right,
        keys::KeyCode::Up => KeyCode::Up,
        keys::KeyCode::Down => KeyCode::Down,
        keys::KeyCode::Home => KeyCode::Home,
        keys::KeyCode::End => KeyCode::End,
        keys::KeyCode::PageUp => KeyCode::PageUp,
        keys::KeyCode::PageDown => KeyCode::PageDown,
        keys::KeyCode::Enter => KeyCode::Enter,
        keys::KeyCode::Esc => KeyCode::Esc,
        keys::KeyCode::Tab => KeyCode::Tab,
        keys::KeyCode::Backspace => KeyCode::Backspace,
    };
    let mut mods = KeyModifiers::NONE;
    if combo.mods.ctrl {
        mods |= KeyModifiers::CONTROL;
    }
    if combo.mods.alt {
        mods |= KeyModifiers::ALT;
    }
    if combo.mods.shift {
        mods |= KeyModifiers::SHIFT;
    }
    Some((code, mods))
}

/// The cockpit runtime: the model, the live backends, and the key dispatcher.
pub struct CockpitApp {
    cockpit: Cockpit,
    /// Live resources keyed by the model's globally-unique token.
    backends: HashMap<PaneToken, Box<dyn PaneBackend>>,
    dispatcher: KeyDispatcher,
    /// How every chat pane in this cockpit mints its driver.
    profile: BackendProfile,
    /// The program a shell pane spawns (the human's `$SHELL`).
    shell_program: String,
    /// Where shell panes start.
    cwd: Option<PathBuf>,
    /// The size a freshly-opened pane is spawned at, until the next render
    /// hands it its real rect.
    spawn_area: Rect,
    quit: bool,
}

impl CockpitApp {
    /// Build the runtime and open the initial companion chat pane, the one
    /// the model already created in [`Cockpit::new`], registered under its
    /// [`focused_token`](Cockpit::focused_token).
    #[must_use]
    pub fn new(profile: BackendProfile, shell_program: String, cwd: Option<PathBuf>) -> Self {
        let cockpit = Cockpit::new();
        let token = cockpit.focused_token();
        let mut app = Self {
            cockpit,
            backends: HashMap::new(),
            dispatcher: KeyDispatcher::default(),
            profile,
            shell_program,
            cwd,
            spawn_area: Rect::new(0, 0, 80, 24),
            quit: false,
        };
        let backend = app.make_chat(PaneKind::Companion);
        app.backends.insert(token, backend);
        app
    }

    // ── accessors the raw loop renders from ─────────────────────────────────

    /// The model, for geometry and tab-bar queries.
    pub fn cockpit(&mut self) -> &mut Cockpit {
        &mut self.cockpit
    }

    /// The prefix dispatcher, for routing a key with [`route_app_key`].
    pub fn dispatcher(&mut self) -> &mut KeyDispatcher {
        &mut self.dispatcher
    }

    /// The backend registered under `token`, if any.
    pub fn backend_mut(&mut self, token: PaneToken) -> Option<&mut Box<dyn PaneBackend>> {
        self.backends.get_mut(&token)
    }

    /// How many live backends the runtime holds, always one per live pane.
    #[must_use]
    pub fn backend_count(&self) -> usize {
        self.backends.len()
    }

    /// Whether the loop should exit.
    #[must_use]
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Ask the loop to exit at the next iteration.
    pub fn request_quit(&mut self) {
        self.quit = true;
    }

    /// Remember the area a newly-opened pane should be spawned at (the loop
    /// sets this from the panes region each frame, so a split's pty starts at a
    /// sane size instead of the 80×24 default).
    pub fn set_spawn_area(&mut self, area: Rect) {
        self.spawn_area = area;
    }

    // ── the action → effect → backend bridge ────────────────────────────────

    /// Apply one [`Action`] to the model and perform the [`Effect`] it returns:
    /// attach a backend for a new pane, drop the backend(s) of a closed pane or
    /// tab. Returns the effect so a caller (or a test) can assert on it.
    pub fn apply(&mut self, action: Action) -> Effect {
        // `prefix prefix` types a LITERAL prefix key into the focused pane , 
        // how you reach a nested shell's or tmux's own Ctrl+B. The model has no
        // state to change for it, so it is delivered here as an ordinary key
        // and the pane's backend encodes it (a PTY pane emits 0x02; a chat pane
        // ignores the Ctrl chord). Without this, the binding silently does
        // nothing and a nested multiplexer is unreachable.
        if action == Action::SendPrefix {
            let prefix = self.dispatcher.prefix();
            if let Some((code, mods)) = combo_to_crossterm(prefix) {
                self.handle_key(code, mods);
            }
            return Effect::None;
        }
        let effect = self.cockpit.apply(action);
        match &effect {
            Effect::None => {}
            Effect::OpenChatPane { token, kind, .. } => {
                let backend = self.make_chat(*kind);
                self.backends.insert(*token, backend);
            }
            Effect::OpenShellPane { token, .. } => {
                let backend = self.make_shell();
                self.backends.insert(*token, backend);
            }
            Effect::ClosePane { token, .. } => {
                self.backends.remove(token);
            }
            Effect::CloseTab { panes } => {
                for token in panes {
                    self.backends.remove(token);
                }
            }
        }
        effect
    }

    /// Mint a chat pane clamped to `kind`, or an inert failed pane carrying the
    /// authority refusal. Never mints an unclamped driver.
    fn make_chat(&self, kind: PaneKind) -> Box<dyn PaneBackend> {
        match ChatPaneBackend::new(kind, &self.profile) {
            Ok(b) => Box::new(b),
            Err(e) => Box::new(FailedPaneBackend::new(e.to_string())),
        }
    }

    /// Spawn a shell pane, or an inert failed pane carrying the spawn error.
    fn make_shell(&self) -> Box<dyn PaneBackend> {
        match PtyPaneBackend::spawn(&self.shell_program, self.spawn_area, self.cwd.as_deref()) {
            Ok(b) => Box::new(b),
            Err(e) => Box::new(FailedPaneBackend::new(format!(
                "could not spawn {}: {e}",
                self.shell_program
            ))),
        }
    }

    // ── the per-frame steps ─────────────────────────────────────────────────

    /// Step every backend of the active tab once (non-blocking).
    pub fn tick(&mut self) {
        for (_, token) in self.cockpit.panes() {
            if let Some(b) = self.backends.get_mut(&token) {
                b.tick();
            }
        }
    }

    /// Route a forwarded key to the focused pane's backend.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let token = self.cockpit.focused_token();
        if let Some(b) = self.backends.get_mut(&token) {
            b.handle_key(code, mods);
        }
    }

    /// Close every pane of the active tab whose backend ended on its own (the
    /// human typed `exit`). Closing the tab's last pane closes the tab, and the
    /// cockpit refuses to close the final tab's final pane, in that case there
    /// is nothing left to run, so the app quits.
    pub fn reap_closed(&mut self) {
        let dead: Vec<_> = self
            .cockpit
            .panes()
            .into_iter()
            .filter(|(_, token)| self.backends.get_mut(token).is_some_and(|b| b.is_closed()))
            .collect();
        if dead.is_empty() {
            return;
        }
        // Closing is aimed by focus, and `close_focused` then refocuses an
        // arbitrary survivor, so reaping a *background* pane would yank the
        // operator's focus out of the pane they are typing in (their next
        // keystrokes, and an Enter, would land somewhere else entirely).
        // Remember the operator's pane and restore it when it outlived the reap.
        let operator_token = self.cockpit.focused_token();
        for (pane, _) in dead {
            if !self.cockpit.focus_pane(pane) {
                continue;
            }
            if self.apply(Action::ClosePane) == Effect::None {
                // The last pane of the last tab: nothing survives it.
                self.quit = true;
            }
        }
        if let Some((survivor, _)) = self
            .cockpit
            .panes()
            .into_iter()
            .find(|(_, token)| *token == operator_token)
        {
            self.cockpit.focus_pane(survivor);
        }
    }

    /// Drain each pane's accumulated output, discarding it.
    ///
    /// `SharedScreen` buffers every byte a PTY pane emits until something
    /// drains it, that is the seam supervision (phase 4) will read. Until a
    /// supervisor consumes it, an unread pane running `tail -f` or a build loop
    /// would grow that buffer without bound for the cockpit's lifetime, so the
    /// loop drains and drops it each frame. When supervision lands this is the
    /// one call site that changes: route the chunk into the observation channel
    /// instead of dropping it.
    /// Every backend is drained, not just the active tab's: a background tab's
    /// shell keeps producing output while you are looking elsewhere, which is
    /// exactly the case that would grow unbounded.
    pub fn drain_observations(&mut self) {
        for b in self.backends.values_mut() {
            let _ = b.observation();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cockpit::PaneRole;

    fn profile() -> BackendProfile {
        BackendProfile {
            endpoint: "http://localhost:1".to_string(),
            model: "test-model".to_string(),
            kind: newt_core::BackendKind::Ollama,
            api_key: None,
            workspace: ".".to_string(),
        }
    }

    /// A cockpit whose shell panes spawn a program that exits immediately, so
    /// no test leaves a live interactive shell behind. On non-Unix the spawn
    /// simply fails and the pane becomes a `FailedPaneBackend`, which is the
    /// other invariant these tests care about (a backend always exists).
    fn app() -> CockpitApp {
        CockpitApp::new(profile(), "/bin/echo".to_string(), None)
    }

    #[test]
    fn a_new_app_registers_a_backend_for_the_initial_chat_pane() {
        let mut a = app();
        assert_eq!(a.backend_count(), 1);
        let token = a.cockpit().focused_token();
        assert!(a.backend_mut(token).is_some());
        assert!(!a.should_quit());
    }

    #[test]
    fn opening_and_closing_panes_keeps_one_backend_per_pane() {
        // The load-bearing invariant: the backend map and the model never drift.
        let mut a = app();
        for action in [
            Action::SplitShell,
            Action::NewChatTab,
            Action::SplitShell,
            Action::SplitShell,
        ] {
            a.apply(action);
        }
        assert_eq!(a.backend_count(), 5, "1 initial + 4 opened");

        let closed = a.apply(Action::ClosePane);
        assert!(matches!(closed, Effect::ClosePane { .. }));
        assert_eq!(a.backend_count(), 4, "the closed pane's backend is dropped");

        // Closing the tab drops every backend that tab held (2 panes left).
        let Effect::CloseTab { panes } = a.apply(Action::CloseTab) else {
            panic!("expected CloseTab");
        };
        assert_eq!(panes.len(), 2);
        assert_eq!(a.backend_count(), 2, "only tab 1's panes survive");
        assert_eq!(a.cockpit().tab_count(), 1);
    }

    #[test]
    fn a_split_shell_pane_gets_a_driverless_backend() {
        let mut a = app();
        let eff = a.apply(Action::SplitShell);
        let Effect::OpenShellPane { pane, token } = eff else {
            panic!("expected OpenShellPane, got {eff:?}");
        };
        assert_eq!(a.cockpit().pane_role(pane), Some(PaneRole::Shell));
        assert!(
            a.backend_mut(token).is_some(),
            "a shell pane still gets a backend — it just has no driver"
        );
    }

    #[test]
    fn a_rejected_action_leaves_the_backend_map_alone() {
        let mut a = app();
        // The final tab's final pane cannot be closed.
        assert_eq!(a.apply(Action::ClosePane), Effect::None);
        assert_eq!(a.backend_count(), 1);
        // Nor can the last tab be closed.
        assert_eq!(a.apply(Action::CloseTab), Effect::None);
        assert_eq!(a.backend_count(), 1);
    }

    #[test]
    fn keys_reach_the_focused_pane_and_follow_focus() {
        let mut a = app();
        a.handle_key(KeyCode::Char('h'), KeyModifiers::NONE);
        a.handle_key(KeyCode::Char('i'), KeyModifiers::NONE);
        let chat = a.cockpit().focused_token();
        let title = a.backend_mut(chat).unwrap().title();
        assert!(title.contains("chat"), "the initial pane is a chat pane");
        // Its input line took the keys, visible on the rendered prompt row.
        let lines = a.backend_mut(chat).unwrap().render_lines(40, 4);
        let text: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "▸ hi");

        // After a split, focus (and therefore keys) move to the new pane; the
        // chat pane's buffer is untouched.
        a.apply(Action::SplitShell);
        assert_ne!(a.cockpit().focused_token(), chat);
        a.handle_key(KeyCode::Char('z'), KeyModifiers::NONE);
        let lines = a.backend_mut(chat).unwrap().render_lines(40, 4);
        let text: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "▸ hi", "the unfocused pane saw nothing");
    }

    #[test]
    fn tick_is_safe_with_no_turn_in_flight() {
        let mut a = app();
        a.apply(Action::SplitShell);
        a.tick();
        a.tick();
        assert_eq!(a.backend_count(), 2);
    }

    /// A shell pane whose program exits is reaped on the next pass, and its
    /// backend goes with it.
    #[cfg(unix)]
    #[test]
    fn reap_closes_a_pane_whose_program_exited() {
        let mut a = app(); // shell program is /bin/echo — it exits at once
        a.apply(Action::SplitShell);
        assert_eq!(a.cockpit().pane_count(), 2);

        // Bounded: the child's exit is observed asynchronously.
        for _ in 0..200 {
            a.reap_closed();
            if a.cockpit().pane_count() == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(a.cockpit().pane_count(), 1, "the exited pane was closed");
        assert_eq!(a.backend_count(), 1, "and its backend dropped with it");
        assert!(!a.should_quit(), "the surviving chat pane keeps us running");
    }

    /// Reaping a *background* pane must not move the operator's focus.
    ///
    /// Three panes: the chat, a dead shell, and a second shell the operator is
    /// typing in. Before this was pinned, the reap aimed the close by focus and
    /// `close_focused` refocused an arbitrary survivor, so a shell exiting in
    /// the background silently redirected the operator's next keystrokes (an
    /// Enter would submit a bogus chat turn).
    #[cfg(unix)]
    #[test]
    fn reap_of_a_background_pane_keeps_the_operators_focus() {
        let mut a = app(); // shell program is /bin/echo — it exits at once
        a.apply(Action::SplitShell); // the pane that will die
        a.apply(Action::SplitShell); // the operator's pane (focused, newest)
        assert_eq!(a.cockpit().pane_count(), 3);
        let operator = a.cockpit().focused_token();

        for _ in 0..200 {
            a.reap_closed();
            if a.cockpit().pane_count() < 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        // Whichever shells were reaped, if the operator's pane is one of the
        // survivors it must still be the focused one.
        if a.cockpit()
            .panes()
            .iter()
            .any(|(_, token)| *token == operator)
        {
            assert_eq!(
                a.cockpit().focused_token(),
                operator,
                "a background reap stole the operator's focus"
            );
        }
    }

    /// Draining is what keeps a PTY pane's pending buffer from growing without
    /// bound until supervision (phase 4) consumes it, so the runtime must offer
    /// the drain over every backend, including background tabs' panes.
    #[test]
    fn drain_observations_covers_every_backend_not_just_the_active_tab() {
        let mut a = app();
        a.apply(Action::NewChatTab); // a second tab; tab 1's backend is now inactive
        assert_eq!(a.backend_count(), 2);
        a.drain_observations();
        assert_eq!(
            a.backend_count(),
            2,
            "draining is read-only over the backend map"
        );
    }

    #[test]
    fn reap_never_touches_a_pane_that_is_still_alive() {
        let mut a = app();
        a.reap_closed();
        assert_eq!(a.cockpit().pane_count(), 1);
        assert_eq!(a.backend_count(), 1);
        assert!(!a.should_quit(), "a live chat pane is never reaped");
    }

    #[test]
    fn spawn_area_is_settable_for_the_next_pane() {
        let mut a = app();
        a.set_spawn_area(Rect::new(0, 1, 100, 30));
        a.apply(Action::SplitShell);
        assert_eq!(a.backend_count(), 2);
    }

    #[test]
    fn combo_to_crossterm_round_trips_through_the_cowork_adapter() {
        // The inverse must agree with the adapter the loop uses on the way in,
        // or a literal prefix keystroke would arrive as a different key.
        for (code, mods) in [
            (KeyCode::Char('b'), KeyModifiers::CONTROL),
            (KeyCode::Char('a'), KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::F(4), KeyModifiers::ALT),
        ] {
            let combo = crate::cowork::to_key_combo(code, mods).expect("in the vocabulary");
            let (back_code, back_mods) = combo_to_crossterm(combo).expect("and back out");
            assert_eq!(
                crate::cowork::to_key_combo(back_code, back_mods),
                Some(combo),
                "{code:?}/{mods:?} did not round-trip"
            );
        }
    }

    /// `prefix prefix` must reach the focused pane as a literal keystroke , 
    /// that is the only way to type a real Ctrl+B into a nested shell or tmux.
    /// A chat pane, by contrast, treats the Ctrl chord as a control key and
    /// types nothing.
    #[test]
    fn send_prefix_delivers_a_literal_keystroke_to_the_focused_pane() {
        let mut a = app();
        let chat = a.cockpit().focused_token();
        a.handle_key(KeyCode::Char('h'), KeyModifiers::NONE);

        assert_eq!(a.apply(Action::SendPrefix), Effect::None, "no model change");

        let lines = a.backend_mut(chat).unwrap().render_lines(40, 4);
        let text: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "▸ h", "a chat pane never types the prefix chord");
        assert_eq!(a.backend_count(), 1, "and no pane was opened or closed");
    }

    // --- key routing --------------------------------------------------------

    #[test]
    fn ctrl_q_quits_only_when_no_prefix_is_armed() {
        let mut d = KeyDispatcher::default();
        let now = std::time::Instant::now();
        assert_eq!(
            route_app_key(&mut d, KeyCombo::ctrl('q'), now),
            AppKey::Quit
        );
        // Armed: Ctrl+Q is a post-prefix key, not the global quit.
        assert_eq!(
            route_app_key(&mut d, KeyCombo::ctrl('b'), now),
            AppKey::Absorbed
        );
        assert_ne!(
            route_app_key(&mut d, KeyCombo::ctrl('q'), now),
            AppKey::Quit
        );
    }

    #[test]
    fn a_prefix_binding_becomes_an_action_and_ordinary_keys_forward() {
        let mut d = KeyDispatcher::default();
        let now = std::time::Instant::now();
        assert_eq!(
            route_app_key(&mut d, KeyCombo::ctrl('b'), now),
            AppKey::Absorbed,
            "the bare prefix reaches nothing"
        );
        assert_eq!(
            route_app_key(&mut d, KeyCombo::char('c'), now),
            AppKey::Do(Action::NewChatTab)
        );
        // An unbound ordinary key is the focused pane's business.
        assert_eq!(
            route_app_key(&mut d, KeyCombo::char('c'), now),
            AppKey::Forward
        );
    }

    #[test]
    fn a_post_prefix_miss_is_absorbed_not_forwarded() {
        // THE leak guard: after a prefix, an unbound key must reach neither the
        // model nor the pane, otherwise it lands in the human's shell.
        let mut d = KeyDispatcher::default();
        let now = std::time::Instant::now();
        let _ = route_app_key(&mut d, KeyCombo::ctrl('b'), now);
        assert_eq!(
            route_app_key(&mut d, KeyCombo::char('\u{1}'), now),
            AppKey::Absorbed
        );
    }
}
