# Changelog

Gilamonster Agent follows the version line chartered in [ROADMAP.md](ROADMAP.md).
The original charter was one tagged version per ratchet milestone,
**v0.3.1 → v0.3.12**, under the `v0.3.x` cockpit line (milestones 1 & 2 keep
their historical tags **`v0.1.0`** ≡ v0.3.1 and **`v0.2.0`** ≡ v0.3.2). As of
2026-07-29 the **`0.4.x` line opens early** on the re-pinned newt-0.8.0
airframe (see the v0.4.0 entry and ROADMAP's renumbering note); the unshipped
cockpit milestones continue inside 0.4.x.

## v0.4.0 — Re-pinned airframe + the LangChain surface (unreleased)

The first release where `Cargo.toml`, the tag, and `gila --version` agree —
v0.1.0–v0.3.3 were tagged with the manifest still reading 0.1.0.

- **Ambient-first launch contract:** bare `gila` and `gila code` start with full
  filesystem/network/exec authority and native host-shell dispatch. Global
  `--ocap` restores newt's configured confinement; cowork, follow, hotseat,
  cockpit/companion, utility, and delegated commands remain confined. Ambient
  actions retain newt's append-only shadow-OCAP recorder.
- **Exact build identity:** `gila --version` and `gila version` append Gila's
  12-character Git commit to the package version, matching newt's
  `0.4.0 (<commit>[-dirty])` provenance shape.
- **PATH-safe Python fallback:** installing Rust Gila ahead of a pyenv shim no
  longer permits a Rust → shim → Rust delegation loop. Each hop carries a
  recursion guard and continues to the real Python gilabot later on PATH;
  `GILABOT_BIN` selects an exact fallback when older Rust builds coexist.
- **newt airframe re-pinned** to the 0.8.0-line main (`8cde29f`, 2026-08-13).
  The four direct newt dependencies move atomically; Gila also adopts newt's
  typed MCP admission witness and frozen startup-authority contract.
- **Upstream drift adopted** (PR #63): `McpServerEntry` trust/enabled fields;
  `NamedPermissionPreset.fs_read`; `gila capabilities check` spawns under the
  operator's real confinement leash (`Config::mcp_probe_caveats`, never
  `top()`); `BackendConfig.model`/`kind` are `Option` — follow/cowork fail
  loud on an unset model instead of probing.
- **Turn-contract fold** (PR #63): newt now reports an errored turn as
  `Completed` carrying `outcome.error` (the partial trajectory survives).
  cowork folds that into the sticky `Failed` the cockpit shows; follow treats
  it as "no comment". Previously a backend 500 rendered as a successful turn.
- **`gila chain <question…>`** (PR #64, landed via #66): a langchain-rust
  `LLMChain` (system frame + human template) over the same newt backend seam
  follow/cowork use — endpoint, model, and token from `Config::resolve()`,
  nothing in code. The dependency is confined to `src/chain.rs` as an
  exploration surface (upstream last published 2024-10) with a cheap exit.
- **CI honesty repairs** (PR #65): the coverage job's lcov upload is
  non-fatal (an org artifact-quota hit had been failing jobs whose gate
  passed), and the scrybe parent-dir test uses `env::temp_dir()` so the
  Windows job passes again.

Release mechanics: tag `v0.4.0` on main after PRs #65/#66 merge; first
GitHub Release of the repo.

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

---

<!-- markdownlint-disable-next-line MD013 -->
Model: OpenAI GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 10:11 EDT | Date: 2026-08-14
