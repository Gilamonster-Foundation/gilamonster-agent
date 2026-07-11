//! tmux prefix-table key dispatcher (#43 — cockpit design phase 1).
//!
//! Ports tmux's `server_client_key_callback` state machine as a **pure**
//! module: it imports neither `newt-*` nor `ratatui`/`crossterm` (it owns
//! [`KeyCombo`]; a thin adapter converts at the boundary), so a future
//! `gila-mux-core` extraction stays mechanical.
//!
//! The dispatcher *is* the authority router (see
//! `docs/decisions/cockpit_tmux_multiplexer.md`): every keystroke is routed
//! here first, and the single most important safety rule is
//! **swallow-on-fallback-miss** — a post-prefix typo never reaches the shell
//! PTY. Bindings can only name [`Action`]s, never shell strings, so rebinding
//! can never mint authority.
//!
//! Ported tmux rules (design §"tmux key semantics"):
//! - The prefix is **config**, compared before any table lookup — not a Root
//!   binding. Matching it switches to the Prefix table and consumes the key.
//! - **Normalization**: fold `Ctrl+<uppercase letter>` to lowercase and strip
//!   `SHIFT` from printable `Char` keys (crossterm reports `"` as
//!   `Char('"')+SHIFT` on some terminals and `Char('B')+SHIFT+CTRL` on
//!   others). Key-release events are dropped at the adapter, before [`KeyCombo`]
//!   exists.
//! - A **non-repeat** match executes and resets to Root. A **`-r` (repeat)**
//!   match executes and *stays* in the Prefix table with a 500 ms lazy
//!   deadline (checked on the next key, not by a timer); a non-repeat key
//!   during repeat re-resolves in Root.
//! - A Prefix-table miss falls back to Root; a miss **after** fallback is
//!   swallowed.
//! - `prefix prefix` (e.g. `Ctrl+B Ctrl+B`) is bound to [`Action::SendPrefix`]
//!   at table-build time — how you type a literal prefix into a nested tmux.
//! - Paste bypasses the dispatcher entirely (the adapter routes
//!   `Event::Paste` straight to the focused pane; nothing here sees it).

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

/// How long a `-r` (repeat) binding holds the Prefix table open, checked
/// lazily on the next keystroke (tmux's `repeat-time` default).
pub const REPEAT_TIMEOUT: Duration = Duration::from_millis(500);

/// A key, in the dispatcher's own vocabulary (no crossterm types).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    F(u8),
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Esc,
    Tab,
    Backspace,
}

/// Modifier state. `shift` on a printable [`KeyCode::Char`] is stripped by
/// normalization — the char itself already carries the case/symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// A normalized key + modifiers combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub mods: Mods,
}

impl KeyCombo {
    /// A bare printable key.
    #[must_use]
    pub const fn char(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            mods: Mods {
                ctrl: false,
                alt: false,
                shift: false,
            },
        }
    }

    /// `Ctrl+<c>`.
    #[must_use]
    pub const fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            mods: Mods {
                ctrl: true,
                alt: false,
                shift: false,
            },
        }
    }

    /// A bare non-character key (arrows etc.).
    #[must_use]
    pub const fn code(code: KeyCode) -> Self {
        Self {
            code,
            mods: Mods {
                ctrl: false,
                alt: false,
                shift: false,
            },
        }
    }

    /// Normalize the crossterm variance traps (design §normalization): fold
    /// `Ctrl+uppercase` to lowercase; strip `SHIFT` from printable chars.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        if let KeyCode::Char(c) = self.code {
            if self.mods.ctrl && c.is_ascii_uppercase() {
                self.code = KeyCode::Char(c.to_ascii_lowercase());
            }
            self.mods.shift = false;
        }
        self
    }
}

/// A pane-relative direction (focus / resize).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// What a binding does. **Closed set** — bindings can only name these, never
/// shell strings, so rebinding never mints authority. Consumers (the cockpit
/// event loop, phase 2+) translate them; unbound-in-v1 semantics (`%`) simply
/// have no member here yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `c` — new chat tab (companion pane).
    NewChatTab,
    /// `"` — split an ambient shell pane.
    SplitShell,
    /// `n` / `p` / `l` — tab navigation.
    NextTab,
    PrevTab,
    LastTab,
    /// `0`-`9` — select tab by index.
    SelectTab(u8),
    /// `o` / `;` — pane focus cycling.
    FocusNext,
    FocusLast,
    /// `x` / `&` — close pane / tab (the app confirms).
    ClosePane,
    CloseTab,
    /// `z` — zoom toggle.
    Zoom,
    /// `d` — save-and-exit (design phase 9 wires the confirm).
    Detach,
    /// `[` — copy mode (design phase 8).
    CopyMode,
    /// `w` — choose-tree picker.
    ChooseTree,
    /// `q` — display pane numbers.
    DisplayPanes,
    /// `f` — follow-me toggle (human-minted tap).
    FollowToggle,
    /// `u` — open the last surfaced URL (global).
    OpenLastUrl,
    /// `-r` arrows — move focus.
    FocusDir(Dir),
    /// `-r` C-arrows / M-arrows — resize by 1 / 5 cells.
    Resize(Dir, u8),
    /// `prefix prefix` — inject a literal prefix byte into the focused PTY.
    SendPrefix,
}

/// The `(name, action)` vocabulary for config-file rebinding — pure data, so
/// the set is auditable and a future override file stays config-not-code.
const ACTION_NAMES: &[(&str, Action)] = &[
    ("new-chat-tab", Action::NewChatTab),
    ("split-shell", Action::SplitShell),
    ("next-tab", Action::NextTab),
    ("prev-tab", Action::PrevTab),
    ("last-tab", Action::LastTab),
    ("select-tab-0", Action::SelectTab(0)),
    ("select-tab-1", Action::SelectTab(1)),
    ("select-tab-2", Action::SelectTab(2)),
    ("select-tab-3", Action::SelectTab(3)),
    ("select-tab-4", Action::SelectTab(4)),
    ("select-tab-5", Action::SelectTab(5)),
    ("select-tab-6", Action::SelectTab(6)),
    ("select-tab-7", Action::SelectTab(7)),
    ("select-tab-8", Action::SelectTab(8)),
    ("select-tab-9", Action::SelectTab(9)),
    ("focus-next", Action::FocusNext),
    ("focus-last", Action::FocusLast),
    ("close-pane", Action::ClosePane),
    ("close-tab", Action::CloseTab),
    ("zoom", Action::Zoom),
    ("detach", Action::Detach),
    ("copy-mode", Action::CopyMode),
    ("choose-tree", Action::ChooseTree),
    ("display-panes", Action::DisplayPanes),
    ("follow-toggle", Action::FollowToggle),
    ("open-last-url", Action::OpenLastUrl),
    ("focus-left", Action::FocusDir(Dir::Left)),
    ("focus-right", Action::FocusDir(Dir::Right)),
    ("focus-up", Action::FocusDir(Dir::Up)),
    ("focus-down", Action::FocusDir(Dir::Down)),
    ("resize-left-1", Action::Resize(Dir::Left, 1)),
    ("resize-right-1", Action::Resize(Dir::Right, 1)),
    ("resize-up-1", Action::Resize(Dir::Up, 1)),
    ("resize-down-1", Action::Resize(Dir::Down, 1)),
    ("resize-left-5", Action::Resize(Dir::Left, 5)),
    ("resize-right-5", Action::Resize(Dir::Right, 5)),
    ("resize-up-5", Action::Resize(Dir::Up, 5)),
    ("resize-down-5", Action::Resize(Dir::Down, 5)),
    ("send-prefix", Action::SendPrefix),
];

impl Action {
    /// Resolve a config-file action name. Fail-loud: `None` is a config error
    /// the caller must surface, never a silent no-op binding.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Action> {
        ACTION_NAMES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, a)| *a)
    }
}

/// Which binding table the dispatcher is currently reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableId {
    Root,
    Prefix,
    /// Reserved for copy mode (design phase 8); no v1 binding switches here.
    Copy,
}

/// One table entry: the action plus tmux's `-r` repeat flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub action: Action,
    pub repeat: bool,
}

/// What the event loop should do with a key.
///
/// The design sketches `{Consumed(Action), Forward, Swallow}`; [`Pending`]
/// (consumed, now awaiting the rest of a prefix sequence) is split out rather
/// than folded into `Consumed` so tests and the event loop can tell "did
/// something" from "armed the prefix" without a sentinel action.
///
/// [`Pending`]: KeyDisposition::Pending
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDisposition {
    /// A binding fired — the event loop performs the action.
    Consumed(Action),
    /// The key armed (or continued) the prefix sequence; show a prefix hint.
    Pending,
    /// Not ours — forward to the focused pane (the only path to a PTY write).
    Forward,
    /// A post-prefix miss — the key must reach **nothing** (the safety rule).
    Swallow,
}

/// A key-string / binding error. Fail-loud: a config typo must abort cockpit
/// startup, never degrade into a dead or misrouted key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    UnknownKey(String),
    UnknownAction(String),
    UnknownTable(String),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::UnknownKey(s) => write!(f, "unknown key string `{s}`"),
            KeyError::UnknownAction(s) => write!(f, "unknown action `{s}`"),
            KeyError::UnknownTable(s) => write!(f, "unknown key table `{s}`"),
        }
    }
}

impl std::error::Error for KeyError {}

/// Parse a tmux-style key string: optional stacked `C-`/`M-`/`S-` modifier
/// prefixes, then a key name (`Left`, `PgUp`, `BSpace`, `F5`, `Space`,
/// `Enter`, `Escape`, `Tab`, …) or a single character (`"`, `b`, `-`). The
/// result is [normalized](KeyCombo::normalized) so parsed bindings compare
/// equal to normalized input events.
///
/// # Errors
/// [`KeyError::UnknownKey`] on an empty string, an unknown key name, or a
/// dangling modifier (`"C-"`).
pub fn parse_key_string(s: &str) -> Result<KeyCombo, KeyError> {
    let original = s;
    let mut mods = Mods::default();
    let mut rest = s;
    while rest.len() >= 2 && rest.as_bytes()[1] == b'-' {
        match rest.as_bytes()[0] {
            b'C' => mods.ctrl = true,
            b'M' => mods.alt = true,
            b'S' => mods.shift = true,
            _ => break,
        }
        rest = &rest[2..];
    }
    let code = match rest {
        "" => return Err(KeyError::UnknownKey(original.to_string())),
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PgUp" | "PageUp" => KeyCode::PageUp,
        "PgDn" | "PageDown" => KeyCode::PageDown,
        "Enter" => KeyCode::Enter,
        "Escape" | "Esc" => KeyCode::Esc,
        "Tab" => KeyCode::Tab,
        "BSpace" | "Backspace" => KeyCode::Backspace,
        "Space" => KeyCode::Char(' '),
        _ => {
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                (Some(f), None) => KeyCode::Char(f),
                _ => {
                    if let Some(n) = rest.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
                        if (1..=12).contains(&n) {
                            KeyCode::F(n)
                        } else {
                            return Err(KeyError::UnknownKey(original.to_string()));
                        }
                    } else {
                        return Err(KeyError::UnknownKey(original.to_string()));
                    }
                }
            }
        }
    };
    Ok(KeyCombo { code, mods }.normalized())
}

/// The prefix-table key dispatcher. One per cockpit; the event loop feeds it
/// every key event (already adapter-converted and release-filtered) and obeys
/// the returned [`KeyDisposition`].
#[derive(Debug)]
pub struct KeyDispatcher {
    tables: HashMap<TableId, HashMap<KeyCombo, Binding>>,
    current: TableId,
    /// The prefix combo — CONFIG, compared before any table lookup.
    prefix: KeyCombo,
    repeating: bool,
    repeat_deadline: Option<Instant>,
}

impl Default for KeyDispatcher {
    fn default() -> Self {
        Self::new(KeyCombo::ctrl('b'))
    }
}

impl KeyDispatcher {
    /// A dispatcher with the v1 default table and the given prefix. The
    /// `send-prefix` binding is derived from `prefix` at build time.
    #[must_use]
    pub fn new(prefix: KeyCombo) -> Self {
        let prefix = prefix.normalized();
        let mut tables = HashMap::new();
        tables.insert(TableId::Root, HashMap::new());
        tables.insert(TableId::Prefix, default_prefix_table(prefix));
        tables.insert(TableId::Copy, HashMap::new());
        Self {
            tables,
            current: TableId::Root,
            prefix,
            repeating: false,
            repeat_deadline: None,
        }
    }

    /// The configured prefix.
    #[must_use]
    pub fn prefix(&self) -> KeyCombo {
        self.prefix
    }

    /// Whether a prefix sequence is currently armed (the dispatcher is reading
    /// the Prefix table). The cockpit loop uses this to hold its direct global
    /// hotkeys while a prefix is pending, so a post-prefix key always resolves
    /// through the dispatcher.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.current == TableId::Prefix
    }

    /// Re-configure the prefix: the old prefix's `send-prefix` binding moves
    /// to the new combo (derived-at-build, per the design).
    pub fn set_prefix(&mut self, prefix: KeyCombo) {
        let prefix = prefix.normalized();
        if let Some(t) = self.tables.get_mut(&TableId::Prefix) {
            t.remove(&self.prefix);
            t.insert(
                prefix,
                Binding {
                    action: Action::SendPrefix,
                    repeat: false,
                },
            );
        }
        self.prefix = prefix;
    }

    /// Bind `key` (a tmux key string) to `action` (a config action name) in
    /// `table`. Fail-loud on any unknown part — a cockpit.toml typo must
    /// abort startup, not silently drop a binding.
    ///
    /// # Errors
    /// [`KeyError`] on an unknown key string, action name, or table name.
    pub fn bind(&mut self, table: &str, key: &str, action: &str) -> Result<(), KeyError> {
        let table = table_id(table)?;
        let combo = parse_key_string(key)?;
        let action =
            Action::from_name(action).ok_or_else(|| KeyError::UnknownAction(action.to_string()))?;
        // Repeat is a property of the default table's ergonomics (arrows);
        // config rebinds are non-repeat unless they re-bind an arrow default.
        let repeat = matches!(action, Action::FocusDir(_) | Action::Resize(_, _));
        self.tables
            .entry(table)
            .or_default()
            .insert(combo, Binding { action, repeat });
        Ok(())
    }

    /// Remove a binding.
    ///
    /// # Errors
    /// [`KeyError`] on an unknown key string or table name.
    pub fn unbind(&mut self, table: &str, key: &str) -> Result<(), KeyError> {
        let table = table_id(table)?;
        let combo = parse_key_string(key)?;
        if let Some(t) = self.tables.get_mut(&table) {
            t.remove(&combo);
        }
        Ok(())
    }

    /// Route one (adapter-normalized, non-release) key event. `now` is passed
    /// in so the 500 ms repeat deadline is lazy and the logic stays
    /// wall-clock-free in tests.
    pub fn on_key(&mut self, key: KeyCombo, now: Instant) -> KeyDisposition {
        let key = key.normalized();
        match self.current {
            TableId::Root | TableId::Copy => self.on_root_key(key),
            TableId::Prefix => self.on_prefix_key(key, now),
        }
    }

    fn on_root_key(&mut self, key: KeyCombo) -> KeyDisposition {
        // The prefix is config, checked BEFORE any table lookup.
        if key == self.prefix {
            self.current = TableId::Prefix;
            self.repeating = false;
            self.repeat_deadline = None;
            return KeyDisposition::Pending;
        }
        match self.tables.get(&TableId::Root).and_then(|t| t.get(&key)) {
            Some(b) => KeyDisposition::Consumed(b.action),
            None => KeyDisposition::Forward,
        }
    }

    fn on_prefix_key(&mut self, key: KeyCombo, now: Instant) -> KeyDisposition {
        if self.repeating {
            let expired = self.repeat_deadline.map_or(true, |d| now > d);
            if expired {
                // Lazy deadline: the repeat window closed before this key —
                // it is an ordinary Root-table key.
                self.reset();
                return self.on_root_key(key);
            }
            if let Some(b) = self
                .tables
                .get(&TableId::Prefix)
                .and_then(|t| t.get(&key))
                .copied()
            {
                if b.repeat {
                    self.repeat_deadline = Some(now + REPEAT_TIMEOUT);
                    return KeyDisposition::Consumed(b.action);
                }
            }
            // A non-repeat key during repeat re-resolves in Root.
            self.reset();
            return self.on_root_key(key);
        }
        if let Some(b) = self
            .tables
            .get(&TableId::Prefix)
            .and_then(|t| t.get(&key))
            .copied()
        {
            if b.repeat {
                self.repeating = true;
                self.repeat_deadline = Some(now + REPEAT_TIMEOUT);
                return KeyDisposition::Consumed(b.action);
            }
            self.reset();
            return KeyDisposition::Consumed(b.action);
        }
        // Prefix-table miss: fall back to Root …
        if let Some(b) = self
            .tables
            .get(&TableId::Root)
            .and_then(|t| t.get(&key))
            .copied()
        {
            self.reset();
            return KeyDisposition::Consumed(b.action);
        }
        // … and a miss AFTER fallback is swallowed: a post-prefix typo never
        // reaches the shell PTY. The single most important rule in the port.
        self.reset();
        KeyDisposition::Swallow
    }

    fn reset(&mut self) {
        self.current = TableId::Root;
        self.repeating = false;
        self.repeat_deadline = None;
    }
}

fn table_id(name: &str) -> Result<TableId, KeyError> {
    match name {
        "root" => Ok(TableId::Root),
        "prefix" => Ok(TableId::Prefix),
        "copy" => Ok(TableId::Copy),
        other => Err(KeyError::UnknownTable(other.to_string())),
    }
}

/// The v1 default Prefix table (design §"v1 binding subset", tmux-verbatim).
/// `%` is deliberately absent until `layout.rs` lands — an unbound prefix key
/// is safely swallowed, strictly better than a key that does the wrong thing.
fn default_prefix_table(prefix: KeyCombo) -> HashMap<KeyCombo, Binding> {
    let n = |action| Binding {
        action,
        repeat: false,
    };
    let r = |action| Binding {
        action,
        repeat: true,
    };
    let mut t = HashMap::new();
    t.insert(KeyCombo::char('c'), n(Action::NewChatTab));
    t.insert(KeyCombo::char('"'), n(Action::SplitShell));
    t.insert(KeyCombo::char('n'), n(Action::NextTab));
    t.insert(KeyCombo::char('p'), n(Action::PrevTab));
    t.insert(KeyCombo::char('l'), n(Action::LastTab));
    for d in 0..=9u8 {
        t.insert(
            KeyCombo::char(char::from(b'0' + d)),
            n(Action::SelectTab(d)),
        );
    }
    t.insert(KeyCombo::char('o'), n(Action::FocusNext));
    t.insert(KeyCombo::char(';'), n(Action::FocusLast));
    t.insert(KeyCombo::char('x'), n(Action::ClosePane));
    t.insert(KeyCombo::char('&'), n(Action::CloseTab));
    t.insert(KeyCombo::char('z'), n(Action::Zoom));
    t.insert(KeyCombo::char('d'), n(Action::Detach));
    t.insert(KeyCombo::char('['), n(Action::CopyMode));
    t.insert(KeyCombo::char('w'), n(Action::ChooseTree));
    t.insert(KeyCombo::char('q'), n(Action::DisplayPanes));
    t.insert(KeyCombo::char('f'), n(Action::FollowToggle));
    t.insert(KeyCombo::char('u'), n(Action::OpenLastUrl));
    for (code, dir) in [
        (KeyCode::Left, Dir::Left),
        (KeyCode::Right, Dir::Right),
        (KeyCode::Up, Dir::Up),
        (KeyCode::Down, Dir::Down),
    ] {
        t.insert(KeyCombo::code(code), r(Action::FocusDir(dir)));
        t.insert(
            KeyCombo {
                code,
                mods: Mods {
                    ctrl: true,
                    alt: false,
                    shift: false,
                },
            },
            r(Action::Resize(dir, 1)),
        );
        t.insert(
            KeyCombo {
                code,
                mods: Mods {
                    ctrl: false,
                    alt: true,
                    shift: false,
                },
            },
            r(Action::Resize(dir, 5)),
        );
    }
    // send-prefix, derived from the configured prefix at build time.
    t.insert(prefix, n(Action::SendPrefix));
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn prefix_then(d: &mut KeyDispatcher, key: KeyCombo, now: Instant) -> KeyDisposition {
        assert_eq!(
            d.on_key(KeyCombo::ctrl('b'), now),
            KeyDisposition::Pending,
            "prefix must arm"
        );
        d.on_key(key, now)
    }

    #[test]
    fn crossterm_variance_matrix_normalizes_to_one_combo() {
        // The same physical keystrokes as different terminals report them.
        let mut d = KeyDispatcher::default();
        let now = t0();

        // Ctrl+B as Char('B')+SHIFT+CTRL (uppercase report) still arms.
        let shouty_prefix = KeyCombo {
            code: KeyCode::Char('B'),
            mods: Mods {
                ctrl: true,
                alt: false,
                shift: true,
            },
        };
        assert_eq!(d.on_key(shouty_prefix, now), KeyDisposition::Pending);

        // `"` as Char('"')+SHIFT (shifted report) still splits the shell.
        let shifted_quote = KeyCombo {
            code: KeyCode::Char('"'),
            mods: Mods {
                ctrl: false,
                alt: false,
                shift: true,
            },
        };
        assert_eq!(
            d.on_key(shifted_quote, now),
            KeyDisposition::Consumed(Action::SplitShell)
        );

        // And the plain reports are identical.
        let mut d2 = KeyDispatcher::default();
        assert_eq!(
            prefix_then(&mut d2, KeyCombo::char('"'), now),
            KeyDisposition::Consumed(Action::SplitShell)
        );
    }

    #[test]
    fn root_keys_forward_and_prefix_sequence_consumes() {
        let mut d = KeyDispatcher::default();
        let now = t0();
        // Ordinary typing forwards to the pane.
        assert_eq!(d.on_key(KeyCombo::char('l'), now), KeyDisposition::Forward);
        // prefix c → new chat tab, then back in Root (l forwards again).
        assert_eq!(
            prefix_then(&mut d, KeyCombo::char('c'), now),
            KeyDisposition::Consumed(Action::NewChatTab)
        );
        assert_eq!(d.on_key(KeyCombo::char('l'), now), KeyDisposition::Forward);
    }

    #[test]
    fn post_prefix_miss_is_swallowed_never_forwarded() {
        // THE safety rule: a typo after the prefix reaches nothing.
        let mut d = KeyDispatcher::default();
        let now = t0();
        assert_eq!(
            prefix_then(&mut d, KeyCombo::char('e'), now),
            KeyDisposition::Swallow
        );
        // `%` is deliberately unbound in v1 → swallowed, not misrouted.
        assert_eq!(
            prefix_then(&mut d, KeyCombo::char('%'), now),
            KeyDisposition::Swallow
        );
        // The dispatcher is back in Root afterwards.
        assert_eq!(d.on_key(KeyCombo::char('e'), now), KeyDisposition::Forward);
    }

    #[test]
    fn send_prefix_is_prefix_pressed_twice() {
        let mut d = KeyDispatcher::default();
        let now = t0();
        assert_eq!(
            prefix_then(&mut d, KeyCombo::ctrl('b'), now),
            KeyDisposition::Consumed(Action::SendPrefix)
        );
    }

    #[test]
    fn repeat_binding_stays_armed_within_the_deadline() {
        let mut d = KeyDispatcher::default();
        let now = t0();
        assert_eq!(
            prefix_then(&mut d, KeyCombo::code(KeyCode::Left), now),
            KeyDisposition::Consumed(Action::FocusDir(Dir::Left))
        );
        // Still in the Prefix table: another arrow WITHOUT the prefix.
        let later = now + Duration::from_millis(300);
        assert_eq!(
            d.on_key(KeyCombo::code(KeyCode::Left), later),
            KeyDisposition::Consumed(Action::FocusDir(Dir::Left))
        );
        // Each repeat extends the lazy deadline from ITS OWN press.
        let later2 = later + Duration::from_millis(400);
        assert_eq!(
            d.on_key(KeyCombo::code(KeyCode::Down), later2),
            KeyDisposition::Consumed(Action::FocusDir(Dir::Down))
        );
    }

    #[test]
    fn repeat_expires_lazily_after_the_deadline() {
        let mut d = KeyDispatcher::default();
        let now = t0();
        prefix_then(&mut d, KeyCombo::code(KeyCode::Left), now);
        // 600ms later the window is closed — the arrow is a Root key again.
        let late = now + Duration::from_millis(600);
        assert_eq!(
            d.on_key(KeyCombo::code(KeyCode::Left), late),
            KeyDisposition::Forward
        );
    }

    #[test]
    fn non_repeat_key_during_repeat_reroutes_to_root() {
        let mut d = KeyDispatcher::default();
        let now = t0();
        prefix_then(&mut d, KeyCombo::code(KeyCode::Left), now);
        let within = now + Duration::from_millis(100);
        // `c` is a Prefix binding but NOT repeat — during repeat it re-resolves
        // in Root, where it is unbound → Forward (tmux behavior).
        assert_eq!(
            d.on_key(KeyCombo::char('c'), within),
            KeyDisposition::Forward
        );
        // The prefix itself during repeat re-arms a fresh sequence.
        let mut d2 = KeyDispatcher::default();
        prefix_then(&mut d2, KeyCombo::code(KeyCode::Right), now);
        assert_eq!(
            d2.on_key(KeyCombo::ctrl('b'), within),
            KeyDisposition::Pending
        );
        assert_eq!(
            d2.on_key(KeyCombo::char('c'), within),
            KeyDisposition::Consumed(Action::NewChatTab)
        );
    }

    #[test]
    fn resize_bindings_carry_their_step() {
        let mut d = KeyDispatcher::default();
        let now = t0();
        let ctrl_left = KeyCombo {
            code: KeyCode::Left,
            mods: Mods {
                ctrl: true,
                alt: false,
                shift: false,
            },
        };
        let alt_left = KeyCombo {
            code: KeyCode::Left,
            mods: Mods {
                ctrl: false,
                alt: true,
                shift: false,
            },
        };
        assert_eq!(
            prefix_then(&mut d, ctrl_left, now),
            KeyDisposition::Consumed(Action::Resize(Dir::Left, 1))
        );
        let mut d2 = KeyDispatcher::default();
        assert_eq!(
            prefix_then(&mut d2, alt_left, now),
            KeyDisposition::Consumed(Action::Resize(Dir::Left, 5))
        );
    }

    #[test]
    fn select_tab_digits_map_to_indices() {
        let now = t0();
        for digit in 0..=9u8 {
            let mut d = KeyDispatcher::default();
            assert_eq!(
                prefix_then(&mut d, KeyCombo::char(char::from(b'0' + digit)), now),
                KeyDisposition::Consumed(Action::SelectTab(digit))
            );
        }
    }

    #[test]
    fn send_prefix_follows_a_rebound_prefix() {
        let mut d = KeyDispatcher::default();
        let now = t0();
        d.set_prefix(KeyCombo::ctrl('a'));
        // New prefix arms; doubled = send-prefix.
        assert_eq!(d.on_key(KeyCombo::ctrl('a'), now), KeyDisposition::Pending);
        assert_eq!(
            d.on_key(KeyCombo::ctrl('a'), now),
            KeyDisposition::Consumed(Action::SendPrefix)
        );
        // The old prefix is an ordinary key now.
        assert_eq!(d.on_key(KeyCombo::ctrl('b'), now), KeyDisposition::Forward);
        // And the old send-prefix table entry moved (C-a c still works).
        assert_eq!(d.on_key(KeyCombo::ctrl('a'), now), KeyDisposition::Pending);
        assert_eq!(
            d.on_key(KeyCombo::char('c'), now),
            KeyDisposition::Consumed(Action::NewChatTab)
        );
    }

    #[test]
    fn parse_key_string_covers_the_config_vocabulary() {
        assert_eq!(parse_key_string("C-b").unwrap(), KeyCombo::ctrl('b'));
        assert_eq!(
            parse_key_string("M-Left").unwrap(),
            KeyCombo {
                code: KeyCode::Left,
                mods: Mods {
                    ctrl: false,
                    alt: true,
                    shift: false
                }
            }
        );
        assert_eq!(parse_key_string("\"").unwrap(), KeyCombo::char('"'));
        assert_eq!(parse_key_string("-").unwrap(), KeyCombo::char('-'));
        assert_eq!(
            parse_key_string("C-M-x").unwrap(),
            KeyCombo {
                code: KeyCode::Char('x'),
                mods: Mods {
                    ctrl: true,
                    alt: true,
                    shift: false
                }
            }
        );
        // Normalization applies to parsed strings too (S- on a char strips;
        // C-B folds) so bindings compare equal to normalized events.
        assert_eq!(parse_key_string("S-\"").unwrap(), KeyCombo::char('"'));
        assert_eq!(parse_key_string("C-B").unwrap(), KeyCombo::ctrl('b'));
        assert_eq!(parse_key_string("Space").unwrap(), KeyCombo::char(' '));
        assert_eq!(
            parse_key_string("F5").unwrap(),
            KeyCombo::code(KeyCode::F(5))
        );
        assert_eq!(
            parse_key_string("BSpace").unwrap(),
            KeyCombo::code(KeyCode::Backspace)
        );
    }

    #[test]
    fn bad_key_strings_fail_loud() {
        for bad in ["", "C-", "banana", "F99", "Q-x-y"] {
            assert!(parse_key_string(bad).is_err(), "`{bad}` must not parse");
        }
    }

    #[test]
    fn config_binding_is_fail_loud_and_actions_only() {
        let mut d = KeyDispatcher::default();
        // Unknown action name: a config typo aborts, never a dead key. There
        // is no way to bind a shell string — the vocabulary is Action names.
        assert_eq!(
            d.bind("prefix", "g", "run-shell"),
            Err(KeyError::UnknownAction("run-shell".into()))
        );
        assert_eq!(
            d.bind("prefix", "Q-x", "zoom"),
            Err(KeyError::UnknownKey("Q-x".into()))
        );
        assert_eq!(
            d.bind("pfx", "g", "zoom"),
            Err(KeyError::UnknownTable("pfx".into()))
        );

        // A good rebind works…
        d.bind("prefix", "g", "follow-toggle").unwrap();
        let now = t0();
        assert_eq!(
            prefix_then(&mut d, KeyCombo::char('g'), now),
            KeyDisposition::Consumed(Action::FollowToggle)
        );
        // …and unbind returns the key to swallowed-after-prefix.
        d.unbind("prefix", "g").unwrap();
        assert_eq!(
            prefix_then(&mut d, KeyCombo::char('g'), now),
            KeyDisposition::Swallow
        );
    }

    #[test]
    fn every_action_name_round_trips() {
        for (name, action) in ACTION_NAMES {
            assert_eq!(Action::from_name(name), Some(*action), "{name}");
        }
        assert_eq!(Action::from_name("nope"), None);
    }
}
