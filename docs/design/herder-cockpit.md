# Design: the herder cockpit — gila as the full-screen multiplexer for newt-agent

**Status:** design-for-discussion — captures the shape and the resolved user
decisions before code lands. Extends `docs/design/cockpit-tmux-multiplexer.md`
(the tmux-semantics core: `keys.rs`, `layout.rs`, the authority map, follow-me)
into gila's full identity as newt's rich-TUI counterpart: a herdr-style
multiplexer that can seat and supervise *other agent harnesses*, not only its
own chat driver, and that talks back with a voice.

> newt's own decision docs designate gila as the home for the rich TUI; newt
> chat stays an inline scroller (`newt:docs/decisions/plain_scroller_tui.md`).
> This document is the "what gila does with that mandate" doc.

---

## TL;DR

gila becomes a **herdr-style multiplexer**: many tabs, each a tmux-like
layout tree of panes, where a pane can host gila's own chat agent, a
subordinate harness (`newt`, `claude`, `codex`, or an arbitrary command) on a
PTY, a plain shell, or an agent inside an OpenShell sandbox. `agent-voice` is
embedded in-process for full voice conversation with gila. gila **supervises**
its child harnesses like a herder watches a paddock — reading their output,
never typing into them without an explicit human-granted authority.

Most of the raw material already exists in-tree (`cockpit.rs`, `layout.rs`,
`keys.rs`, `cowork.rs`, `pty.rs`, `follow.rs`, `authority.rs`) and is reused,
not rewritten. This doc adds the pieces the tmux-multiplexer design left as
named-but-undesigned seams: the `PaneBackend` trait that lets a pane host
something other than gila's own chat driver, the agents-manifest palette, the
supervision watch/herd modes, the voice conversation loop, and the flip of
`gila`'s bare default entry point.

---

## Why gila, not newt (the doctrine check, inherited)

Same doctrine as `cockpit-tmux-multiplexer.md`: newt's `InputSurface` is one
plain-scroller path with no panes, no alternate screen, no dashboards. The
*"Advanced TUI: panes, live status, dashboards"* tier is gila's. Supervising
other agent processes from inside a full-screen TUI is a strict superset of
that tier, so it stays here — newt never grows a process-supervision surface,
and gila never re-implements newt's own chat loop (it already re-implements
the chat *widget* per `cockpit-tmux-multiplexer.md`'s load-bearing verdict;
supervising `newt` itself is done by treating a `newt` invocation as an
opaque PTY subordinate, the same as `claude` or `codex`).

---

## The `PaneBackend` seam

`cockpit.rs` (owned by another workstream) stays the pure, I/O-free
tmux-semantics model: tabs, layout trees, focus, `Action`/`Effect`. What that
model does *not* know about is what fills a pane. The herder cockpit adds one
seam beneath it:

```rust
pub trait PaneBackend {
    fn title(&self) -> String;
    fn render_lines(&mut self, area: Rect) -> Vec<Line<'static>>;
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<()>;
    fn resize(&mut self, area: Rect);
    fn tick(&mut self) -> PaneEvent;              // poll TurnDriver / drain PTY / poll gRPC
    fn observation(&mut self) -> Option<String>;  // drained new output for supervision
    fn is_closed(&mut self) -> bool;
    fn shutdown(&mut self);
}
```

`cockpit.rs`'s `Effect` grows `PaneOpened(PaneId, PaneRole)` /
`PaneClosed(PaneId)` variants so the runtime — not the pure model — constructs
and tears down backends. The runtime owns a parallel
`HashMap<PaneId, Box<dyn PaneBackend>>` alongside the pure `Cockpit` state.
`keys.rs::Action` gains `NewTabWithAgent`, `SpawnPaneCommand`, `VoiceToggle`,
`SuperviseToggle`, and a palette-open action.

### Three implementations, each repackaging existing code

1. **`ChatPaneBackend`** — cowork's chat half: `TurnDriver`
   (submit/poll/cancel/transcript), `authority::driver_config(PaneKind, …)`,
   cowork's transcript→ratatui rendering, the input-line editing lifted from
   `CoworkApp`. `PaneKind` is chosen at pane creation, same authority map as
   `cockpit-tmux-multiplexer.md`.
2. **`PtyPaneBackend`** — `pty.rs` machinery essentially verbatim, with the
   command parameterized by an [`AgentManifest`](#the-agents-manifest)
   entry's `argv`. Spawn goes through
   `agent_bridle_core::spawn_confined_subprocess` — the same gate
   `capabilities.rs` uses — with a `SpawnPosture::{Ambient, Confined}` knob
   read from the manifest entry's `posture`. Cockpit panes default
   **Confined**, per `docs/decisions/ambient_native_shell_default.md`; an
   ambient pane (a trusted login shell, say) is an explicit per-entry opt-in,
   never a pane-creation-time flag.
3. **`OpenShellPaneBackend`** (feature `openshell`) —
   `OpenShellClient::connect` + `create_sandbox` + `wait_ready`, then the raw
   tonic `ExecSandboxInteractive` bidi stream. Output bytes feed the same
   vt100 `SharedScreen` the PTY backend uses, so pane rendering is one code
   path regardless of whether the far end is a local PTY or a sandboxed
   gRPC stream. `handle_key` translates via `pty::encode_key` into
   `ExecSandboxInput`. The async gRPC stream bridges to the sync render loop
   with a tokio task + mpsc — the same pattern `TurnDriver` already uses
   internally, so no second concurrency model enters the codebase.

The three backends share one authority posture: **only the input router that
owns a pane's `handle_key` call site can write into that pane.** No backend
type exposes a write handle to anything else — the same non-`Clone` linear
capability discipline `cockpit-tmux-multiplexer.md` uses for `PtyWriter`
applies unchanged to `PtyPaneBackend` and `OpenShellPaneBackend`.

---

## The agents manifest — the palette's source of truth

`~/.gila/agents.toml` (parsed by `src/agents_manifest.rs`, pure — no
newt/ratatui/crossterm) is the herdr-style manifest that populates the
command palette:

```toml
[[agents]]
name = "newt"
argv = ["newt"]
description = "newt-agent, in a PTY pane"
# cwd = "/path/to/project"
# env = ["OPENAI_API_KEY"]
# posture = "confined"   # or "ambient"
```

A **missing** file is not an error — it resolves to a built-in default
palette (`$SHELL`, `newt`, `claude`, `codex`), the same fail-safe-full
posture `manifest.rs`'s `capabilities.toml` already uses for its own default.
A **present but malformed** file (empty name, empty argv, a duplicate name,
a malformed `env` entry) fails loud at load time — the palette must never
silently drop or misroute an entry. This mirrors `manifest.rs`'s house
style exactly: `load()` degrades gracefully on absence, `validate()` never
degrades gracefully on nonsense.

The palette itself is a fuzzy list built from manifest entries plus the
built-in "gila chat" and `openshell:<agent>` (feature-gated) options,
surfaced by a prefix-key binding (`keys.rs::Action::PaletteOpen`).

---

## Supervision: watch and herd modes

`src/supervisor.rs` follows `follow.rs`'s pattern exactly, generalized from
"one shell pane" to "any non-chat pane": each backend's `observation()` drain
feeds `TurnDriver::submit_observation` as a `ShellObservation` on a
designated supervisor chat pane — a `Companion`/`Reader`-kind pane, clamped
by the same `read_only_caveats()` lattice `cockpit-tmux-multiplexer.md`
documents, so an unprompted supervisor turn is safe by construction.

Two modes, both default **off**:

- **`watch`** — only the currently focused pane's observations feed the
  supervisor. The lightweight default once supervision is turned on: "tell
  me what's happening in the pane I'm looking at."
- **`herd`** — every pane's observations feed the supervisor, each chunk
  tagged with its source pane id (`ShellObservation::new("pane:<id>", …)`).
  This is the herder's actual job: one operator, N subordinate harnesses,
  one place that sees all of them at once. Volume management reuses
  `cockpit-tmux-multiplexer.md`'s per-flush cap and flood guard verbatim,
  applied per pane rather than per single shell.

**Steering is not part of this ratchet.** gila typing *into* a child pane on
the supervisor's behalf is deliberately deferred (phase 7 below, "the pane-
drive capstone" already named in the tmux-multiplexer doc's roadmap): any
agent-proposed keystroke into a subordinate pane must pass an operator
confirm prompt before it reaches `PtyWriter::write_input` or the OpenShell
equivalent. Until that ratchet lands, supervision is strictly read-only —
the herder watches; it does not yet drive.

Optional, later: herdr-style status detection (idle/working/blocked
heuristics on pane output), and gila self-reporting into a real herdr
session via `herdr pane report-agent` (shell-out only, never a crate dep)
when `HERDR_ENV=1` is set — the same environment-gated integration boundary
the `herdr` skill already uses from the operator side.

---

## Voice: the conversation loop

`src/voice_ui.rs` (new, existing `voice` feature) embeds the `agent-voice`
facade (`../agent-voice`) in-process — one `VoiceStack` per process, created
lazily on first toggle, per its `docs/GILA_PLUGIN.md` embed path B. The
facade pulls no ratatui, so it composes cleanly with the render-thread-owns-
the-tty rule `cockpit-tmux-multiplexer.md` establishes: voice runs on its own
tokio task, never on the render thread.

- **Conversation loop** — a tokio task runs continuous capture → Silero VAD
  segments utterances → `transcribe()` → text routed to the designated voice
  target pane (default: the supervisor chat pane; switchable to whichever
  pane is focused) and auto-submitted, exactly as if typed.
- **TTS** — completed assistant turns are spoken via `say()`/`synthesize()`
  on their own playback task, so a long synthesis never blocks the render
  loop or a second capture segment.
- **Barge-in** — VAD speech-start detected during playback cancels the
  current TTS output and starts a new capture segment immediately. This is
  the one place the loop must actively fight its own echo: playback mutes
  capture by default except for the barge-in energy threshold, per the
  tmux-multiplexer doc's risk list.
- **Controls** — a prefix-key toggle (`Ctrl+B v`) starts/stops the loop;
  the status line shows `listening` / `thinking` / `speaking`; push-to-talk
  hold remains available as a degraded mode when the operator does not want
  always-on capture (a shared workspace, a noisy room). Voice is **muted**
  while a PTY or OpenShell pane is focused, unless explicitly pinned to a
  chat pane — a focused shell pane is not a place stray transcribed speech
  should land as keystrokes.
- Model paths and settings reuse the existing `gila voice` CLI's handling
  unchanged (models under `<data dir>/agent-voice/models`; the documented
  `CARGO_TARGET_DIR`/piper-phonemize build gotcha carries over as-is).

---

## Default entry point: the flip

Bare `gila` currently launches the inline chat path. This design flips that
default: bare `gila` launches the full-screen multiplexer, with a default
layout of one gila-chat pane plus one confined shell pane (cowork parity).

- **`gila code`** and a new **`--inline`** escape hatch keep the previous
  inline behavior reachable on purpose — operators scripting gila, or
  running it somewhere a full-screen TUI is inappropriate, are not forced
  onto the new surface.
- **Non-TTY invocations fall back automatically** to the old inline path —
  a full-screen ratatui app has no meaning against a pipe or a CI log, so
  the fallback is unconditional, not a flag the caller must remember.
- **The ambient-native-shell trust contract is preserved by surface, not
  weakened by the flip**: chat panes minted through `authority.rs` keep
  their existing `PaneKind` posture; PTY panes default Confined; an
  explicit ambient pane still requires an explicit manifest opt-in. The
  flip changes *what greets the user by default*, not *what authority
  anything is granted*.
- `docs/decisions/ambient_native_shell_default.md` gets a default-surface
  note recording the flip and its date, per that doc's own revisit
  discipline.
- `gila cowork`, `gila matrix`, and `gila follow` keep working unchanged —
  `cowork` was already slated to become a preset alias for a cockpit layout
  in `cockpit-tmux-multiplexer.md`; this doc does not pull that forward,
  only records that the flip does not regress any of the three.

---

## Cargo surface

- `agent-voice = { git = ..., optional = true }` under the existing `voice`
  feature, replacing/extending current voice wiring as needed.
- New feature `openshell = ["dep:openshell-sdk"]`; verify the SDK re-exports
  raw tonic clients before adding a matching `tonic` dependency directly.
- Stay pinned to ratatui 0.29 / crossterm 0.28 (newt-tui's pins) and
  portable-pty 0.9 / vt100 0.15 — unchanged from `cockpit-tmux-multiplexer.md`.
  No herdr crate dependency: herdr is a UX reference only, and its vendored
  `[patch.crates-io]` portable-pty must stay out of gila's dependency graph.
- `.cargo/config.toml.template` gets a local-path overlay for OpenShell and
  agent-voice, matching the existing newt overlay pattern.

---

## Phasing (roadmap ratchets, each shippable)

Each phase ships full test coverage at the pure-function layer (backends,
manifest parsing, supervisor routing) with only the raw render loop as an
uncovered carve-out, per the repo's three-layer pattern.

1. **PaneBackend + PTY panes** — `pane_backend.rs`, `PtyPaneBackend`,
   `cockpit_app.rs`. `gila cockpit` opens one confined shell pane;
   split/close/nav/zoom are real. Existing commands untouched.
2. **Chat panes + tabs + palette** — `ChatPaneBackend`; default layout
   chat+shell (cowork parity); `agents.toml` (this doc's manifest); spawn
   `newt`/`claude`/`codex` in PTY panes from the palette; multi-tab.
3. **Default flip** — bare `gila` launches the multiplexer; `gila code` /
   `--inline` and the non-TTY fallback; the decision-doc update.
4. **Supervision** — `supervisor.rs`, watch/herd modes; wiremock-backed
   unit tests mirroring `follow.rs`'s existing test style.
5. **Voice conversation** (`--features voice`) — the conversation loop, VAD
   turn-taking, TTS, barge-in, the toggle + status-line states.
6. **OpenShell panes** (`--features openshell`) — `openshell_pane.rs`, the
   palette's `openshell:<agent>` entries, a `--openshell <url>` flag.
7. **Steering ratchet** — confirm-gated agent-proposed pane input; the
   roadmap's pane-drive capstone, closing the loop this doc leaves open in
   [Supervision](#supervision-watch-and-herd-modes).

---

## Risks (additive to `cockpit-tmux-multiplexer.md`'s list)

- **Version skew** — OpenShell's tonic pin may conflict with the `jupyter`
  feature's dependency set; both stay optional, CI builds the feature
  matrix rather than assuming they compose.
- **Sync render loop vs. async backends** — every backend that bridges an
  async source (gRPC, a future mesh attach) into the sync render loop must
  not block on that bridge; backpressure gets an explicit test per backend.
- **PTY portability** — new integration tests stay Unix-gated, matching the
  existing `tests/pty` pattern.
- **Voice loop echo** — TTS playback can retrigger VAD; mute-during-playback
  except for the barge-in threshold is the mitigation, and it will need
  iteration once real hardware is in the loop.
- **Contract drift vs. newt's `docs/design/tui-panel-system.md`**
  `PaneManifest`/`PaneContext` — `PaneBackend` is kept adapter-friendly on
  purpose so a future manifest wrapper, if newt's design lands first, is an
  adapter, not a rewrite.

---

## Critical files

**Modify** (owned elsewhere during this ratchet — coordinate, do not touch
directly while another workstream is mid-refactor): `src/cockpit.rs`,
`src/keys.rs`, `src/main.rs`, `src/lib.rs`, `Cargo.toml`, `.newt/roadmap.toml`,
`docs/decisions/ambient_native_shell_default.md`.

**Create**: `src/pane_backend.rs`, `src/agents_manifest.rs`,
`src/cockpit_app.rs`, `src/supervisor.rs`, `src/voice_ui.rs`,
`src/openshell_pane.rs`, `~/.gila/agents.toml` (template, not committed).

**Reuse unchanged**: `src/cowork.rs` (terminal scaffold, chat rendering),
`src/pty.rs` (the whole PTY stack), `src/follow.rs` (the observation
pattern, generalized by `supervisor.rs`), `src/authority.rs`, `src/layout.rs`,
`src/manifest.rs` (the sibling `capabilities.toml` parser this doc's
`agents.toml` parser is styled after).

---

## Verification

- **Unit**: `agents_manifest.rs` parse/validate/defaults/template
  (tempfile-backed, mirroring `manifest.rs`'s test style); supervisor
  routing with wiremock; `PaneBackend` trait object dispatch over fake
  backends.
- **Integration**: PTY backend spawning `sh -c` fixtures (existing
  `tests/pty` patterns); `assert_cmd` for the CLI flags and the non-TTY
  fallback; OpenShell tests gated on a server env var so CI without a
  gateway skips cleanly rather than failing.
- **Manual**: `cargo run --bin gila` opens full screen by default; prefix
  splits; palette-spawn `claude` and `newt`; `--features voice` round-trips
  a conversation with barge-in; `--features openshell` against a local
  gateway at `http://127.0.0.1:8080`.
