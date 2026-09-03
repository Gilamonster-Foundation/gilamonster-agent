//! The cockpit state model (#54, cockpit design phase 4, ratchet 2).
//!
//! `Cockpit` is the **pure** heart of the tmux-semantics multiplexer: tabs, each
//! holding a [`LayoutTree`](crate::layout::LayoutTree) of panes, a focused pane,
//! and per-pane [`role`](PaneRole). It composes the three modules the earlier
//! v0.3.x ratchets built, [`keys`](crate::keys) (the prefix dispatcher produces
//! [`Action`]s), [`layout`](crate::layout) (pane geometry), and
//! [`authority`](crate::authority) (a chat pane's caveat posture), with **no**
//! terminal I/O and **no** live drivers, so the whole state machine is
//! unit-tested. The raw render loop (ratchet 3) owns the ratatui frame, the
//! `TurnDriver`s, and the PTYs; it drives this model with `apply` and performs
//! the returned [`Effect`]s.
//!
//! # The observe-only guarantee, at the model layer
//!
//! A [`PaneRole::Shell`] pane carries **no** authority posture, it is
//! structurally not a chat pane, so this model can never mint a driver for it
//! (only [`Effect::OpenChatPane`] carries a [`PaneKind`], and it is only ever
//! produced for [`PaneRole::Chat`] panes). The write-side structural proof (a
//! non-`Clone` `PtyWriter`) and the behavioral regression test land with the
//! real PTY in ratchet 3; this is the first of the design's "three ways".

use std::collections::HashMap;

use crate::authority::PaneKind;
use crate::keys::{Action, Dir as KeyDir};
use crate::layout::{Axis, Dir, LayoutTree, PaneId, Rect};

/// What a pane *is*. Only a [`Chat`](PaneRole::Chat) pane has a driver (and thus
/// an authority posture); a [`Shell`](PaneRole::Shell) pane is the user's real
/// `$SHELL`, observed but never driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneRole {
    /// A chat pane driven by a `TurnDriver`, clamped by its [`PaneKind`].
    Chat(PaneKind),
    /// The ambient shell pane, no driver, observe-only.
    Shell,
}

/// A **globally unique** pane identity, allocated once per pane for the life of
/// the cockpit. [`PaneId`]s are per-tab layout-tree ids and collide across tabs
/// (every tab's first pane is id `0`); the raw loop keys its live resources
/// (drivers, PTYs) by token so two tabs' panes can never alias each other's
/// backend.
pub type PaneToken = u64;

/// One tab: a [`LayoutTree`] of panes plus per-pane roles, tokens, and the
/// focused pane.
#[derive(Debug, Clone)]
struct Tab {
    title: String,
    layout: LayoutTree,
    roles: HashMap<PaneId, PaneRole>,
    tokens: HashMap<PaneId, PaneToken>,
    focused: PaneId,
}

impl Tab {
    /// A fresh tab containing a single companion chat pane (id `0`), with the
    /// given globally-unique token for that pane.
    fn new(title: impl Into<String>, first_token: PaneToken) -> Self {
        let layout = LayoutTree::single();
        let root = layout.panes()[0];
        let mut roles = HashMap::new();
        roles.insert(root, PaneRole::Chat(PaneKind::Companion));
        let mut tokens = HashMap::new();
        tokens.insert(root, first_token);
        Self {
            title: title.into(),
            layout,
            roles,
            tokens,
            focused: root,
        }
    }
}

/// A side effect the raw render loop must perform after an [`apply`](Cockpit::apply).
/// The model has already updated its own state; the effect tells the loop what
/// live resource to attach or drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Nothing to do (focus/tab bookkeeping only, or a rejected action).
    None,
    /// A chat pane was created: the loop mints a `TurnDriver` via
    /// [`authority::driver_config`](crate::authority::driver_config) clamped to
    /// `kind`. This is the **only** effect that carries a [`PaneKind`]. The
    /// loop registers the backend under `token` (globally unique across tabs).
    OpenChatPane {
        pane: PaneId,
        token: PaneToken,
        kind: PaneKind,
    },
    /// An ambient shell pane was created: the loop spawns a `PtyShell` and wires
    /// its observe-only read tap. No driver, no write handle for the agent.
    OpenShellPane { pane: PaneId, token: PaneToken },
    /// A pane was closed: the loop drops that pane's driver or PTY.
    ClosePane { pane: PaneId, token: PaneToken },
    /// The whole active tab closed: the loop drops every resource it held.
    CloseTab { panes: Vec<PaneToken> },
}

/// The cockpit: an ordered set of tabs with one active. Construct with
/// [`new`](Cockpit::new), feed it [`Action`]s with [`apply`](Cockpit::apply),
/// and read pane geometry with [`rects`](Cockpit::rects).
#[derive(Debug, Clone)]
pub struct Cockpit {
    tabs: Vec<Tab>,
    active: usize,
    /// The last terminal size seen (via [`rects`](Cockpit::rects)); structural
    /// ops re-fit the layout to it first so a `split`/`close` has real extents
    /// to work with even before the first render. `rects` re-fits to the true
    /// terminal each frame, so this need only be a reasonable default.
    term: Rect,
    /// The next [`PaneToken`] to hand out. Monotonic and never reused: a closed
    /// pane's token is retired with it, so a stale backend registration can
    /// never be resurrected by a later pane that happens to reuse its
    /// per-tab [`PaneId`].
    next_token: PaneToken,
}

/// The size a fresh cockpit assumes until the first [`rects`](Cockpit::rects)
/// tells it the real terminal, big enough that early splits are not rejected.
const DEFAULT_TERM: Rect = Rect {
    x: 0,
    y: 0,
    w: 80,
    h: 24,
};

impl Default for Cockpit {
    fn default() -> Self {
        Self::new()
    }
}

impl Cockpit {
    /// A cockpit with one tab holding a single companion chat pane.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tabs: vec![Tab::new("1", 0)],
            active: 0,
            term: DEFAULT_TERM,
            next_token: 1,
        }
    }

    /// Hand out the next globally-unique [`PaneToken`]. The counter only ever
    /// goes up, so tokens are unique for the life of the cockpit.
    fn alloc_token(&mut self) -> PaneToken {
        let t = self.next_token;
        self.next_token += 1;
        t
    }

    /// Re-fit the active tab's layout to the last-known terminal size, so a
    /// structural op (split/close/nav) works on real extents.
    fn fit_active(&mut self) {
        let (w, h) = (self.term.w, self.term.h);
        self.tabs[self.active].layout.resize_tree(w, h);
    }

    // ── queries ─────────────────────────────────────────────────────────────

    /// The number of tabs.
    #[must_use]
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// The active tab's index.
    #[must_use]
    pub fn active_tab(&self) -> usize {
        self.active
    }

    /// The tab titles, in order.
    #[must_use]
    pub fn tab_titles(&self) -> Vec<String> {
        self.tabs.iter().map(|t| t.title.clone()).collect()
    }

    /// The focused pane of the active tab.
    #[must_use]
    pub fn focused_pane(&self) -> PaneId {
        self.tabs[self.active].focused
    }

    /// The role of a pane in the active tab, if it exists.
    #[must_use]
    pub fn pane_role(&self, pane: PaneId) -> Option<PaneRole> {
        self.tabs[self.active].roles.get(&pane).copied()
    }

    /// The [`PaneToken`] of a pane in the active tab, if it exists. The raw
    /// loop keys its live backends by this, never by [`PaneId`].
    #[must_use]
    pub fn pane_token(&self, pane: PaneId) -> Option<PaneToken> {
        self.tabs[self.active].tokens.get(&pane).copied()
    }

    /// The [`PaneToken`] of the focused pane. Every live pane has a token by
    /// construction (allocated with the pane), so this is total.
    #[must_use]
    pub fn focused_token(&self) -> PaneToken {
        let tab = &self.tabs[self.active];
        tab.tokens[&tab.focused]
    }

    /// Every pane of the active tab as `(id, token)`, in layout order. The raw
    /// loop walks this to tick and reap its backends.
    #[must_use]
    pub fn panes(&self) -> Vec<(PaneId, PaneToken)> {
        let tab = &self.tabs[self.active];
        tab.layout
            .panes()
            .into_iter()
            .filter_map(|p| tab.tokens.get(&p).map(|&t| (p, t)))
            .collect()
    }

    /// Focus `pane` in the active tab. Returns whether it exists (an unknown
    /// pane leaves focus untouched). The loop uses this to aim a close at a
    /// pane whose backend ended on its own.
    pub fn focus_pane(&mut self, pane: PaneId) -> bool {
        if !self.tabs[self.active].roles.contains_key(&pane) {
            return false;
        }
        self.tabs[self.active].focused = pane;
        self.touch_focused();
        true
    }

    /// The number of panes in the active tab.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.tabs[self.active].roles.len()
    }

    /// The pane rectangles of the active tab within `term` (delegates to the
    /// tab's [`LayoutTree`]). The loop renders each pane by its [`pane_role`].
    ///
    /// [`pane_role`]: Cockpit::pane_role
    #[must_use]
    pub fn rects(&mut self, term: Rect) -> Vec<(PaneId, Rect)> {
        self.term = term;
        let tab = &mut self.tabs[self.active];
        tab.layout.resize_tree(term.w, term.h);
        tab.layout.rects(term)
    }

    // ── the action state machine ────────────────────────────────────────────

    /// Apply one dispatched [`Action`], mutating the model and returning the
    /// [`Effect`] the raw loop must perform. Pure: no I/O, no drivers.
    pub fn apply(&mut self, action: Action) -> Effect {
        match action {
            Action::NewChatTab => self.new_chat_tab(),
            Action::SplitShell => self.split(Axis::Vertical, PaneRole::Shell),
            Action::NextTab => self.select_tab_rel(1),
            Action::PrevTab => self.select_tab_rel(-1),
            Action::LastTab => self.select_tab_rel(0), // no MRU yet; stay put
            Action::SelectTab(n) => self.select_tab_abs(n as usize),
            Action::FocusDir(dir) => self.focus_dir(dir),
            Action::FocusNext => self.focus_cycle(),
            Action::Zoom => {
                let tab = &mut self.tabs[self.active];
                tab.layout.toggle_zoom(tab.focused);
                Effect::None
            }
            Action::ClosePane => self.close_focused(),
            Action::CloseTab => self.close_tab(),
            // Actions with no model effect yet (rendered/handled elsewhere or a
            // later ratchet): copy mode, follow toggle, resize, etc.
            _ => Effect::None,
        }
    }

    /// `touch` the focused pane so a future directional-nav tie breaks toward
    /// the most-recently-focused neighbour (mirrors `LayoutTree`'s MRU).
    fn touch_focused(&mut self) {
        let tab = &mut self.tabs[self.active];
        let f = tab.focused;
        tab.layout.touch(f);
    }

    fn new_chat_tab(&mut self) -> Effect {
        let title = (self.tabs.len() + 1).to_string();
        let token = self.alloc_token();
        let tab = Tab::new(title, token);
        let pane = tab.focused;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        Effect::OpenChatPane {
            pane,
            token,
            kind: PaneKind::Companion,
        }
    }

    fn split(&mut self, axis: Axis, role: PaneRole) -> Effect {
        self.fit_active();
        // Allocate before the split borrows the tab; a rejected split simply
        // retires the token unused (they are cheap and never reused anyway).
        let token = self.alloc_token();
        let tab = &mut self.tabs[self.active];
        let Some(new_pane) = tab.layout.split(tab.focused, axis) else {
            return Effect::None; // too small to split
        };
        tab.roles.insert(new_pane, role);
        tab.tokens.insert(new_pane, token);
        tab.focused = new_pane;
        tab.layout.touch(new_pane);
        match role {
            PaneRole::Chat(kind) => Effect::OpenChatPane {
                pane: new_pane,
                token,
                kind,
            },
            PaneRole::Shell => Effect::OpenShellPane {
                pane: new_pane,
                token,
            },
        }
    }

    fn select_tab_rel(&mut self, delta: isize) -> Effect {
        let n = self.tabs.len() as isize;
        if n <= 1 || delta == 0 {
            return Effect::None;
        }
        self.active = (((self.active as isize + delta) % n + n) % n) as usize;
        Effect::None
    }

    fn select_tab_abs(&mut self, n: usize) -> Effect {
        // tmux is 1-indexed on the keys `1`..`9`; key `0` selects tab 10.
        let idx = if n == 0 { 9 } else { n - 1 };
        if idx < self.tabs.len() {
            self.active = idx;
        }
        Effect::None
    }

    fn focus_dir(&mut self, dir: KeyDir) -> Effect {
        let ldir = map_dir(dir);
        self.fit_active();
        let term = self.term;
        let tab = &mut self.tabs[self.active];
        // Same rule as `focus_cycle`: a zoomed tab renders one pane, so
        // directional nav must not move focus to a hidden neighbour.
        if tab.layout.is_zoomed() {
            return Effect::None;
        }
        if let Some(nb) = tab.layout.neighbour(tab.focused, ldir, term) {
            tab.focused = nb;
            self.touch_focused();
        }
        Effect::None
    }

    fn focus_cycle(&mut self) -> Effect {
        let tab = &mut self.tabs[self.active];
        // While a pane is zoomed only that pane is rendered, so moving focus
        // would aim every forwarded keystroke at a pane the operator cannot
        // see, shell commands typed blind into an invisible PTY. Focus stays
        // put until the zoom is toggled off.
        if tab.layout.is_zoomed() {
            return Effect::None;
        }
        let panes = tab.layout.panes();
        if let Some(pos) = panes.iter().position(|&p| p == tab.focused) {
            tab.focused = panes[(pos + 1) % panes.len()];
            self.touch_focused();
        }
        Effect::None
    }

    fn close_focused(&mut self) -> Effect {
        self.fit_active();
        let tab = &mut self.tabs[self.active];
        let victim = tab.focused;
        if tab.layout.close(victim) {
            tab.roles.remove(&victim);
            // The token retires with the pane, so the loop drops exactly the
            // backend this pane owned, no other pane can ever claim it.
            let token = tab.tokens.remove(&victim).unwrap_or_default();
            // Refocus an arbitrary surviving pane.
            tab.focused = tab.layout.panes()[0];
            Effect::ClosePane {
                pane: victim,
                token,
            }
        } else {
            // The last pane in the tab, closing it closes the whole tab.
            self.close_tab()
        }
    }

    fn close_tab(&mut self) -> Effect {
        if self.tabs.len() <= 1 {
            return Effect::None; // never close the last tab
        }
        let tab = self.tabs.remove(self.active);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        Effect::CloseTab {
            panes: tab.tokens.values().copied().collect(),
        }
    }
}

/// Map a [`keys::Dir`](crate::keys::Dir) to a [`layout::Dir`](crate::layout::Dir).
fn map_dir(d: KeyDir) -> Dir {
    match d {
        KeyDir::Left => Dir::Left,
        KeyDir::Right => Dir::Right,
        KeyDir::Up => Dir::Up,
        KeyDir::Down => Dir::Down,
    }
}

// ── rendering + key routing (the raw loop's testable helpers) ───────────────

use crate::keys::{KeyCombo, KeyDispatcher, KeyDisposition};

/// What the cockpit raw loop should do with one key. Pure so the routing is
/// tested off the terminal loop (mirrors `cowork`'s `route_key`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitKey {
    /// Perform a cockpit [`Action`] (`apply` it to the model).
    Do(Action),
    /// Quit the cockpit.
    Quit,
    /// Nothing (a bare prefix armed, a swallowed miss, or an unbound key).
    Ignore,
}

/// Route a key through the prefix `dispatcher`. `Ctrl+Q` (in the Root table)
/// quits; a completed prefix binding becomes [`CockpitKey::Do`]; a pending
/// prefix or a swallowed miss is [`CockpitKey::Ignore`]. Pure.
#[must_use]
pub fn route_cockpit_key(
    dispatcher: &mut KeyDispatcher,
    combo: KeyCombo,
    now: std::time::Instant,
) -> CockpitKey {
    // Ctrl+Q is the one direct global (only when not mid-prefix), matching cowork.
    if !dispatcher.is_armed() && combo == KeyCombo::ctrl('q') {
        return CockpitKey::Quit;
    }
    match dispatcher.on_key(combo, now) {
        KeyDisposition::Consumed(action) => CockpitKey::Do(action),
        KeyDisposition::Pending | KeyDisposition::Swallow | KeyDisposition::Forward => {
            CockpitKey::Ignore
        }
    }
}

/// The tab-bar line for the status row: each tab as `n:title`, the active one
/// marked with a leading `▸` and shown first-class. Pure and testable.
#[must_use]
pub fn tab_bar(titles: &[String], active: usize) -> String {
    titles
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mark = if i == active { "▸" } else { " " };
            format!("{mark}{}:{t}", i + 1)
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// The placeholder label a pane shows until live content (transcript / shell
/// grid) is wired in the next ratchet. Pure.
#[must_use]
pub fn pane_label(role: PaneRole, id: PaneId, focused: bool) -> String {
    let what = match role {
        PaneRole::Chat(PaneKind::Companion) => "companion chat",
        PaneRole::Chat(PaneKind::Reader) => "reader chat",
        PaneRole::Chat(PaneKind::Workbench) => "workbench chat",
        PaneRole::Shell => "shell",
    };
    let focus = if focused { " ●" } else { "" };
    format!("{what} [{id}]{focus}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERM: Rect = Rect {
        x: 0,
        y: 0,
        w: 120,
        h: 40,
    };

    #[test]
    fn new_cockpit_is_one_companion_pane_in_one_tab() {
        let c = Cockpit::new();
        assert_eq!(c.tab_count(), 1);
        assert_eq!(c.pane_count(), 1);
        assert_eq!(
            c.pane_role(c.focused_pane()),
            Some(PaneRole::Chat(PaneKind::Companion))
        );
    }

    #[test]
    fn new_chat_tab_adds_a_companion_and_switches_to_it() {
        let mut c = Cockpit::new();
        let eff = c.apply(Action::NewChatTab);
        assert_eq!(c.tab_count(), 2);
        assert_eq!(c.active_tab(), 1);
        // The effect the loop acts on carries the Companion clamp, the only
        // authority a new chat pane can be minted with.
        match eff {
            Effect::OpenChatPane { kind, token, .. } => {
                assert_eq!(kind, PaneKind::Companion);
                // The loop registers the new backend under this token, and the
                // model agrees it is the focused pane's.
                assert_eq!(token, c.focused_token());
            }
            other => panic!("expected OpenChatPane, got {other:?}"),
        }
        assert_eq!(
            c.pane_role(c.focused_pane()),
            Some(PaneRole::Chat(PaneKind::Companion))
        );
    }

    #[test]
    fn split_shell_adds_a_driverless_shell_pane() {
        let mut c = Cockpit::new();
        let eff = c.apply(Action::SplitShell);
        assert_eq!(c.pane_count(), 2);
        let shell = c.focused_pane();
        // A shell pane has NO authority posture, the model cannot mint a driver
        // for it (only OpenChatPane carries a PaneKind).
        assert_eq!(c.pane_role(shell), Some(PaneRole::Shell));
        assert_eq!(
            eff,
            Effect::OpenShellPane {
                pane: shell,
                token: c.focused_token(),
            }
        );
    }

    #[test]
    fn only_chat_effects_carry_authority() {
        // Across a mix of opens, every PaneKind-bearing effect is for a Chat
        // pane; a shell never yields a driver. This is the model-layer half of
        // the observe-only guarantee.
        let mut c = Cockpit::new();
        for action in [Action::NewChatTab, Action::SplitShell, Action::NewChatTab] {
            let eff = c.apply(action);
            if let Effect::OpenChatPane { pane, .. } = eff {
                assert!(matches!(c.pane_role(pane), Some(PaneRole::Chat(_))));
            }
            if let Effect::OpenShellPane { pane, .. } = eff {
                assert_eq!(c.pane_role(pane), Some(PaneRole::Shell));
            }
        }
    }

    #[test]
    fn tab_navigation_wraps_and_selects() {
        let mut c = Cockpit::new();
        c.apply(Action::NewChatTab); // tab 2 (active)
        c.apply(Action::NewChatTab); // tab 3 (active)
        assert_eq!(c.active_tab(), 2);
        c.apply(Action::NextTab);
        assert_eq!(c.active_tab(), 0, "next wraps 3→1");
        c.apply(Action::PrevTab);
        assert_eq!(c.active_tab(), 2, "prev wraps 1→3");
        c.apply(Action::SelectTab(2));
        assert_eq!(c.active_tab(), 1, "key `2` = tab index 1");
        c.apply(Action::SelectTab(9)); // only 3 tabs → out of range, no-op
        assert_eq!(c.active_tab(), 1);
    }

    #[test]
    fn directional_focus_crosses_a_split() {
        let mut c = Cockpit::new();
        let base = c.focused_pane();
        // Split makes a shell below; focus is on the new (bottom) pane.
        c.apply(Action::SplitShell);
        let shell = c.focused_pane();
        assert_ne!(shell, base);
        c.apply(Action::FocusDir(KeyDir::Up));
        assert_eq!(c.focused_pane(), base, "up returns to the original pane");
        c.apply(Action::FocusDir(KeyDir::Down));
        assert_eq!(c.focused_pane(), shell);
    }

    #[test]
    fn zoom_toggles_without_changing_pane_set() {
        let mut c = Cockpit::new();
        c.apply(Action::SplitShell);
        assert_eq!(c.pane_count(), 2);
        c.apply(Action::Zoom);
        // Zoom is a render concern; the pane set is unchanged. rects returns the
        // single zoomed pane.
        assert_eq!(c.pane_count(), 2);
        let mut c2 = c.clone();
        assert_eq!(c2.rects(TERM).len(), 1, "zoomed: one pane fills the term");
        c.apply(Action::Zoom);
        let mut c3 = c.clone();
        assert_eq!(c3.rects(TERM).len(), 2, "unzoomed: both panes");
    }

    /// While a pane is zoomed only that pane is drawn, so focus must not move
    /// to one the operator cannot see: every forwarded keystroke would go into
    /// an invisible PTY (shell commands typed blind).
    #[test]
    fn focus_cannot_leave_a_zoomed_pane() {
        let mut c = Cockpit::new();
        c.apply(Action::SplitShell);
        let zoomed = c.focused_pane();
        c.apply(Action::Zoom);
        let mut probe = c.clone();
        assert_eq!(probe.rects(TERM).len(), 1, "zoomed: one pane is rendered");

        for action in [
            Action::FocusNext,
            Action::FocusDir(KeyDir::Up),
            Action::FocusDir(KeyDir::Down),
            Action::FocusDir(KeyDir::Left),
            Action::FocusDir(KeyDir::Right),
        ] {
            c.apply(action);
            assert_eq!(
                c.focused_pane(),
                zoomed,
                "{action:?} moved focus off the only visible pane"
            );
        }

        // Unzooming restores ordinary navigation.
        c.apply(Action::Zoom);
        c.apply(Action::FocusNext);
        assert_ne!(c.focused_pane(), zoomed, "focus moves again once unzoomed");
    }

    #[test]
    fn close_pane_then_close_tab() {
        let mut c = Cockpit::new();
        c.apply(Action::NewChatTab); // now 2 tabs, active = tab 2 (1 pane)
        c.apply(Action::SplitShell); // tab 2 now has 2 panes
        assert_eq!(c.pane_count(), 2);
        let eff = c.apply(Action::ClosePane);
        assert!(matches!(eff, Effect::ClosePane { .. }));
        assert_eq!(c.pane_count(), 1);
        // Closing the last pane of the tab closes the tab (2 tabs → 1).
        let eff = c.apply(Action::ClosePane);
        assert!(matches!(eff, Effect::CloseTab { .. }));
        assert_eq!(c.tab_count(), 1);
        // The final tab's last pane cannot be closed.
        let eff = c.apply(Action::ClosePane);
        assert_eq!(eff, Effect::None);
        assert_eq!(c.tab_count(), 1);
        assert_eq!(c.pane_count(), 1);
    }

    #[test]
    fn tab_bar_marks_the_active_tab() {
        let titles = vec!["1".to_string(), "2".to_string(), "3".to_string()];
        let bar = tab_bar(&titles, 1);
        assert_eq!(bar, " 1:1  ▸2:2   3:3");
    }

    #[test]
    fn pane_label_names_role_and_focus() {
        assert_eq!(
            pane_label(PaneRole::Chat(PaneKind::Companion), 0, true),
            "companion chat [0] ●"
        );
        assert_eq!(pane_label(PaneRole::Shell, 2, false), "shell [2]");
    }

    #[test]
    fn route_ctrl_q_quits_and_prefix_binding_acts() {
        use crate::keys::{KeyCombo, KeyDispatcher};
        let mut d = KeyDispatcher::default();
        let now = std::time::Instant::now();
        // Ctrl+Q from Root quits.
        assert_eq!(
            route_cockpit_key(&mut d, KeyCombo::ctrl('q'), now),
            CockpitKey::Quit
        );
        // Prefix then `c` → NewChatTab; the bare prefix itself is ignored.
        assert_eq!(
            route_cockpit_key(&mut d, KeyCombo::ctrl('b'), now),
            CockpitKey::Ignore
        );
        assert_eq!(
            route_cockpit_key(&mut d, KeyCombo::char('c'), now),
            CockpitKey::Do(Action::NewChatTab)
        );
    }

    #[test]
    fn tokens_are_unique_across_tabs_and_never_reused() {
        // Every tab's first pane is PaneId 0, so ids alias across tabs; tokens
        // must not, that aliasing is exactly what would let one tab's pane
        // drive another tab's backend.
        let mut c = Cockpit::new();
        let mut seen = vec![c.focused_token()];
        for action in [
            Action::SplitShell,
            Action::NewChatTab,
            Action::SplitShell,
            Action::NewChatTab,
        ] {
            c.apply(action);
            seen.push(c.focused_token());
        }
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "tokens alias: {seen:?}");

        // A closed pane's token retires with it and is not handed out again.
        // (The last tab opened holds a single pane, so split first, closing a
        // tab's only pane closes the whole tab instead.)
        c.apply(Action::SplitShell);
        let retired = c.focused_token();
        let eff = c.apply(Action::ClosePane);
        assert_eq!(
            eff,
            Effect::ClosePane {
                pane: 1,
                token: retired
            }
        );
        c.apply(Action::SplitShell);
        assert_ne!(c.focused_token(), retired, "a retired token was reused");
    }

    #[test]
    fn panes_lists_every_pane_with_its_token_and_focus_is_aimable() {
        let mut c = Cockpit::new();
        let base = c.focused_pane();
        c.apply(Action::SplitShell);
        let shell = c.focused_pane();
        let panes = c.panes();
        assert_eq!(panes.len(), 2);
        for (id, token) in &panes {
            assert_eq!(c.pane_token(*id), Some(*token));
        }
        assert!(c.focus_pane(base), "an existing pane can be focused");
        assert_eq!(c.focused_pane(), base);
        assert!(!c.focus_pane(999), "an unknown pane is rejected");
        assert_eq!(c.focused_pane(), base, "and leaves focus untouched");
        assert!(c.focus_pane(shell));
    }

    #[test]
    fn pane_token_is_none_for_an_unknown_pane() {
        let c = Cockpit::new();
        assert_eq!(c.pane_token(c.focused_pane()), Some(c.focused_token()));
        assert_eq!(c.pane_token(999), None);
    }

    #[test]
    fn close_tab_reports_every_token_the_tab_held() {
        let mut c = Cockpit::new();
        c.apply(Action::NewChatTab);
        let chat = c.focused_token();
        c.apply(Action::SplitShell);
        let shell = c.focused_token();
        let Effect::CloseTab { mut panes } = c.apply(Action::CloseTab) else {
            panic!("expected CloseTab");
        };
        panes.sort_unstable();
        let mut expected = vec![chat, shell];
        expected.sort_unstable();
        assert_eq!(panes, expected, "the loop must drop BOTH backends");
    }

    #[test]
    fn rects_cover_every_pane_without_overlap() {
        let mut c = Cockpit::new();
        c.apply(Action::SplitShell);
        c.apply(Action::NewChatTab);
        c.apply(Action::SplitShell);
        let rects = c.rects(TERM);
        assert_eq!(rects.len(), c.pane_count());
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let (a, b) = (rects[i].1, rects[j].1);
                let disjoint =
                    a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(disjoint, "panes overlap: {a:?} {b:?}");
            }
        }
    }
}
