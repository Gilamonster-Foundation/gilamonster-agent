# Design: `gila cockpit` — a tmux-semantics agent multiplexer, built natively in Rust on newt-core seams

**Status:** design-for-discussion — *not* a commitment to the exact module names
or phase count below. Captured so the shape, the authority model, and the
resolved decisions are on record before any code lands. Supersedes nothing;
extends the `gila cowork` / hotseat epic (#11) and slots the multi-pane
dashboard (#21) into a single host surface.

> "Computer Science has as much to do with Computers as Astronomy does with
> Telescopes." — Edsger W. Dijkstra

The cockpit is **an authority map rendered as a screen**, not a terminal
emulator with an AI bolted on. tmux is the telescope; the thing we are actually
building is a seat where a human and one or more agents share the same
workspace, and where *every distinct authority* — who can type into your shell,
who can call a tool, who can see your scrollback — is declared at construction,
attenuation-only, and visible. We port tmux's key and layout semantics faithfully
not because we love tmux, but because that discipline is exactly what keeps a
mistyped keystroke from ever reaching your real shell.

---

## TL;DR

`gila cockpit` is a single, full-screen ratatui application that gives one TUI
**multiple tabs and panes with tmux prefix-key bindings**, implemented **natively
in Rust** (no real tmux, no embedded tmux, no shelling out). The load-bearing
requirements map as follows:

- **`Ctrl+B c`** → new chat tab (a companion agent pane).
- **`Ctrl+B "`** → split an **ambient shell pane**: the user's real `$SHELL` on a
  PTY. The user types into it; **the agent can see it but can never type into
  it** — enforced three ways (structural, lattice, and by regression test).
- **"follow me as I work in the shell"** → the operator toggles a *follow link*
  (`Ctrl+B f`) between a shell pane and a chat pane; the agent receives the shell
  output as **redaction-gated observations** and the user chats about it in the
  chat pane. The phrase is *discoverable* conversationally, but the tap is always
  **minted by the human keystroke**, never by the model asking for it.
- **Manage a Python venv + Jupyter server** (`gila jupyter …` and a status pane),
  **open browser windows** (a single audited chokepoint), **MCP integration**
  (per-tab tool mounts), and **multiple concurrent chat windows** (one
  `TurnDriver` per chat pane).

The whole thing is **composition over newt-agent's published crates** — it drives
`newt-core`'s cowork seams (`TurnDriver`, `ShellObservation`, `transcript_lines`)
and reuses the airframe's object-capability identity. newt stays the lean cell;
gila is the organism that carries this rich surface, exactly as the
`plain_scroller_tui` rule and the FleetView carve-out already prescribe.

This document is the output of a design competition: three independent designs
(ship-first, architecture-first, capability-first) were scored by a three-judge
panel. The capability-first design won (authority-as-architecture); the
architecture-first design's tmux fidelity and module hygiene, and the ship-first
design's ratchet phasing and conversational discovery, were grafted in.

---

## Why this belongs in gila, not newt (the doctrine check)

newt's chat surface is one plain-scroller path behind the `InputSurface` seam:
no alternate screen, no panes, no dashboards (`newt:docs/decisions/plain_scroller_tui.md`).
That doc's tier table explicitly assigns *"Advanced TUI: panes, live status,
dashboards"* to gilamonster-agent, and its Revisit trigger says: if newt cannot
express an interaction as scrolled lines, write a decision doc **on the gila
side** — do not grow the surface in newt. A tmux-style tabs-and-panes multiplexer
is the canonical case. This is that doc.

We reuse newt strictly through its **published crates and public seams**. We do
**not** embed newt's private `RichSurface` (it is `pub(crate)`, gated behind an
off-by-default `rich-tui` feature, and not even compiled into gila) — the same
rejection FleetView already recorded. The chat widget in each pane is gila's own,
built on `newt-core`'s renderer-agnostic `transcript_lines` and the `TurnDriver`
whose module docs *literally prescribe this gila consumption pattern* and whose
`observation.rs` *names the downstream gilamonster split-pane UI*. The seam was
built upstream for exactly this.

---

## The load-bearing verdict: gila re-implements the chat widget (it does not host newt-tui)

A six-agent deep read of newt-tui settled the one architectural question that
gates everything else:

**`newt_tui::run_code` / `run_chat` cannot be rendered into a `Rect`.** It is a
process-global, blocking REPL: it owns stdout (streaming turn output straight
into real scrollback), toggles raw mode per input read, and the rich variant's
`Viewport::Inline` is pinned to the *real terminal bottom* and rebuilt every
read. There is no seam to draw it into a caller's rectangle, and its ~17k-line
loop holds all session state as locals.

So the verdict is **(c): gila builds its own ratatui chat widget per pane**, over
these `newt-core` seams (all already published on the pinned rev):

| Seam | Shape | Role in the cockpit |
|---|---|---|
| `TurnDriver` | `submit(text)` / `submit_observation(obs)` / `poll() -> TurnStatus` / `cancel()` | One per chat pane. **One turn in flight per driver.** Each turn runs on a dedicated OS thread with a current-thread runtime because `chat_complete` is `!Send`. |
| `transcript_lines(&[MemMessage], width) -> Vec<TranscriptLine>` | renderer-agnostic; carries `role` + `is_first` | Chat-pane rendering; we map `TranscriptLine` → ratatui `Line`. |
| `ShellObservation::new(source, text)` | secret-redaction **by construction**; framed *observation, not instruction*; **never starts a turn** | The only path from a shell pane into a model. |
| `read_only_caveats()` (today in `follow.rs`) | attenuation of `Caveats::top()`: `fs_write`/`exec`/`net` = `Scope::none()`, `max_calls = AtMost(0)` | The lattice clamp for observer/companion panes. |

**Known gaps in the seam (each has a resolution below):**

1. `TurnDriver` hard-codes `NoMcp` and a headless `ChatCtx` → no MCP tools, no
   permission prompts, no token streaming (the whole reply arrives at
   `Completed`), no summarizer. → Resolved by a small upstream PR + a per-tab MCP
   actor; streaming and prompts are parallel-track upgrades.
2. `TurnDriverConfig.caveats` defaults to `Caveats::top()` (**full authority**).
   → Resolved by making `authority.rs` the *only* construction site.
3. `TurnDriver` does **not** persist transcripts and exposes only
   `transcript() -> &[MemMessage]` (no setter). → The cockpit appends to
   `ConversationStore` itself; transcript folding goes through
   `TurnDriver::with_transcript(config, Vec<MemMessage>)` (an Idle-only rebuild),
   never an in-place mutation.
4. `cancel()` **joins** the worker thread and can stall up to the inference
   timeout. → Never called on the render thread; a short-lived reaper thread owns
   it in v1 (a non-blocking-cancel upstream PR later deletes the workaround).

---

## What gila already ships (the ground we build on)

All of these are **real and tested today**, not stubs — the cockpit is an
evolution, not a greenfield:

- **`cowork.rs`** — a working full-screen ratatui split: companion chat (top),
  the user's **real `$SHELL`** on a PTY (bottom, via `portable-pty` + `vt100`),
  `Ctrl-O` focus swap, `Ctrl-Q` quit. `PtyShellSource` already tees PTY output
  into an `ObservationChannel` → `submit_observation`. **The "agent observes the
  user's shell" capability already exists.** Its bricks — `TerminalGuard`,
  `setup_terminal`/`restore_terminal`, `transcript_to_lines`, the `split_panes`
  test style, `PtyChildGuard` — are reused directly.
- **`follow.rs`** — the Tier-A typescript-tail observer, and the home of
  `read_only_caveats()` (the lattice clamp). Its `drive_comment` /
  `FOLLOW_COMMENT_NUDGE` is the basis for opt-in proactive commentary.
- **`pty.rs`** — the PTY plumbing and `encode_key`. **Today this is where the
  `Ctrl+B` leak lives**: `encode_key` maps `Ctrl+B → 0x02` and forwards it to the
  focused shell. Fixing that leak is literally the first code PR.
- **FleetView (`gila matrix --mock`, `fleet.rs`)** — a full-screen dashboard
  built on the repo's proven **three-layer pattern**: a pure `FleetModel::apply_key
  -> Step` state machine, a pure render function, and a thin raw loop. This is the
  pattern every new surface must follow to stay above the **80% line-coverage
  floor**. `FleetModel`'s pure shape is exactly the `Tab` seam — which is why
  FleetView can later become `Tab::Fleet` inside the cockpit (Phase 9).
- **`capabilities.rs` / `manifest.rs`** — the capability manifest,
  `Gate::authorize`, and agent-bridle Landlock-confined spawns. Jupyter and (a
  future) agent-invocable browser-open plug into this pattern.
- **`venv.rs`** — venv *resolution* only (no Jupyter yet); the Jupyter manager is
  new code that reuses its injected-`exists`/`env` test pattern.

The observe-only guarantee as built in cowork is **structural**: the only call
site of `PtyShell::write_input` is the user-keystroke routing arm. This design
*hardens* that into a compile-time guarantee and *adds* the lattice clamp that
cowork's own chat driver is missing today.

---

## The authority map (the spine of the design)

Every authority in the cockpit is enumerated and owned by exactly one holder.
This table is the contract; the UI and the phasing both follow from it.

| Authority | Sole holder | Enforcement |
|---|---|---|
| The TTY (stdin/stdout/termios) | The render thread, **exclusively** | No interrupt-watch stdin thread; no `block_in_place` on the render thread; tracing routed to a file, never stderr-on-tty. |
| A PTY **write** handle | The cockpit input router only | `PtyShell::split() -> (PtyReader, PtyWriter)`; `PtyWriter` is a **non-`Clone` newtype**. No agent-facing struct (driver, channel, actor) has a field of that type. |
| A PTY **read** tap | `ShellObservation` (redaction by construction) | The only path from shell to model; active only while a follow link is on. |
| A chat `TurnDriver` | `authority::caveats_for(PaneKind)` | The **only** `TurnDriverConfig::new(` call site in the codebase, asserted by a repo-grep test. No unclamped driver can exist. |
| A per-tab MCP mount | An actor task owning the connections | Proxied over mpsc; replaces the multi-tab-breaking `NEWT_CONFIG` env overlay. |
| The Jupyter server | A manifest capability | `Scope::only` exec + `fs_write`; never `pip install` on the agent path. |
| Browser open | `browser.rs::open_url` chokepoint | User keypress = bare spawn (human authority); agent invocation = `Gate::authorize`-gated, **deferred out of v1 entirely**. |

The confused-deputy failure mode is *the* thing this cockpit must not ship. The
structural rule that closes it: **an agent that can speak unprompted must be
structurally unable to act unprompted.** Proactive commentary is therefore only
ever granted to zero-tool companion drivers.

### Pane kinds and their authority

| Pane kind | Prefix | What it is | Authority posture |
|---|---|---|---|
| **chat-companion** | `c` (default) | A chat pane with no tools. Receives follow-me observations; eligible for proactive commentary. The "follow me as I work" seat. | `read_only_caveats()`: `fs_read = All`, `fs_write`/`exec`/`net` = `none()`, `max_calls = AtMost(0)`, `NoMcp`. Unprompted turns are safe by construction. |
| **chat-reader** | (grant) | Middle posture: read-only tools allowed, bounded call budget. | `fs_read` scoped, read-only MCP tools, `exec`/`net` = `none()`, bounded `max_calls`. |
| **chat-workbench** | (grant) | Tool-capable chat pane. **Created only by an explicit grant modal that displays the caveat grant before construction** — never a second creation key (see decision f). | `fs_read`/`fs_write` scoped to workspace + session scratch; `exec` **held at `none()` until the permission gate lands** (lattice-deny, not policy-deny); `net = none()`. Per-tab MCP proxy mounted. |
| **shell-ambient** | `"` | The user's real `$SHELL` on a PTY. | **No driver at all.** Write side = the non-`Clone` `PtyWriter` in the input router. Read side = redaction-gated observations while follow is on. |
| **jupyter-status** | `j` | Passive status pane: server state, port, notebook dir, plain-text URL, `o` to open. | No driver, no PTY. `o` = user keypress = bare spawn. |
| **fleet** (Phase 9) | (layout) | FleetView embedded as `Tab::Fleet(FleetModel)`; absorbs `gila matrix`. | Zero drivers, zero writes; honest-metrics law (a missing metric renders as a dim `—`, never a fabricated `0`). |

### The observe-only guarantee, proven three ways

1. **Structural** — no agent-reachable type holds a `PtyWriter`. The write handle
   is a non-`Clone` linear capability owned by the input router. This is the
   compiler enforcing the guarantee, replacing cowork's review-enforced call-site
   audit. (Mirrors `newt-core` `session.rs` `AttachRole::Observer`, which
   structurally cannot submit.)
2. **Lattice** — every follower/companion driver is clamped by
   `read_only_caveats()`; `exec`/`net`/`fs_write` are `Scope::none()`,
   `max_calls = AtMost(0)`.
3. **Behavioral regression test** — a tool-capable agent is prompted *"type `ls`
   into the shell"*; the test asserts the PTY input byte stream is **unchanged**
   and the attempt was denied. Plus: *"a swallowed post-prefix typo writes zero
   bytes to the fake PTY"* and *"`Ctrl+B Ctrl+B` writes exactly `0x02`"*.

**Blocking pre-task for the authority phase** (a graft the whole panel agreed on):
a 30-minute read of `newt-core` `agentic/tools.rs` to confirm that *every*
side-effecting built-in tool in a headless driven turn is actually caveat-gated
when `permission_gate` is `None`. The lattice-deny claims above must not be
documented as true until this is verified against the pinned rev.

---

## tmux key semantics (ported faithfully — the dispatcher *is* the authority router)

`src/keys.rs` ports tmux's `server_client_key_callback` state machine. It imports
**neither `newt-*` nor `ratatui`/`crossterm`** (it owns a `KeyCombo` type; a thin
adapter converts at the boundary) so a future `gila-mux-core` crate extraction is
mechanical.

```rust
struct KeyCombo { code: KeyCode, mods: Mods }           // own types, no crossterm
enum TableId { Root, Prefix, Copy }
struct Binding { action: Action, repeat: bool }
enum KeyDisposition { Consumed(Action), Forward, Swallow }

struct KeyDispatcher {
    tables: HashMap<TableId, HashMap<KeyCombo, Binding>>,
    current: TableId,
    prefix: KeyCombo,                 // CONFIG, compared before any table lookup
    repeating: bool,
    repeat_deadline: Option<Instant>,
}
fn on_key(&mut self, k: KeyCombo, now: Instant) -> KeyDisposition
```

The rules that matter, ported exactly:

- **Prefix is config, checked before any table lookup** — not a root binding.
  Matching `prefix` (or `prefix2`) switches to the `Prefix` table and consumes
  the key.
- **Normalization is a real trap.** Fold `Ctrl+letter` to lowercase; strip
  `SHIFT` on printable `Char` (crossterm reports `"` as `Char('"')+SHIFT` on some
  terminals and `Char('B')+SHIFT+CTRL` on others). A dedicated **crossterm
  variance test matrix** is a Phase-1 deliverable. Drop `KeyEventKind::Release`.
- **Non-repeat match** executes then resets to `Root`. **`-r` (repeat) bindings**
  (arrows, resize) execute and *stay* in the `Prefix` table with a 500 ms lazy
  deadline; a non-repeat key pressed during repeat re-resolves in `Root`.
- **Swallow-on-fallback-miss.** A miss falls back to `Root`; if `Root` also
  misses *after a fallback*, the key is **swallowed** — a post-prefix typo
  **never reaches the shell PTY**. This is the single most important safety rule
  in the port, and it is regression-tested against a fake PTY.
- **`Ctrl+B Ctrl+B` = send-prefix** injects a literal `0x02` into the focused
  PTY (derived from the configured prefix at table-build time) — this is how you
  type a literal `Ctrl+B` into your shell or a nested tmux.
- **Paste bypasses the dispatcher** entirely (`Event::Paste` routes straight to
  the focused pane).

**v1 binding subset** (hardcoded default table, tmux-verbatim keys):
`c "` `%` `n p l` `0-9` `o ;` `x &` `z` `d` `[` `w` `q` `f` (follow toggle) `u`
(open last surfaced URL — global, works from any pane), `-r` arrows (focus pane),
`-r C-arrows` (resize 1), `-r M-arrows` (resize 5), `C-b` (send-prefix).

**Rebindability**: `~/.gila/cockpit.toml` `[keys]` with a ported
`parse_key_string` (`"C-b"`, `"M-Left"`, `"\""`), `unbind` support, and round-trip
tests. **Bindings can only name `Action`s, never shell strings** — so rebinding
can never mint authority. The `KEYC_ANY` wildcard is deferred, with its revisit
trigger recorded: a future *capture table* (a password-entry shield or a
follow-me key-capture mode).

---

## tmux layout semantics (absolute-cell tree — enough fidelity to feel right)

`src/layout.rs` ports the tmux cell tree. Also `newt-*`/`ratatui`-free (owns a
`Geom`/`Rect` type; adapter at the boundary).

```rust
struct LayoutTree { cells: Vec<Cell>, root: CellId }     // arena, parent indices
enum CellKind { Leaf(PaneId), Split { dir: Dir, children: Vec<CellId> } }
// ABSOLUTE integer sizes, never weights/Percentage constraints.
// 1-cell border gap between siblings: invariant sum(children)+(n-1) == parent.
fn rects(&self, term: Rect) -> Vec<(PaneId, Rect)>       // pure
```

Ported algorithms (per the deep-read pseudocode): `split` (new pane gets
`(ss+1)/2 - 1`), `close` (give `size+1` to one neighbour, then collapse
single-child parents), `resize_check`/`resize_adjust` (round-robin cell
distribution on terminal resize), `resize_tree` with **clip-at-render** when the
tree legitimately exceeds the terminal, `zoom` as a root swap with the strict
**unzoom → mutate → rezoom** discipline, and **geometric directional nav** (edge
adjacency across the 1-cell border, MRU tie-break). `PANE_MIN = 3` (chat panes
need more than tmux's 1). Presets `even-h` / `even-v` / `main-vertical`
(main-vertical is the natural follow-me shape: shell left, chat right).

**A `proptest` invariant suite** is a hard deliverable: after arbitrary
split/close/resize sequences, `sum(children)+(n-1) == parent` holds, no pane
overlaps, directional-nav is total, offsets are re-derivable, and close-then-reopen
restores total area.

**Explicitly NOT ported** (the local checkout is post-3.5 master with non-stock
features): floating panes, scrollbars, pane-status borders, full-size splits, and
tmux's textual custom-layout strings (we serialize the tree with serde instead).

**Sequencing honesty**: the very first shippable cockpit (Phase 3) may ship a
*fixed* per-tab layout (generalized `split_panes`) so `c` and `"` work on day one;
`%` stays **unbound** until the real tree lands (an unbound prefix key is safely
swallowed — strictly better than a key that does the wrong thing). We do **not**
ship `%` aliased to a stacked split. The tree (`layout.rs`) then lands as its own
phase before `%` is bound.

---

## Concurrency topology (the ledger, no lies)

- **One process, one multi-thread tokio runtime**, built at startup by
  **replicating** newt-cli's 3-line builder (16 MiB thread stacks — the Windows
  clap-overflow guard). We **replicate, not import**: `newt-cli` is the binary
  crate and is *not* in gila's pinned dep set (gila pins only `newt-tui`,
  `newt-core`, `newt-identity`, `newt-mcp-client`); importing it would add a fifth
  crate to the lockstep pin for three lines of code.
- **The render thread owns the tty exclusively.** Alt-screen ratatui `Terminal`
  under `TerminalGuard`; `crossterm::poll(50ms)`; `KeyEventKind::Release`
  filtered; tracing → `~/.gila/state/cockpit/log`. No stdin interrupt-watch
  thread, no `block_in_place` on the render thread — a blocked render thread
  freezes *all* tabs and panes.
- **Turns**: one `TurnDriver` per chat pane; each turn already runs on its own OS
  thread + current-thread runtime (`chat_complete` is `!Send`), so N panes need no
  shared async host. The frame loop `poll()`s each driver non-blocking.
- **Cancel**: `Esc` sets the pane to `Cancelling` and hands the driver to a
  short-lived **reaper `std::thread`** (`cancel()` joins the worker and can stall
  up to the inference timeout). **Never on the render thread.** This is required
  in v1, not an upstream luxury.
- **MCP**: one **actor task per tab** owning its `Vec<ConnectedServer>`
  (connected off-thread with a `connecting…` pane state — connect is sequential,
  ~20 s/request). Command channel `mpsc<(qualified_name, args, oneshot<String>)>`;
  the actor **serializes calls per connection** because `newt-mcp-client` silently
  drops mismatched-id replies. `McpProxy` (`Clone` + `Send`) impls `McpTools` over
  the channel. The proxy/actor **lands ahead of the upstream seam** (wired to
  `NoMcp` until then, showing an honest *"tools: pending upstream"* status), so
  enabling tools is a one-line change on the pin bump.
- **fd hygiene**: `O_CLOEXEC` on PTY master fds (a *verify-or-fcntl* task —
  confirm portable-pty 0.9's actual behavior, else set it explicitly) plus a
  `mark_fds_cloexec`-style sweep before the first actor spawns, so PTY masters
  never leak into MCP children (a leaked master means the PTY never EOFs and the
  reader thread never joins).
- **No `std::env::set_var` inside the cockpit** — it is process-global and breaks
  multi-tab. `Config::resolve` runs exactly once per workspace at startup (it
  publishes a `OnceLock` scratch dir + token atomics); recorded v1 constraint:
  **one workspace per cockpit process**. Per-tab MCP server lists are composed
  **in memory** (resolved config servers + `manifest.agent_exposed()` entries),
  passed by value into the tab's actor. The `NEWT_CONFIG` temp-file overlay
  survives only on the *exec-a-foreign-session* paths (`gila code` / `gila
  hotseat`), documented as the boundary.
- **Persistence**: `ConversationStore` constructed once, `Clone`d
  (`Arc<Mutex<Connection>>`) per tab; the **cockpit appends turns** after
  `Completed` (the driver does not persist). The session state file stores
  conversation **ids + serde layout only — never transcripts** (the store is the
  single transcript home); atomic tmp+rename autosave on tab create/close/rename
  and on turn completion.

---

## Follow-me (the requirement, resolved)

**Discovery is conversational; minting is a keystroke.** When the user says
*"follow me as I work in the shell,"* the companion agent replies with the exact
keybinding, a `/follow` chat command performs the same human-initiated toggle, and
a status hint (*"press `Ctrl+B "` first"*) appears if no shell pane exists yet.
But the link itself is only ever created by the human keystroke — the model
cannot request a tap into existence.

**The toggle**: `Ctrl+B f` toggles a `FollowLink { shell_pane, chat_pane }`
between the most-recent ambient shell pane and the active chat pane. It is
**visible state** — both panes' borders show a `👁 follow` badge (plain-text
`FOLLOW` fallback), the shell pane title reads `[following → tab:pane]`, and the
status line names the link. *An invisible tap on your shell is a trust failure
even when it is technically safe.* Default is **off**, per-link, never global,
never persisted across restarts. `Ctrl+B f` cycles `off → follow → follow+comment`.

**Mechanics**: per frame, `follow_tick` drains the shell pane's new output,
strips ANSI escapes (model legibility), and batches on **newline-quiescence**
(flush at ≥250 ms idle or ≥4 KiB) → `ShellObservation::new("pty", chunk)` →
`submit_observation`. Redaction is automatic and framed *observation, not
instruction*; it **never starts a turn** — the model sees the accumulated
observations on the user's next message.

**Two independent bounds on runaway output** (both grafted):
- **Per-flush cap**: tail-truncate any single flush to 8 KiB with an honest
  `[… N bytes trimmed]` head marker (bounds one flush).
- **Flood guard**: if shell output exceeds ~128 KiB/min (e.g. `cargo build`
  under follow), **auto-suspend the link with a visible notice** rather than
  silently drowning the context and the redactor (bounds ingestion rate).

**Proactive commentary** (opt-in, companion-only). Reuses `follow.rs`
`drive_comment`, nudged on quiescence with a ≥30 s min-interval, and gated by the
**commentary guard triple**: fire only when the driver is `Idle` **and** the
target pane's input buffer is **empty** **and** a quiet period has elapsed — so
the agent never interrupts you mid-compose. Commentary is **only ever offered on
zero-tool companion panes** (the structural confused-deputy closure).

**Transcript growth** (the driver has no summarizer): a gila-side pure fn
`fold_observations(transcript: &[MemMessage]) -> Vec<MemMessage>` runs **between
turns** when accumulated observations exceed `OBS_BUDGET` (64 messages or 16 KiB
redacted text): the oldest half collapses into one **honest elision marker**
(*"earlier shell activity elided (N chunks; commands seen: …)"*), never a
fabricated summary. Because `TurnDriver` has no transcript setter, the fold is
applied via **`TurnDriver::with_transcript` (an Idle-only rebuild)** — with a
regression test that a `Running` driver is *never* rebuilt, and a pin-bump test
comparing `transcript()` before/after every rebuild to catch upstream driver
state silently lost across the swap.

**Honest redaction statement** (verbatim in `gila cockpit --help` and the ADR):
`redact_secrets` is *value-shape based* (`sk-`, `ghp_`, `AKIA`, JWT, `Bearer`,
private-key blocks, `key=value` list). It protects against **known credential
shapes** reaching the model. It does **not** protect against novel secret
formats, business data, hostnames, or paths in your scrollback. **The visible,
default-off, human-only follow toggle is the real control.** gila adds a
`token=<hex>` URL pre-scrub (Jupyter) before `ShellObservation` as
defense-in-depth.

---

## Jupyter + browser

**Jupyter** is both a CLI subcommand (`gila jupyter {up|status|stop|restart}
[dir]`) and a pane (`Ctrl+B j`). The manager (`src/jupyter.rs`) is native Rust
(pure planning fns with injected `exists`/`env`, the `venv.rs` test pattern) that
ports the gilabot lifecycle **with its bugs fixed**:

- **gila owns venv creation** — a dedicated `~/.gila/jupyter-venv`
  (`python3 -m venv` + the venv's own `pip install jupyterlab`; never PATH pip).
- **Pass `--port` explicitly** (probe a free port) *and* read the actual
  port/token from Jupyter's runtime-dir `*.json` (gilabot's assume-8888 bug).
- **Persist `{pid, port, dir, token_file, started_at}`** at
  `~/.gila/state/jupyter/<dir-hash>.json` + a `runs.jsonl` audit line (the
  git-tend `repo_state.json` pattern). `stop`/`restart` operate **from the state
  file only** — no blind 8888–8891 sweeps.
- **`chmod 600` the token file**; write the per-dir config with
  `open_browser=False`, `ip=127.0.0.1`. `allow_network` is opt-in (binds
  `0.0.0.0`); the third-party IP-echo probe is **dropped** in v1 (ambient net
  authority for cosmetic output).
- **Capability wiring**: manifest entry `name = "jupyter"`, `expose = "cli"` by
  default. If the operator flips `expose` to agent, the agent gets
  `start`/`stop`/`status` **only — never `pip install`** (dependency mutation
  stays human), via a confined spawn: `Gate::authorize` with
  `exec = Scope::only([<venv>/bin/jupyter])`,
  `fs_write = Scope::only([notebook_dir, runtime_dir, state_dir])`. Recorded
  honestly: the agent-bridle *net* axis is advisory-only today, so the listen
  address is policy, not enforcement.

**Browser** — a single `src/browser.rs::open_url` chokepoint: xdg-open →
sensible-browser chain, **`spawn()` not `status()`** (a wait blocks the event
loop), stdout/stderr nulled (raw-mode safety), never-fail print-URL-to-status-line
fallback, zero new crates. **URL to the user**: CLI output renders OSC 8 via
`newt_tui::terminal_hyperlink` (already in the dep tree); in-TUI it is **plain URL
text** (ratatui buffers cannot carry OSC 8; terminals auto-linkify) plus the `o`
key. **Authority split**: a user keypress (`o` on the Jupyter pane, or `Ctrl+B u`
opening the last URL found by pure `last_url_in(lines)`) = bare spawn (human
authority needs no leash). **Agent-invocable browser-open is NOT shipped in v1**
(recorded deferral): xdg-open dispatches arbitrary URI schemes to arbitrary
handlers — a canonical confused deputy. When wanted, it lands as a workbench-only
MCP tool `cockpit__open_url`, `Gate::authorize`-gated with `exec =
Scope::only([/usr/bin/xdg-open])` + an `http`/`https` + host allow-list caveat
(default: `localhost:<jupyter-port>` only).

---

## Detach / persistence

`Ctrl+B d` in v1 = **save-and-exit**, not tmux detach. It serializes the session
(layout tree via serde, pane kinds + titles + follow config, per-chat conversation
ids) to `~/.gila/state/cockpit/session-<workspace-hash>.json`, then tears down
cleanly (`PtyChildGuard` kills shells, actors drop, `TerminalGuard` restores).
`gila cockpit --resume` rebuilds chat panes via
`TurnDriver::with_transcript(config, transcript)` (transcripts pulled from
`ConversationStore`) and spawns **fresh PTYs** (cwd restored). Jupyter is
independently supervised via its own state file and **intentionally survives**
cockpit exit. The `d`-key confirm modal states plainly: *"shell processes do not
survive detach in v1."*

**True client/server detach** (a daemon keeping PTYs alive) is **explicitly
deferred with an ocap rationale**: a persistent background process holding live
*write* handles to your shells plus a control socket is a larger standing
authority than the entire v1 cockpit — it needs socket auth, a server-side
authority model, and an attach protocol. `newt-core` `session.rs`
(`SessionRegistry`, `AttachRole::{Driver, Observer}`, `OutputChunk` replay) is the
ready-made vocabulary for that server when the mesh-attach work matures; we defer
*to that seam* rather than invent a private daemon protocol now. No lock-in: the
session file is plain JSON, transcripts live in the standard store, and losing the
state file loses layout only.

---

## Surface & naming

- **New subcommand `gila cockpit`** (`Cli::Cockpit { resume: bool, layout:
  Option<String> }`). *"mux"* names the mechanism; *"cockpit"* names what the user
  needs to see (the Dijkstra rule) — one seat, many instruments, each with visible
  authority. Issue #11 already calls the epic *"hotseat cockpit."*
- **`gila cowork` becomes a thin preset alias** = `gila cockpit --layout cowork`
  (one tab: companion chat over ambient shell, follow armed), deprecated in help
  after one release. `CoworkApp` is absorbed; its bricks are reused.
- **`gila follow`** stays as the non-TUI, scriptable Tier-A typescript CLI — a
  different tool, kept.
- **FleetView / `gila matrix`** stays a **sibling for v1** (its Phase 3–6 track
  proceeds independently), then is **absorbed as `Tab::Fleet`** in Phase 9
  (`FleetModel`'s pure `apply_key`/render *is* the `Tab` seam, so the wrap is
  mechanical). `gila matrix` then becomes `gila cockpit --layout fleet`,
  **closing #21** as cockpit+fleet composition rather than a third full-screen
  surface.

### Module layout (single crate — no workspace split)

The repo is one crate; the 80% llvm-cov floor and clippy gate are repo-wide; the
`.cargo/config.toml.template` newt path-overlay assumes one crate. A split buys
nothing until compile times hurt. New modules, all three-layer compliant:

```
src/keys.rs         pure dispatcher + KeyCombo + parse_key_string   (no newt/ratatui/crossterm)
src/layout.rs       arena LayoutTree + 8 algorithms + rects()       (no newt/ratatui/crossterm)
src/authority.rs    caveats_for(PaneKind) + driver_config()  ← the ONLY TurnDriverConfig::new site
src/cockpit/mod.rs  CockpitModel { tabs, active, dispatcher, modal, follow_links } + apply_key/apply_action
src/cockpit/pane.rs enum Pane { Chat, Shell, Jupyter, (Fleet P9) } + per-kind ctors → authority.rs
src/cockpit/chat.rs ChatPane: TurnDriver + input + scroll + pump    (lifted from cowork.rs)
src/cockpit/render.rs render_cockpit_frame(&CockpitModel, &mut Frame)   (TestBackend snapshots)
src/mcp_actor.rs    per-tab actor + McpProxy (McpTools over mpsc) + bridge over public newt-mcp-client
src/jupyter.rs      pure planning fns + JupyterManager
src/browser.rs      open_url chokepoint
main.rs::run_cockpit  the one thin raw-loop carve-out (the only by-design-uncovered code)
```

`authority.rs` absorbs `read_only_caveats()` from `follow.rs` (re-exported for
compat). `tests/authority_seam.rs` greps that `TurnDriverConfig::new(` appears
**only** in `authority.rs`.

---

## Streaming & permission prompts (v1 stance)

**Spinner-then-full-reply is acceptable for v1.** Streaming is UX, not authority,
and must not block the authority work; the spinner shows *elapsed time* honestly
(no fabricated token counts — a missing metric is a dim `—`). Two parallel-track
upstream PRs upgrade this later: an optional **delta channel**
(`OutputChunk`-shaped, so a future mesh attach reuses the wire type) and a
**channel-based `PermissionGate`**. Until the gate lands, the v1 stance is
**default-deny**: out-of-caveat calls fail and the denial renders in-transcript;
**workbench `exec` is held at `Scope::none()` (lattice-deny, not policy-deny)** and
config that tries to widen `exec` **fails loud at parse time**. When the gate
lands, prompts render as a cockpit modal whose grants **meet the pane ceiling**
(attenuation-only) and **expire with the request** — never persisted, never
exceeding the pane's declared authority.

---

## Upstream newt-agent PR track (small seams, FleetView 4a/4b pattern)

| PR | What | Blocks |
|---|---|---|
| **`TurnDriver::with_tools`** | Thread an `McpTools` impl into `run_one_turn` (a factory `Box<dyn Fn() -> Box<dyn McpTools + Send> …>`), replacing hard-coded `NoMcp` (~10 lines in the driver; the real gila-side cost is the ~150-line bridge over public `newt-mcp-client` regardless of side). | gila Phase 5 (workbench tools). Phases 1–4 ship `NoMcp`, unblocked. |
| **turn delta channel** | `TurnDriverConfig.on_delta: Option<Sender<TurnDelta>>` from the headless stream path; wire shape mirrors `session.rs` `OutputChunk`. | Parallel-track; upgrades rendering whenever it lands. |
| **channel `PermissionGate`** | A `Send` gate handle (request → oneshot, deny-on-timeout) accepted by a driven turn. | gila Phase 7 (escalation modal). |
| **non-blocking cancel** | Split `cancel()` into `request_cancel()` + reap-on-`poll()`. | Parallel-track; deletes gila's reaper-thread workaround. |
| **summarizer seam** | Expose the existing summarizer on `TurnDriverConfig` for driven transcripts. | Parallel-track, low priority; gila's `fold_observations` is adequate. |
| **`run_code` takes `Config` by value** | Retire the `NEWT_CONFIG` `set_var` overlay in the whole-process hand-off paths (`gila code`/`hotseat`). | Housekeeping; the cockpit never uses the overlay, but the seam should die everywhere. |

The `src/turnhost.rs` fork (a copy of `run_one_turn`) exists **only on a
contingency branch** if the `with_tools` PR stalls — **not merged to main behind a
feature gate.** A dormant full-`ChatCtx` authority path in-tree is a standing
footgun the doctrine does not need; it also freezes against `ChatCtx`'s ~40
churning fields, which is exactly why the seam belongs upstream.

---

## Phasing (ratchet PRs — hours-to-days each)

Each phase keeps all logic in pure lib functions (the thin raw loop is the only
coverage carve-out), ships regression tests, and is one issue / one PR.

0. **Decision doc.** Split a condensed `docs/decisions/cockpit_tmux_multiplexer.md`
   ADR from this design (authority map, pane-kind caveat table, fidelity subsets,
   deferrals). One-file PR, FleetView pattern. *(Also: the blocking `tools.rs`
   read.)* — extends #11.
1. **`keys.rs`** — dispatcher + normalization + key-string parser. Pure.
   *Tests:* crossterm variance matrix; swallow-after-fallback; repeat stay/expiry;
   non-repeat-during-repeat reroute; send-prefix after rebind; paste bypass;
   fail-loud bad config. — NEW.
2. **Wire the dispatcher into the *existing* cowork loop before `encode_key`.**
   This kills the live `Ctrl+B → 0x02` leak in week one as its own small PR, on
   the already-tested surface. *Tests:* "swallowed typo writes zero bytes to the
   fake PTY"; "`Ctrl+B Ctrl+B` writes exactly `0x02`". — closes part of #10.
3. **`layout.rs`** — arena tree + 8 algorithms + presets. Pure. *Tests:* the
   `proptest` invariant suite; degenerate sizes `0..=6`; zoom discipline; nav MRU
   tie-break; layout-larger-than-terminal clamp. — NEW.
4. **Cockpit shell + authority seam.** `CockpitModel`/`Pane`/`ChatPane`/render +
   `run_cockpit`; companion chat + ambient shell panes; `PtyShell::split()` +
   non-`Clone` `PtyWriter`; **`authority.rs` with `caveats_for` landing in the
   *same PR* as the first per-tab driver** (no window of unclamped drivers);
   follow-me toggle + batching + flood guard + `fold_observations`; `gila cowork`
   becomes the preset alias. *Tests:* `apply_key`/`apply_action` suites; render
   snapshots; **the three-way observe-only proof**; authority-seam grep test;
   `fold` elision honesty; append-after-`Completed`. — closes #10 remainder,
   extends #11 and #24.
5. **`mcp_actor.rs`** — per-tab actor + `McpProxy` + bridge, wired to `NoMcp`
   with an honest *"tools: pending upstream"* status; CLOEXEC sweep; drop-reaps
   children. *Tests:* fake-transport per-connection serialization; tab-close
   reaps; proxy is `Send`; mismatched-id regression. — NEW; extends #20.
6. **newt pin-bump PR** (four crates lockstep + agent-bridle `[patch.crates-io]`
   rev re-synced byte-for-byte to newt's lock) as its **own no-feature PR**, then
   **workbench panes**: `PaneKind::Workbench` via the grant-display modal;
   `workspace_caveats()`; default-deny denial rendering. *Tests:* grant-modal
   state machine; caveat meet with operator preset; end-to-end fake-MCP call;
   denial-renders. — extends #11/#20; depends on upstream `with_tools`.
7. **Jupyter + browser.** `jupyter.rs` (venv create, `--port` fix, runtime-file
   truth, state files, `600` token, no sweeps) + CLI + status pane + `browser.rs`
   + `o`/`Ctrl+B u` + `token=` pre-scrub + manifest entry. *Tests:* pure planning
   fns; state-file round-trip; runtime-file parsing; no-blind-sweep regression;
   `600`-mode; `last_url_in`; opener fallback. — NEW.
8. **Copy/scroll mode + escalation modal.** Copy table, chat scroll offsets,
   vt100 scrollback view, line-wise select + **OSC 52 yank with an internal
   paste-buffer fallback** (matters inside an outer ssh/tmux); permission
   escalation modal when the gate PR lands (grants meet the pane ceiling, expire
   per request). *Tests:* scroll/selection state machine; OSC 52 encoding; modal
   grant-attenuation property test (granted ≤ ceiling always). — NEW; the modal
   half depends on the upstream gate PR (splits if it lags).
9. **Save-and-exit + resume.** `Ctrl+B d` confirm + session serde;
   `--resume` via `with_transcript`; fresh PTYs; honest modal copy. *Tests:*
   round-trip; transcript-restore equivalence; corrupt-state fail-loud-and-fresh.
   — NEW.
10. **FleetView as a tab.** `Tab::Fleet(FleetModel)`; `gila matrix` aliases
    `cockpit --layout fleet`. *Tests:* nested `apply_key` routing; render parity
    vs standalone. — **closes #21**; extends the FleetView track.

**Known follow-up (not a blocker), recorded so it isn't discovered in the field:**
`transcript_lines` wraps by *char count*, so CJK/emoji-heavy shell observations
overflow pane `Rect`s. A gila-side width-aware wrap (keeping the
`TranscriptLine` `role`/`is_first` vocabulary) is planned.

---

## Non-goals / deferrals (recorded, with rationale)

- **True client/server detach** (a daemon holding live shell write handles) —
  defer to the `newt-core` `session.rs` mesh-attach seam; it is a larger standing
  authority than the whole v1 cockpit.
- **Agent-invocable browser-open** — confused-deputy hazard; ships later as a
  gated `cockpit__open_url` MCP tool with a host allow-list.
- **Cowork over the mesh (Tier C)** — out of scope per epic #11.
- **Floating panes, scrollbars, tmux custom-layout strings** — not ported.
- **`KEYC_ANY` wildcard / capture tables** — deferred; revisit trigger is a
  password-entry shield or follow-me key-capture mode.
- **Multi-workspace in one cockpit process** — `Config::resolve` publishes
  process-global scratch/token state; one workspace per process in v1.
- **Embedding newt's private `RichSurface`** — rejected (same as FleetView).
- **A `gila-mux-core` crate extraction** — `keys.rs`/`layout.rs` are kept
  dependency-pure so the extraction is *possible and mechanical*, but it is not
  done now (nothing needs a separate publish cadence yet).

---

## Open questions

- **The `tools.rs` verification** (Phase 0 blocker): are all side-effecting
  built-in tools genuinely caveat-gated under `permission_gate: None` at the
  current pin? The lattice-deny guarantee rests on this.
- **`redact_secrets` coverage of `token=`**: if the pinned `redact_secrets`
  already catches Jupyter's `token=<hex>`, the gila pre-scrub downgrades to a
  belt-and-suspenders regression test and the upstream redaction PR shrinks.
- **Delta-channel vs mesh `OutputChunk`**: should the streaming seam and the mesh
  attach seam be *the same* wire type from day one, or converge later?
- **Grant-modal ergonomics**: is a modal per workbench-creation the right ceremony,
  or should tool-grant be a per-first-tool-call prompt once the gate lands?
- **PR #30 (FleetView Phase 2) merge order**: Phases 1–3 here touch `keys.rs`/
  `layout.rs` (new files) but Phase 2 touches `cowork.rs`/`main.rs`; sequence to
  avoid churn against the in-flight FleetView branch.

---

## Provenance

This design was produced by a structured multi-agent process: a six-agent deep
read of the tmux C source, newt-agent (`newt-tui`/`newt-core`/`newt-mcp-client`),
and gilamonster-agent; then three independent competing designs (ship-first,
architecture-first, capability-first) scored by a three-judge panel
(systems-correctness, operator-ux, security-ocap). The capability-first design
won 2–1; its authority architecture was adopted as the spine, with the
architecture-first design's tmux fidelity, module purity, flood guard, and
proptest suite and the ship-first design's ratchet phasing, conversational
follow-me discovery, and pin-bump hygiene grafted in. Factual corrections the
panel caught (replicate rather than import `build_cli_runtime`; fold via
`with_transcript` not a non-existent setter; keep the turn-fork off `main`; hold
workbench `exec` at lattice-deny until the gate lands) are incorporated above.
