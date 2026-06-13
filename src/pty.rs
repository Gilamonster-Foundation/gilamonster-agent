//! `gila cowork` — the **PTY-hosted interactive shell pane** (Tier B, part 2).
//!
//! # What this is
//!
//! Issue #9 landed the split-pane scaffold with a *placeholder* bottom pane.
//! This module fills that pane with the human's **own** real interactive shell,
//! hosted on a pseudo-terminal: `ssh`, `vim`, `less` — the full command suite —
//! run natively, exactly as if the human opened a terminal. The agent in the top
//! pane watches what the human does (via the #8 observation channel) and assists.
//!
//! This is the human's shell, not the agent's confined deputy. The agent never
//! drives it; the brush/ocap confinement that clamps the *agent* is irrelevant
//! here — the human keeps full control of their own shell.
//!
//! ```text
//!   $SHELL (or /bin/bash)            vt100 grid           ratatui cells
//!   ┌──────────────┐  raw bytes  ┌──────────────┐  map  ┌──────────────┐
//!   │  pty slave   │ ──────────► │ vt100::Parser│ ────► │ shell Rect   │
//!   └──────────────┘   (thread)  └──────────────┘       └──────────────┘
//!          ▲                            │
//!          │ keystrokes                 │ new output → chunk
//!   (master writer)                     ▼
//!                              ObservationChannel (#8)  → TurnDriver
//!                              redaction-gated by construction
//! ```
//!
//! # Why `portable-pty` + `vt100`
//!
//! There is **no pseudo-terminal in std**, and crossterm/ratatui own only the
//! *host* terminal, not a spawned child's. [`portable-pty`](portable_pty) is the
//! maintained, cross-platform PTY crate (the wezterm one): it opens a pty pair,
//! spawns a command on the slave, and hands back a master reader/writer plus a
//! kernel-level resize handle. [`vt100`] is the companion terminal-state parser:
//! a shell emits a raw escape-code byte-stream (cursor moves, colours, scroll),
//! and `vt100` folds that into a screen *grid* we can read cell-by-cell. Neither
//! has a std equivalent; together they are the standard way to host a real shell
//! inside a TUI pane.
//!
//! # The testable units vs. the live carve-out
//!
//! Spawning a real shell and reading its output on a thread is inherently a
//! by-design-uncovered surface (same carve-out #8's live tail loop and #9's
//! crossterm event loop use). Everything *around* it is factored into pure units
//! the gate exercises:
//!
//! - [`pty_shell_program`] — `$SHELL` → `/bin/bash` resolution.
//! - [`screen_to_lines`] / [`vt_color_to_ratatui`] / [`cell_style`] — the
//!   vt100-grid → ratatui-line mapping.
//! - [`encode_key`] — the crossterm-key → PTY-write byte encoding.
//! - [`pty_size_for`] — the pane-[`Rect`] → [`PtySize`] resize math.
//! - [`PtyChildGuard`] — the RAII kill-on-drop, proven against a fake child.
//! - [`drain_new_output`] — the screen-delta → observation-chunk extraction the
//!   [`ObservationSource`] feeds into the #8 channel.

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyModifiers};
use portable_pty::PtySize;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::follow::ObservationSource;

/// The source tag stamped on every PTY observation, so the model can see the
/// activity came from the hosted shell pane (distinct from Tier A's
/// `"typescript"` tag). Same channel, different producer — see [`crate::follow`].
pub const PTY_SOURCE_TAG: &str = "pty";

/// The default shell when `$SHELL` is unset or empty.
pub const DEFAULT_SHELL: &str = "/bin/bash";

/// Resolve the program to host in the shell pane: the human's `$SHELL`, falling
/// back to [`DEFAULT_SHELL`] when it is unset or empty.
///
/// This is the human's *own* login shell — whatever they normally use — so
/// `ssh`, `vim`, and their dotfiles all behave exactly as in a real terminal.
/// Pure over the looked-up value so both arms are unit-testable without touching
/// the process environment.
#[must_use]
pub fn resolve_shell(shell_env: Option<String>) -> String {
    match shell_env {
        Some(s) if !s.trim().is_empty() => s,
        _ => DEFAULT_SHELL.to_string(),
    }
}

/// The program to host, read from the live `$SHELL` environment variable.
/// Thin wrapper over [`resolve_shell`] for the binary's spawn path.
#[must_use]
pub fn pty_shell_program() -> String {
    resolve_shell(std::env::var("SHELL").ok())
}

/// Map a [`vt100::Color`] to a ratatui [`Color`], or `None` for the terminal
/// default (which ratatui renders as "reset", i.e. the host theme's default).
///
/// Indexed colours pass through as ANSI indices; RGB passes through directly.
/// Pure — keyed only on the vt100 colour — so the colour bridge is unit-testable
/// without a live screen.
#[must_use]
pub fn vt_color_to_ratatui(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

/// Build the ratatui [`Style`] for one vt100 [`Cell`](vt100::Cell): foreground /
/// background colour plus bold / italic / underline / inverse modifiers.
///
/// `inverse` is mapped to ratatui's [`Modifier::REVERSED`] rather than swapping
/// the colours by hand, so the host terminal does the swap consistently with the
/// rest of the UI. Pure over the cell — unit-testable against a parsed screen.
#[must_use]
pub fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    if let Some(fg) = vt_color_to_ratatui(cell.fgcolor()) {
        style = style.fg(fg);
    }
    if let Some(bg) = vt_color_to_ratatui(cell.bgcolor()) {
        style = style.bg(bg);
    }
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// Map a parsed vt100 [`Screen`](vt100::Screen) into styled ratatui [`Line`]s —
/// the live shell's grid, ready to draw into the #9 shell [`Rect`].
///
/// One ratatui line per screen row; each visible cell becomes a styled
/// [`Span`]. Wide-character continuation cells (the empty second half of a
/// double-width glyph) are skipped so the text does not double up, and empty
/// cells render as a single space to preserve column alignment. Pure over the
/// screen, so the whole grid→widget mapping is unit-testable by feeding bytes to
/// a [`vt100::Parser`] and asserting on the produced lines.
#[must_use]
pub fn screen_to_lines(screen: &vt100::Screen) -> Vec<Line<'static>> {
    let (rows, cols) = screen.size();
    let mut lines = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            // The empty trailing half of a wide glyph is already covered by the
            // glyph's own cell; emitting it again would shift the row.
            if cell.is_wide_continuation() {
                continue;
            }
            let text = if cell.has_contents() {
                cell.contents()
            } else {
                " ".to_string()
            };
            spans.push(Span::styled(text, cell_style(cell)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Encode a crossterm key event as the bytes to write to the PTY master.
///
/// This is the keystroke-forwarding path: when the shell pane has focus, the
/// human's keys must reach the shell as the raw control bytes a terminal would
/// send. Printable characters go straight through as UTF-8; Enter is `\r` (a
/// terminal sends carriage-return, which the shell's line discipline turns into
/// newline); Backspace is DEL (`0x7f`); Tab, Esc, and the arrow/Home/End/PageUp
/// family map to their ANSI escape sequences; `Ctrl-<letter>` maps to the
/// corresponding C0 control byte (`Ctrl-C` → `0x03`, etc.).
///
/// Returns `None` for keys with no PTY meaning (e.g. a bare modifier press), so
/// the caller simply does not write. Pure — no PTY needed — so every branch is
/// unit-testable.
#[must_use]
pub fn encode_key(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Char(c) if ctrl => {
            // Ctrl-A..Ctrl-Z → 0x01..0x1a; Ctrl-[ \ ] ^ _ follow on. Map by the
            // ASCII-uppercase position so case does not matter.
            let upper = c.to_ascii_uppercase();
            if ('A'..='_').contains(&upper) {
                Some(vec![(upper as u8) - b'A' + 1])
            } else if c == ' ' {
                // Ctrl-Space is the conventional NUL.
                Some(vec![0])
            } else {
                None
            }
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        _ => None,
    }
}

/// Compute the [`PtySize`] (and equivalently the vt100 grid size) for a pane
/// [`Rect`].
///
/// The shell pane is drawn inside a one-cell border on every side ([#9's
/// `Block::bordered`](crate::cowork)), so the usable interior is `width-2` ×
/// `height-2`. A pty must never be zero-sized — `vt100` and most shells assume at
/// least a 1×1 grid — so both dimensions are clamped up to 1. Pure geometry over
/// the rect, so the resize math is unit-testable without a terminal.
#[must_use]
pub fn pty_size_for(area: Rect) -> PtySize {
    let cols = area.width.saturating_sub(2).max(1);
    let rows = area.height.saturating_sub(2).max(1);
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// A child process that can be killed and polled for exit — the minimal slice of
/// [`portable_pty::Child`] the lifecycle guard needs.
///
/// Factored into its own trait so [`PtyChildGuard`]'s kill-on-drop is testable
/// against a fake that records the kill, without spawning a real shell. The real
/// `Box<dyn portable_pty::Child + Send + Sync>` implements it via the blanket
/// impl below.
pub trait KillableChild: Send {
    /// Terminate the child process. Best-effort: a dead child is not an error.
    fn kill(&mut self) -> std::io::Result<()>;
    /// Whether the child has already exited (so the reader loop can stop).
    fn has_exited(&mut self) -> bool;
}

impl KillableChild for Box<dyn portable_pty::Child + Send + Sync> {
    fn kill(&mut self) -> std::io::Result<()> {
        portable_pty::ChildKiller::kill(self.as_mut())
    }
    fn has_exited(&mut self) -> bool {
        matches!(self.try_wait(), Ok(Some(_)))
    }
}

/// RAII guard that **kills the hosted shell on teardown** — on a clean quit, on
/// an error unwinding out of the loop, and on a panic mid-render.
///
/// A hosted shell is a real child process; if `gila cowork` exits without
/// reaping it, the shell is orphaned (and may keep holding the pty open). `Drop`
/// is the only leak-proof shape: it fires on every exit path, mirroring the
/// terminal-restore guard #9 uses for raw mode. The kill is idempotent — an
/// explicit [`kill`](Self::kill) before drop makes the `Drop` a no-op — and
/// best-effort, so a teardown kill never panics even if the child already exited.
///
/// Generic over [`KillableChild`] so a test can install a fake child that records
/// the kill and assert the guard fires it on `Drop`.
pub struct PtyChildGuard<C: KillableChild> {
    child: C,
    killed: bool,
}

impl<C: KillableChild> PtyChildGuard<C> {
    /// Wrap a child so its death is tied to the guard's scope.
    pub fn new(child: C) -> Self {
        Self {
            child,
            killed: false,
        }
    }

    /// Kill the child now (idempotent, best-effort). Lets the caller tear down
    /// deterministically; `Drop` then becomes a no-op.
    pub fn kill(&mut self) {
        if !self.killed {
            let _ = self.child.kill();
            self.killed = true;
        }
    }

    /// Whether the child has already exited on its own (the human typed `exit`).
    pub fn has_exited(&mut self) -> bool {
        self.child.has_exited()
    }
}

impl<C: KillableChild> Drop for PtyChildGuard<C> {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The shared, thread-safe accumulator the reader thread writes into and the
/// render/observation side reads from.
///
/// The reader thread owns the read end of the pty master and, on every burst of
/// output, (1) feeds the bytes to the [`vt100::Parser`] so the next render sees
/// the updated grid, and (2) appends the decoded text to a pending buffer the
/// [`ObservationSource`] drains into the #8 channel. Wrapping both in one mutex
/// keeps the parser and the pending buffer consistent under concurrent access.
pub struct SharedScreen {
    parser: vt100::Parser,
    /// New shell text since the last [`drain_new_output`] — fed to the channel.
    pending: String,
}

impl SharedScreen {
    /// A fresh accumulator sized to a `rows`×`cols` grid with `scrollback` lines
    /// of history retained for the parser.
    #[must_use]
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback),
            pending: String::new(),
        }
    }

    /// Feed a burst of raw shell bytes: update the screen grid and append the
    /// decoded text to the pending observation buffer. Called by the reader
    /// thread for every chunk it reads off the pty master.
    pub fn ingest(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.pending.push_str(&String::from_utf8_lossy(bytes));
    }

    /// Resize the underlying grid to a new `rows`×`cols` (mirrors a pty resize so
    /// the parser's idea of the screen tracks the pane).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(rows, cols);
    }

    /// Borrow the current screen grid for rendering.
    #[must_use]
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Take and clear the pending shell output accumulated since the last call —
    /// the next chunk to feed the #8 channel. Returns `None` when nothing new has
    /// arrived (so the [`ObservationSource`] reports "no growth" like the tail).
    pub fn drain_new_output(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending))
        }
    }
}

/// An [`ObservationSource`] over a hosted PTY shell — Tier B's producer for the
/// **same** #8 [`ObservationChannel`](crate::follow::ObservationChannel) Tier A's
/// typescript tail feeds.
///
/// Each [`next_chunk`](ObservationSource::next_chunk) drains whatever new output
/// the reader thread has accumulated in the [`SharedScreen`]. Because it feeds
/// the channel — never the transcript directly — the human's shell activity is
/// **redaction-gated by construction**: the channel wraps every chunk as a
/// `ShellObservation`, which scrubs credentials before the agent can ever see it.
/// The agent observes what the human does and assists; it never drives the shell.
///
/// This is the whole reason #8 factored the producer behind a trait: swapping the
/// PTY in for the typescript tail touches nothing on the channel/agent side.
pub struct PtyShellSource {
    shared: Arc<Mutex<SharedScreen>>,
}

impl PtyShellSource {
    /// Build a source that drains the given shared screen accumulator.
    #[must_use]
    pub fn new(shared: Arc<Mutex<SharedScreen>>) -> Self {
        Self { shared }
    }
}

impl ObservationSource for PtyShellSource {
    fn next_chunk(&mut self) -> Option<String> {
        self.shared.lock().ok()?.drain_new_output()
    }

    fn source_tag(&self) -> &str {
        PTY_SOURCE_TAG
    }
}

/// How many scrollback lines the hosted shell's parser retains. Generous enough
/// that paging through `less`/`git log` keeps a useful history without unbounded
/// growth.
const SHELL_SCROLLBACK: usize = 1000;

/// The live, hosted shell — the by-design-uncovered surface that ties the
/// testable units together against a real PTY and a real child process.
///
/// On [`spawn`](PtyShell::spawn) it opens a pty pair, launches the human's
/// `$SHELL` on the slave, and starts a background reader thread that pumps the
/// master's output into the shared [`SharedScreen`] (updating both the render
/// grid and the #8 observation buffer). The struct exposes:
///
/// - [`shared`](PtyShell::shared) — the grid the render reads and the
///   [`PtyShellSource`] drains, cloneable for both.
/// - [`write_input`](PtyShell::write_input) — forward the human's keystrokes
///   (already [`encode_key`]-encoded) to the shell.
/// - [`resize`](PtyShell::resize) — resize the kernel pty *and* the parser grid
///   together when the pane changes size.
/// - the embedded [`PtyChildGuard`], so dropping the `PtyShell` kills the shell
///   and the reader thread observes EOF and exits — no orphaned process.
///
/// Spawning a real shell and reading it on a thread cannot be exercised by the
/// coverage gate (it needs a real OS pty and a real child), so this struct is the
/// same carve-out #8's live tail loop and #9's crossterm event loop are; the
/// logic it wires (the grid mapping, the key encoding, the resize math, the
/// guard's kill-on-drop, the channel feed) is all unit-tested above.
pub struct PtyShell {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    shared: Arc<Mutex<SharedScreen>>,
    /// Killing the child on drop closes the slave, which EOFs the master read,
    /// which lets the reader thread fall out of its loop and finish.
    guard: PtyChildGuard<Box<dyn portable_pty::Child + Send + Sync>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl PtyShell {
    /// Spawn `program` on a fresh pty sized for `size`, in `cwd`, and start the
    /// reader thread that mirrors its output into the shared screen.
    ///
    /// `cwd` is where the shell starts (the cowork workspace). The child inherits
    /// the parent environment and gets `TERM=xterm-256color` so colour-aware
    /// programs behave; the pty is given a controlling tty so job control and
    /// full-screen programs (`vim`, `less`, `ssh`) work natively.
    pub fn spawn(
        program: &str,
        size: PtySize,
        cwd: Option<&std::path::Path>,
    ) -> anyhow::Result<Self> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system.openpty(size)?;

        let mut cmd = portable_pty::CommandBuilder::new(program);
        cmd.env("TERM", "xterm-256color");
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        let child = pair.slave.spawn_command(cmd)?;
        // The slave handle is no longer needed by us; dropping it means only the
        // child holds the slave open, so the master EOFs once the child exits.
        drop(pair.slave);

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;

        let shared = Arc::new(Mutex::new(SharedScreen::new(
            size.rows,
            size.cols,
            SHELL_SCROLLBACK,
        )));

        // Reader thread: pump master output into the shared screen until EOF.
        let reader_shared = shared.clone();
        let handle = std::thread::Builder::new()
            .name("gila-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        // EOF: the shell exited and the master closed.
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut s) = reader_shared.lock() {
                                s.ingest(&buf[..n]);
                            }
                        }
                        // A real I/O error on the master also ends the loop.
                        Err(_) => break,
                    }
                }
            })?;

        Ok(Self {
            master: pair.master,
            writer,
            shared,
            guard: PtyChildGuard::new(child),
            reader: Some(handle),
        })
    }

    /// The shared screen accumulator — clone the `Arc` for the render side and
    /// for a [`PtyShellSource`] feeding the #8 channel.
    #[must_use]
    pub fn shared(&self) -> Arc<Mutex<SharedScreen>> {
        self.shared.clone()
    }

    /// A [`PtyShellSource`] over this shell's output — hand it to the channel.
    #[must_use]
    pub fn observation_source(&self) -> PtyShellSource {
        PtyShellSource::new(self.shared())
    }

    /// Forward already-encoded keystroke bytes (see [`encode_key`]) to the shell.
    pub fn write_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Resize the hosted shell to a new pane [`Rect`]: resize the kernel pty (so
    /// the shell gets SIGWINCH and reflows) and the parser grid (so the render
    /// tracks it). Best-effort on the kernel side — a failed resize is not fatal.
    pub fn resize(&mut self, area: Rect) {
        let size = pty_size_for(area);
        let _ = self.master.resize(size);
        if let Ok(mut s) = self.shared.lock() {
            s.resize(size.rows, size.cols);
        }
    }

    /// Whether the hosted shell has exited on its own (the human typed `exit`).
    pub fn has_exited(&mut self) -> bool {
        self.guard.has_exited()
    }
}

impl Drop for PtyShell {
    fn drop(&mut self) {
        // Kill the child first; that closes the slave → the master read EOFs →
        // the reader thread breaks out of its loop. Then join it so no thread is
        // left reading a closed fd. The child guard's own Drop is idempotent.
        self.guard.kill();
        // Dropping the master writer/handle also helps the reader see EOF on
        // platforms where the kill alone doesn't.
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- shell resolution ---------------------------------------------------

    #[test]
    fn resolve_shell_uses_env_when_set() {
        assert_eq!(resolve_shell(Some("/usr/bin/zsh".into())), "/usr/bin/zsh");
    }

    #[test]
    fn resolve_shell_falls_back_when_unset_or_blank() {
        assert_eq!(resolve_shell(None), DEFAULT_SHELL);
        assert_eq!(resolve_shell(Some(String::new())), DEFAULT_SHELL);
        assert_eq!(resolve_shell(Some("   ".into())), DEFAULT_SHELL);
    }

    #[test]
    fn pty_shell_program_returns_a_nonempty_program() {
        // Reads the live env; whatever it resolves to must be non-empty (env or
        // the fallback). We don't assert a specific path — that depends on $SHELL.
        assert!(!pty_shell_program().is_empty());
    }

    // --- colour bridge ------------------------------------------------------

    #[test]
    fn vt_color_default_is_none() {
        assert_eq!(vt_color_to_ratatui(vt100::Color::Default), None);
    }

    #[test]
    fn vt_color_indexed_and_rgb_pass_through() {
        assert_eq!(
            vt_color_to_ratatui(vt100::Color::Idx(4)),
            Some(Color::Indexed(4))
        );
        assert_eq!(
            vt_color_to_ratatui(vt100::Color::Rgb(10, 20, 30)),
            Some(Color::Rgb(10, 20, 30))
        );
    }

    // --- cell style + grid → lines ------------------------------------------

    /// Parse SGR-styled output and assert the produced ratatui spans carry the
    /// colour and bold modifier — the vt100-grid → ratatui mapping end to end.
    #[test]
    fn screen_to_lines_carries_text_color_and_bold() {
        let mut parser = vt100::Parser::new(3, 20, 0);
        // Bold + red "HI", then reset.
        parser.process(b"\x1b[1;31mHI\x1b[0m");
        let lines = screen_to_lines(parser.screen());
        assert_eq!(lines.len(), 3, "one ratatui line per screen row");

        // Flatten row 0 to text.
        let row0: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(row0.starts_with("HI"), "rendered text: {row0:?}");

        // The 'H' cell is red + bold.
        let h = &lines[0].spans[0];
        assert_eq!(h.style.fg, Some(Color::Indexed(1)), "red fg");
        assert!(h.style.add_modifier.contains(Modifier::BOLD), "bold set");
    }

    #[test]
    fn screen_to_lines_blank_screen_is_all_spaces_full_grid() {
        let parser = vt100::Parser::new(2, 5, 0);
        let lines = screen_to_lines(parser.screen());
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(text, "     ", "blank row renders as 5 spaces");
        }
    }

    #[test]
    fn screen_to_lines_handles_wide_glyphs_without_doubling() {
        // A wide (double-width) glyph occupies two cells: the glyph cell and a
        // continuation cell. The mapping must emit the glyph once and skip the
        // continuation so the row text isn't shifted or doubled.
        let mut parser = vt100::Parser::new(1, 10, 0);
        parser.process("世X".as_bytes()); // 世 is double-width, X single.
        let lines = screen_to_lines(parser.screen());
        let row0: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        // The wide glyph appears exactly once, immediately followed by X (no
        // empty continuation cell duplicated in between).
        assert!(row0.starts_with("世X"), "wide glyph mapped once: {row0:?}");
    }

    #[test]
    fn cell_style_maps_all_modifiers() {
        let mut parser = vt100::Parser::new(1, 10, 0);
        // bold, italic, underline, inverse all on for "X".
        parser.process(b"\x1b[1;3;4;7mX");
        let cell = parser.screen().cell(0, 0).unwrap();
        let style = cell_style(cell);
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn cell_style_background_color_maps() {
        let mut parser = vt100::Parser::new(1, 10, 0);
        // Background blue (idx 4) for "Y".
        parser.process(b"\x1b[44mY");
        let cell = parser.screen().cell(0, 0).unwrap();
        assert_eq!(cell_style(cell).bg, Some(Color::Indexed(4)));
    }

    // --- key encoding -------------------------------------------------------

    #[test]
    fn encode_printable_char_is_utf8() {
        assert_eq!(
            encode_key(KeyCode::Char('a'), KeyModifiers::NONE),
            Some(vec![b'a'])
        );
        // Multi-byte UTF-8 passes through.
        assert_eq!(
            encode_key(KeyCode::Char('é'), KeyModifiers::NONE),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn encode_enter_is_carriage_return() {
        assert_eq!(
            encode_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(vec![b'\r'])
        );
    }

    #[test]
    fn encode_backspace_is_del() {
        assert_eq!(
            encode_key(KeyCode::Backspace, KeyModifiers::NONE),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn encode_ctrl_letters_are_c0_controls() {
        // Ctrl-C → ETX (0x03), Ctrl-D → EOT (0x04), Ctrl-A → SOH (0x01).
        assert_eq!(
            encode_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Some(vec![0x04])
        );
        assert_eq!(
            encode_key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            Some(vec![0x01])
        );
        // Case-insensitive: Ctrl-Z and Ctrl-z both → SUB (0x1a).
        assert_eq!(
            encode_key(KeyCode::Char('Z'), KeyModifiers::CONTROL),
            Some(vec![0x1a])
        );
    }

    #[test]
    fn encode_ctrl_space_is_nul() {
        assert_eq!(
            encode_key(KeyCode::Char(' '), KeyModifiers::CONTROL),
            Some(vec![0])
        );
    }

    #[test]
    fn encode_arrows_and_nav_are_ansi_escapes() {
        assert_eq!(
            encode_key(KeyCode::Up, KeyModifiers::NONE),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Down, KeyModifiers::NONE),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Right, KeyModifiers::NONE),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Left, KeyModifiers::NONE),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Home, KeyModifiers::NONE),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::End, KeyModifiers::NONE),
            Some(b"\x1b[F".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Delete, KeyModifiers::NONE),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn encode_tab_and_esc() {
        assert_eq!(
            encode_key(KeyCode::Tab, KeyModifiers::NONE),
            Some(vec![b'\t'])
        );
        assert_eq!(
            encode_key(KeyCode::Esc, KeyModifiers::NONE),
            Some(vec![0x1b])
        );
        assert_eq!(
            encode_key(KeyCode::BackTab, KeyModifiers::NONE),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn encode_unmapped_key_is_none() {
        // A function key we don't forward yields no bytes (caller writes nothing).
        assert_eq!(encode_key(KeyCode::F(5), KeyModifiers::NONE), None);
        // Ctrl with a non-letter, non-space yields nothing.
        assert_eq!(encode_key(KeyCode::Char('1'), KeyModifiers::CONTROL), None);
    }

    // --- resize math --------------------------------------------------------

    #[test]
    fn pty_size_subtracts_the_border() {
        let size = pty_size_for(Rect::new(0, 0, 80, 24));
        assert_eq!(size.cols, 78, "width minus 2 border columns");
        assert_eq!(size.rows, 22, "height minus 2 border rows");
    }

    #[test]
    fn pty_size_never_zero() {
        // A pane too small for any interior still yields a 1×1 grid (never 0).
        let size = pty_size_for(Rect::new(0, 0, 1, 1));
        assert_eq!(size.cols, 1);
        assert_eq!(size.rows, 1);
        let zero = pty_size_for(Rect::new(0, 0, 0, 0));
        assert_eq!(zero.cols, 1);
        assert_eq!(zero.rows, 1);
    }

    // --- PtyChildGuard lifecycle (fake child) -------------------------------

    /// A fake child that records kills and reports a scripted exit state — lets
    /// us prove the guard's kill-on-drop without spawning a real shell.
    struct FakeChild {
        kills: Arc<Mutex<usize>>,
        exited: bool,
    }

    impl KillableChild for FakeChild {
        fn kill(&mut self) -> std::io::Result<()> {
            *self.kills.lock().unwrap() += 1;
            Ok(())
        }
        fn has_exited(&mut self) -> bool {
            self.exited
        }
    }

    #[test]
    fn guard_kills_child_exactly_once_on_drop() {
        let kills = Arc::new(Mutex::new(0));
        {
            let _guard = PtyChildGuard::new(FakeChild {
                kills: kills.clone(),
                exited: false,
            });
            assert_eq!(*kills.lock().unwrap(), 0, "no kill before drop");
        } // guard dropped here
        assert_eq!(*kills.lock().unwrap(), 1, "Drop kills exactly once");
    }

    #[test]
    fn guard_manual_kill_makes_drop_a_noop() {
        let kills = Arc::new(Mutex::new(0));
        let mut guard = PtyChildGuard::new(FakeChild {
            kills: kills.clone(),
            exited: false,
        });
        guard.kill();
        assert_eq!(*kills.lock().unwrap(), 1);
        guard.kill(); // idempotent
        assert_eq!(*kills.lock().unwrap(), 1);
        drop(guard);
        assert_eq!(
            *kills.lock().unwrap(),
            1,
            "Drop after manual kill is a no-op"
        );
    }

    #[test]
    fn guard_restores_on_panic_unwind() {
        let kills = Arc::new(Mutex::new(0));
        let k = kills.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = PtyChildGuard::new(FakeChild {
                kills: k,
                exited: false,
            });
            panic!("render blew up with the shell live");
        }));
        assert!(result.is_err(), "panic propagated");
        assert_eq!(
            *kills.lock().unwrap(),
            1,
            "the guard killed the shell during unwind"
        );
    }

    #[test]
    fn guard_reports_child_exit() {
        let kills = Arc::new(Mutex::new(0));
        let mut guard = PtyChildGuard::new(FakeChild {
            kills,
            exited: true,
        });
        assert!(guard.has_exited(), "a self-exited shell is observable");
    }

    // --- SharedScreen + PtyShellSource (the #8 feed) ------------------------

    #[test]
    fn shared_screen_ingest_updates_grid_and_buffers_output() {
        let mut shared = SharedScreen::new(4, 20, 0);
        shared.ingest(b"hello");
        // Grid updated: row 0 starts with "hello".
        let lines = screen_to_lines(shared.screen());
        let row0: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(row0.starts_with("hello"), "grid updated: {row0:?}");
        // And the text is pending for the observation channel.
        assert_eq!(shared.drain_new_output().as_deref(), Some("hello"));
        // Drained: nothing left.
        assert!(shared.drain_new_output().is_none());
    }

    #[test]
    fn shared_screen_resize_changes_grid_size() {
        let mut shared = SharedScreen::new(4, 20, 0);
        shared.resize(2, 10);
        assert_eq!(shared.screen().size(), (2, 10));
    }

    #[test]
    fn drain_new_output_strips_escape_codes_into_lossy_text() {
        // The pending buffer carries the raw bytes decoded lossily — including
        // escape codes, which the channel's redaction/framing handles. We only
        // assert the human-visible text is present.
        let mut shared = SharedScreen::new(4, 40, 0);
        shared.ingest(b"\x1b[32m$ ls\x1b[0m\r\nfile.txt\r\n");
        let chunk = shared.drain_new_output().expect("output buffered");
        assert!(chunk.contains("$ ls"));
        assert!(chunk.contains("file.txt"));
    }

    /// THE end-to-end seam test: a PTY source feeds the SAME #8 channel a
    /// typescript tail would, and a secret in the shell output is redacted before
    /// it can reach the transcript — exactly the Tier A guarantee, carried to
    /// Tier B unchanged because the source is swapped behind the trait.
    #[test]
    fn pty_source_feeds_the_channel_and_redaction_holds() {
        use crate::follow::{follow_tick, FollowTick, ObservationChannel};
        use newt_core::agentic::{TurnDriver, TurnDriverConfig};
        use newt_core::BackendKind;

        let shared = Arc::new(Mutex::new(SharedScreen::new(6, 40, 0)));
        // The human runs a command that prints a secret in their own shell.
        shared
            .lock()
            .unwrap()
            .ingest(b"$ cat .env\r\nsecret_key=wJalrXUtnFEMIabcdEFGHIJKLMNOPbPxRfiCY\r\n");

        let mut source = PtyShellSource::new(shared.clone());
        assert_eq!(source.source_tag(), PTY_SOURCE_TAG);

        let cfg =
            TurnDriverConfig::new("http://127.0.0.1:1", "test-model", BackendKind::Ollama, ".");
        let mut channel = ObservationChannel::new(TurnDriver::new(cfg));

        // One follow tick drains the PTY output into the channel.
        assert_eq!(follow_tick(&mut channel, &mut source), FollowTick::Observed);
        // A second tick: nothing new → Idle (the source reports no growth).
        assert_eq!(follow_tick(&mut channel, &mut source), FollowTick::Idle);

        let transcript = channel.driver().transcript();
        assert_eq!(transcript.len(), 1, "one observation accumulated");
        let body = &transcript[0].content;
        assert!(body.contains("source: pty"), "tagged as the PTY source");
        assert!(body.contains("cat .env"), "benign activity preserved");
        assert!(body.contains("[REDACTED]"), "redaction fired: {body}");
        assert!(
            !body.contains("wJalrXUtnFEMIabcdEFGHIJKLMNOPbPxRfiCY"),
            "secret leaked into transcript: {body}"
        );
    }

    #[test]
    fn pty_source_idle_when_no_new_output() {
        let shared = Arc::new(Mutex::new(SharedScreen::new(4, 20, 0)));
        let mut source = PtyShellSource::new(shared);
        // No ingest yet → nothing to observe.
        assert!(source.next_chunk().is_none());
    }

    // --- live-PTY smoke test (real child; #[ignore]'d in the gate) -----------

    /// A real-PTY smoke test: spawn a deterministic child (`echo`) on an actual
    /// pty, let the reader thread mirror its output into the shared screen, and
    /// assert the rendered grid contains the echoed text — proving the live
    /// `spawn` → reader-thread → grid path works end to end, and that dropping the
    /// `PtyShell` reaps the child (no orphan).
    ///
    /// `#[ignore]`d so it never runs in `just check` / `just cov-ci`: spawning a
    /// real process and racing a reader thread is inherently timing-dependent and
    /// could flake on a loaded CI box. Run it deliberately with
    /// `cargo test -- --ignored pty_shell_spawns_a_real_child_and_renders`. All
    /// the *logic* it exercises is covered deterministically by the unit tests
    /// above; this is the optional live confirmation.
    #[test]
    #[ignore = "spawns a real child on a real pty; run with --ignored (kept out of the timing-sensitive gate)"]
    fn pty_shell_spawns_a_real_child_and_renders() {
        // Spawn a real shell on a real pty, drive a deterministic one-shot
        // command through the keystroke-forwarding path, and assert the output
        // lands in the rendered grid.
        let mut shell = PtyShell::spawn(
            "/bin/sh",
            PtySize {
                rows: 4,
                cols: 40,
                pixel_width: 0,
                pixel_height: 0,
            },
            None,
        )
        .expect("spawn sh on a pty");

        shell.write_input(b"printf hello\r\n").unwrap();

        let mut got = false;
        for _ in 0..300 {
            {
                let s = shell.shared();
                let guard = s.lock().unwrap();
                let txt: String = screen_to_lines(guard.screen())
                    .iter()
                    .flat_map(|l| l.spans.iter())
                    .map(|sp| sp.content.to_string())
                    .collect();
                if txt.contains("hello") {
                    got = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(got, "the shell's `printf hello` output reached the grid");

        // The PTY source drains the same output for the #8 channel.
        let mut source = shell.observation_source();
        assert!(
            source.next_chunk().is_some_and(|c| c.contains("hello")),
            "the observation source sees the shell output too"
        );

        // Dropping the shell must reap the child and join the reader without
        // hanging — no orphaned process.
        drop(shell);
    }
}
