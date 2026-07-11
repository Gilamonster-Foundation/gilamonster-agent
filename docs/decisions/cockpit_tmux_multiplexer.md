# Decision: `gila cockpit` — a native tmux-semantics multiplexer, authority-first

**Status:** Accepted (design PR #37 merged 2026-07-09; this ADR is the condensed
contract — ROADMAP.md v0.1.0, issue #42).
**Date:** 2026-07-11
**Related:**
`docs/design/cockpit-tmux-multiplexer.md` (the full design this condenses),
`docs/decisions/fleetview_full_screen_dashboard.md` (the carve-out pattern),
`newt:docs/decisions/plain_scroller_tui.md` (why this lives in gila, not newt),
`ROADMAP.md` (v0.1.0–v0.12.0; cockpit = phases 0–10 → v0.1.0–v0.10.0).

---

## TL;DR

One full-screen ratatui app: multiple tabs + panes with tmux prefix-key
bindings, **built natively** (no real tmux, no embedded tmux, no shelling out),
composed over newt-core's **published** seams (`TurnDriver`,
`ShellObservation`, `transcript_lines`) — never newt's private `RichSurface`.
`Ctrl+B c` = new chat tab; `Ctrl+B "` = ambient shell pane (the user's real
`$SHELL`; the agent can see it, can never type into it); `Ctrl+B f` =
human-minted follow link feeding redaction-gated observations to a chat pane.

The design spine is **authority-first**: every authority is enumerated and has
exactly one holder. The confused-deputy failure is the thing this cockpit must
not ship; the closing rule is **an agent that can speak unprompted must be
structurally unable to act unprompted** — proactive commentary only ever goes
to zero-tool companion drivers.

## The authority map (the contract)

| Authority | Sole holder | Enforcement |
|---|---|---|
| The TTY | The render thread, exclusively | No stdin side-threads; tracing to file, never stderr-on-tty. |
| PTY **write** handle | The cockpit input router only | `PtyShell::split()`; `PtyWriter` is a **non-`Clone` newtype**; no agent-facing type has such a field. |
| PTY **read** tap | `ShellObservation` (redaction by construction) | Only path from shell to model; active only while follow is on. |
| A chat `TurnDriver` | `authority::caveats_for(PaneKind)` | The **only** `TurnDriverConfig::new(` call site, asserted by a repo-grep test. |
| Per-tab MCP mount | An actor task owning the connections | Proxied over mpsc; replaces the `NEWT_CONFIG` env overlay. |
| Jupyter server | A manifest capability | `Scope::only` exec + fs_write; never `pip install` on the agent path. |
| Browser open | `browser.rs::open_url` chokepoint | User keypress = bare spawn; agent invocation **deferred out of v1**. |

## Pane kinds and their caveat postures

| Kind | Key | Posture |
|---|---|---|
| chat-companion | `c` (default) | `read_only_caveats()`: fs_read=All, fs_write/exec/net=none, max_calls=AtMost(0), NoMcp. Eligible for proactive commentary — safe by construction. |
| chat-reader | (grant) | Scoped fs_read, read-only MCP, exec/net=none, bounded max_calls. |
| chat-workbench | (grant modal only) | fs_read/fs_write scoped to workspace+scratch; **exec held at none() until the upstream permission gate lands** (lattice-deny, not policy-deny); net=none; per-tab MCP proxy. |
| shell-ambient | `"` | **No driver at all.** Write = the non-`Clone` `PtyWriter`; read = redaction-gated observations while follow is on. |
| jupyter-status | `j` | No driver, no PTY; `o` = user keypress = bare spawn. |
| fleet | (layout) | Zero drivers, zero writes; honest-metrics law (missing metric = dim `—`, never a fabricated 0). |

**Observe-only is proven three ways:** structural (no agent-reachable type
holds a `PtyWriter`), lattice (`read_only_caveats()` clamps every
follower/companion driver), and a behavioral regression test (a tool-capable
agent prompted to type into the shell writes zero bytes to a fake PTY).

## Fidelity subsets

**Keys (`src/keys.rs`, design phase 1 / issue #43).** Ports tmux's
`server_client_key_callback` state machine; imports neither `newt-*` nor
ratatui/crossterm (own `KeyCombo`; boundary adapter). Ported exactly: prefix is
config checked before any table lookup; normalization (fold Ctrl+letter to
lowercase, strip SHIFT on printable chars, drop key-release events); non-repeat
match resets to Root; `-r` bindings stay in Prefix with a 500 ms lazy deadline;
**a miss after fallback is swallowed** (a post-prefix typo never reaches the
shell PTY — the single most important safety rule); `Ctrl+B Ctrl+B` =
send-prefix (literal 0x02, derived from config at table-build time); paste
bypasses the dispatcher. v1 binding subset: `c " % n p l 0-9 o ; x & z d [ w q
f u`, `-r` arrows / C-arrows / M-arrows, `C-b`. Rebinding via
`~/.gila/cockpit.toml [keys]` with a ported `parse_key_string` — **bindings
name `Action`s only, never shell strings**, so rebinding can never mint
authority.

**Layout (`src/layout.rs`, design phase 3).** tmux's absolute-cell arena tree
(never weights): split `(ss+1)/2 - 1`, close gives `size+1` to a neighbour and
collapses single-child parents, round-robin resize, clip-at-render,
unzoom→mutate→rezoom, geometric directional nav with MRU tie-break,
`PANE_MIN = 3`, presets even-h / even-v / main-vertical. A `proptest` invariant
suite is a hard deliverable. Until the tree lands, `%` stays **unbound**
(swallowed) — never aliased to a wrong split.

## Phase-0 verification: newt `agentic/tools.rs` lattice-deny under `permission_gate: None`

Verified 2026-07-11 against the pinned rev `81488ef` (the design's phase-0
blocking pre-task). Verdict: **the lattice-deny claim is TRUE only narrowly,
FALSE in general** — two bypasses at that rev are independent of the
permission gate:

1. **`NEWT_DISABLE_OCAP=1`** (also `--disable-ocap` / `--yolo`): `run_command`
   skips the caveat-confined shell and dispatches on the plain host shell —
   `exec = Scope::none()` is **never consulted** (`tools.rs:751` + `:272`; the
   `exec_floor` clamp defaults to `None`, and `exec_floor_permits(None, _)` is
   unconditionally true, `tools.rs:300-302`).
2. **MCP-dispatched tools** route before the built-in match and "carry no
   Caveats leash in this build" (`tools.rs:672-677`) — any side-effecting MCP
   tool bypasses the fs/exec/net lattice entirely.

On the pure built-in paths the claim holds bit-for-bit with `gate = None`:
`write_file`/`edit_file` gate on `fs_write` (`tools.rs:840`/`:936`),
`read_file`/`list_dir` on `fs_read`, `web_fetch` and (un-bypassed)
`run_command` dispatch under the caveats. Also noted: **`max_calls` is
enforced in the agentic loop, not in `tools.rs`** — the executor enforces only
the scope axes.

**Consequences (binding on phases 4–6):**
- The cockpit **must refuse to construct any driver while `NEWT_DISABLE_OCAP`
  is set** in its environment (fail-loud at `authority.rs`, regression-tested)
  and must not propagate it to child processes.
- Companion/reader guarantees stand as designed (they are `NoMcp`; bypass 2
  cannot reach them). Workbench panes must treat **every MCP mount as outside
  the lattice** — the per-tab proxy (design phase 5) is the only tool surface,
  and its grant modal is the authority ceremony, until an upstream MCP caveats
  leash exists.
- The zero-tool posture (`max_calls = AtMost(0)`) relies on the driven loop,
  not the executor — keep the behavioral observe-only regression test (design
  §"proven three ways") as the backstop.

## Deferrals (recorded, with rationale)

- True client/server detach → the `newt-core` `session.rs` mesh-attach seam.
- Agent-invocable browser-open → later, as a gated `cockpit__open_url` MCP tool.
- Cowork over the mesh (Tier C) → out of scope per epic #11.
- Floating panes, scrollbars, tmux custom-layout strings → not ported.
- `KEYC_ANY` / capture tables → revisit trigger: password shield or key-capture.
- Multi-workspace per process → v1 is one workspace per process.
- Embedding newt's private `RichSurface` → rejected (FleetView precedent).
- `gila-mux-core` crate extraction → kept *possible* (dependency-pure modules),
  not done.

## Sequencing note

Cockpit phases 0–1 (this ADR, `keys.rs`) create new files only and do not
conflict with the in-flight FleetView Phase 2 branch (PR #30). Phase 2
(dispatcher into cowork) touches `cowork.rs` and must sequence against it.
