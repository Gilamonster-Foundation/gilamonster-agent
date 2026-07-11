# Changelog

Gilamonster Agent follows the version line chartered in [ROADMAP.md](ROADMAP.md):
one tagged version per ratchet milestone, **v0.3.1 → v0.3.12**, all under the
`v0.3.x` cockpit line. Milestones 1 & 2 shipped before this scheme and keep
their original git tags **`v0.1.0`** (≡ v0.3.1) and **`v0.2.0`** (≡ v0.3.2);
the live `v0.3.N` tag series runs from `v0.3.3` onward.

## v0.3.3 — Layout (2026-07-11)

The tmux cell tree that the cockpit's panes are laid out on.

- **`src/layout.rs`** (#50, PR #53) — a pure (no newt-*/ratatui) `LayoutTree`
  arena of `Leaf`/`Split` cells with **absolute integer sizes** and a 1-cell
  divider, upholding `sum(children)+(n-1)==parent`. `split` (new pane =
  `(ss+1)/2-1`), `close` (freed `size+1` to one neighbour + single-child
  collapse), `resize_tree` (round-robin ±1 fit, `PANE_MIN=3` floor), `rects`
  (pure walk + clip-at-render), `zoom` (unzoom→mutate→rezoom), geometric
  directional nav with an MRU tie-break, and the even-h / even-v /
  main-vertical presets.
- The hard deliverable ships: a **proptest invariant suite** (400 cases over
  arbitrary op sequences — invariant holds, no overlaps, nav total, area
  conserved) plus 16 unit tests. The proptest earned its keep, catching a
  `close` that round-robin'd freed space instead of handing it to one neighbour.

Next: **v0.3.4 — Cockpit + authority** (`run_cockpit`, N companion `TurnDriver`
panes, `authority.rs`, ambient shell + observe-only proven three ways; #54) —
the first release that seats multiple newt-agents in panes.

## v0.3.2 — Kill the leak (2026-07-11, tagged `v0.2.0`)

The cockpit prefix dispatcher takes over cowork's keystroke path, closing the
live shell leak.

- **`Ctrl+B → 0x02` shell leak closed** (#48, PR #49) — `run_cowork` used to
  encode every shell-focused key straight to the PTY, so a bare `Ctrl+B` wrote
  `0x02` into the user's shell. Every key now passes through
  `keys::KeyDispatcher` first, via a pure `cowork::route_key`: a bare prefix or
  a swallowed post-prefix miss reaches nothing, `Ctrl+B Ctrl+B` (send-prefix)
  injects exactly `0x02`, and ordinary keys still forward to the shell or the
  chat pane. `Ctrl-Q`/`Ctrl-O` stay direct globals, held while a prefix is
  armed (`KeyDispatcher::is_armed`).
- Cockpit actions route to the app where the scaffold supports them (focus →
  `swap_focus`); the rest are consumed-but-no-op until their phases — which
  still closes the leak. 7 pure tests; verified live in tmux.

Next: **v0.3.3 — Layout** (`layout.rs`, the tmux absolute-cell arena tree with
its proptest invariant suite; #50).

## v0.3.1 — Keys & doctrine (2026-07-11, tagged `v0.1.0`)

The first tagged release: the cockpit's decision record and its key
dispatcher, plus the roadmap-as-code foundation.

- **Cockpit ADR** (`docs/decisions/cockpit_tmux_multiplexer.md`, #42, PR #44)
  — the condensed contract from the merged design (PR #37): authority map,
  pane-kind caveat postures, keys/layout fidelity subsets, deferrals. Includes
  the phase-0 verification of newt `agentic/tools.rs` at the pinned rev, which
  found the lattice-deny claim true only narrowly and binds two requirements
  on phases 4–6: fail-loud driver construction while `NEWT_DISABLE_OCAP` is
  set, and treating every MCP mount as outside the caveats lattice.
- **`src/keys.rs`** (#43, PR #45) — the pure tmux prefix-table dispatcher
  (design phase 1): prefix-as-config, crossterm-variance normalization, `-r`
  repeat with the 500 ms lazy deadline, swallow-on-fallback-miss (a
  post-prefix typo never reaches a PTY), send-prefix derived from the
  configured prefix, fail-loud `parse_key_string`/`bind`/`unbind` where
  bindings name `Action`s only — rebinding can never mint authority. 14
  design-mandated tests.
- **Roadmap-as-code** (PRs #40, #41) — `ROADMAP.md` (the v0.3.1→v0.3.12
  charter) and `.newt/roadmap.toml` (the same plan as machine-loadable data,
  exported via newt's `/roadmap export`, #1082). `/roadmap import` on a fresh
  checkout bootstraps the working copy; newt's `/roadmap eval` can now also
  gate nodes on their referenced issues (#1083).

Versioning discipline from here: tag `v0.3.N` when milestone _N_'s last PR
merges. Next: **v0.3.2 — Kill the leak** (dispatcher wired into cowork before
`encode_key`; closes part of #10).
