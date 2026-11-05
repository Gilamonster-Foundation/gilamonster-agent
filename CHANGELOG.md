# Changelog

Gilamonster Agent follows the version line chartered in [ROADMAP.md](ROADMAP.md).
The original charter was one tagged version per ratchet milestone,
**v0.3.1 → v0.3.12**, under the `v0.3.x` cockpit line (milestones 1 & 2 keep
their historical tags **`v0.1.0`** ≡ v0.3.1 and **`v0.2.0`** ≡ v0.3.2). As of
2026-07-29 the **`0.4.x` line opens early** on the re-pinned newt-0.8.0
airframe (see the v0.4.0 entry and ROADMAP's renumbering note); the unshipped
cockpit milestones continue inside 0.4.x.

## v0.5.0 — Jupyter, the cockpit runtime, and installers (2026-11-05)

Requires Rust 1.91 to build. Prebuilt installers for every release from
here on; the in-process Python bridge still needs a source build.

- **`gila jupyter`** (PR #79, fixes #82): an opt-in command family behind
  the non-default `jupyter` feature — `execute`, `start`, `status`, `list`,
  `stop`, `bootstrap` — with a Pixi-first launcher, a durable server
  registry under `GILA_HOME`, process-group lifecycle, and authenticated
  readiness and shutdown. The bound endpoint is learned from protected
  runtime metadata, never from the requested port. Three waits are tunable
  with unchanged defaults (`GILA_JUPYTER_REGISTRY_LOCK_TIMEOUT_SECS`,
  `GILA_JUPYTER_STARTUP_TIMEOUT_SECS`, `GILA_JUPYTER_LIST_TIMEOUT_SECS`).
  Verified on native Windows: private-ACL ownership, pipe inheritance,
  socket blocking mode, directory replacement and junction handling all
  fixed before merge.
- **Herder cockpit phase 1** (PR #86): the live resources behind each pane —
  a `PaneBackend` trait with chat, PTY and failed-pane backends, the
  runtime that owns the backend map and prefix dispatcher, globally unique
  pane tokens, and the `~/.gila/agents.toml` palette manifest (parsed, not
  yet wired in). A chat pane mints its driver only through
  `authority::driver_config` and construction cannot fail; a PTY spawn
  error becomes a failed pane instead of taking the cockpit down.
- **Installers on every tag** (PR #97, #98): Linux x86_64 (tar.gz, `.deb`,
  `.rpm`, built on Ubuntu 22.04 with a glibc ≤ 2.35 guard), macOS arm64
  (tar.gz, Homebrew formula `gila.rb`, OpenSSL linked statically) and
  Windows x86_64 (zip, Chocolatey `.nupkg`). Bridge-less builds: Python-
  backed command groups delegate to a Python `gila` on PATH.
- **Push hook scope** (PR #96): tag-only, delete-only and docs-only pushes
  skip the local gates (CRAFT-14); CI remains the full gate.

## v0.4.0 — Ambient airframe on newt v0.8.6 (2026-11-04)

The first release where `Cargo.toml`, the tag, and `gila --version` agree —
v0.1.0–v0.3.3 were tagged with the manifest still reading 0.1.0. It is also
the first GitHub Release of the repo. **Requires Rust 1.91** to build.

- **Ambient everywhere; OCAP removed** (PR #90). Every `gila` surface runs
  under the operator's own host filesystem, network, and command authority on
  the native shell, frozen once at startup. The `--ocap` opt-in, the bundled
  `newt-net-guard`, the direct agent-bridle dependency, and the confined
  spawn in `gila cap run` are gone; the manifest's `confined` key is ignored.
  `follow` and the companion pane stay observers by product posture — a zero
  tool budget — not by confinement; `hotseat` is a tool-capable triage mode
  with a bounded call budget. The shadow flight recorder stays armed:
  observability is not policy. Newt remains the confined airframe; Gila
  trades confinement for pragmatism.
- **newt airframe re-pinned to v0.8.6** (PR #93): the four `newt-*` git deps
  move from `be5647e5` to `a7f7d598` with newt's vendored agent-bridle
  0.8.0-rc.6 restated as a `[patch.crates-io]`. MSRV 1.88 → 1.91. For users:
  config migration notices now print on stderr, a context-overflow turn
  reports as `context_exceeded` in the solve contract, and transient backend
  errors are retried with backoff.
- **Hybrid CLI: Rust-native where it pays, Python where it exists** (PR #70,
  #71, #72, #73). `git commit` writes the commit through libgit2 in-process;
  `git tend` is Rust orchestration over the Git CLI; the utility and
  worktree commands are Rust-native; nine command groups
  (confluence, jira, slack, mcp, assistant, doc, calendar, review, pagerduty)
  run in-process through a pyo3 bridge to the installed Python gilabot (set
  `PYO3_PYTHON` at build time); everything else delegates to the Python CLI
  with a one-line stderr notice. Routing per command: `docs/COMMANDS.md`;
  switching from the Python `gila`: `docs/MIGRATION.md`.
- **Exact build identity** (PR #75): `gila --version` and `gila version`
  append Gila's 12-character Git commit to the package version, matching
  newt's `0.4.0 (<commit>[-dirty])` provenance shape.
- **PATH-safe Python fallback** (PR #75): installing Rust Gila ahead of a
  pyenv shim no longer permits a Rust → shim → Rust delegation loop. Each hop
  carries a recursion guard and continues to the real Python gilabot later on
  PATH; `GILABOT_BIN` selects an exact fallback when older Rust builds coexist.
- **Reproducible builds:** `Cargo.lock` is committed (PR #87).
- **Backend config is explicit** (PR #63, #85): an unset model fails in
  `follow`/`cowork` instead of probing (an omitted backend kind defaults to
  OpenAI), and a backend error mid-turn shows as a sticky `Failed` in the
  cockpit instead of a successful turn; `follow` treats it as "no comment".
- **`gila chain <question…>`** (PR #64, landed via #66): a langchain-rust
  `LLMChain` over the same newt backend seam `follow`/`cowork` use. An
  exploration surface confined to one module.
- **Cockpit and FleetView previews** (PR #59, #30, #60): a runnable
  tmux-semantics multiplexer scaffold (`gila cockpit`), FleetView navigation
  and drill-in over mock data (`gila matrix --mock`), and a `gila scrybe`
  configuration-validation scaffold. Previews only: no live pane drivers and
  no live scrybe connection yet.
- **Terminal-Bench path** (PR #77, #78): a native `gila solve` headless
  evaluator and Harbor 0.20 adapter that honors each task container's
  working directory; the scoreboard moved to `docs/terminal-bench.md`
  (PR #88).
- **CI repairs** (PR #65): the coverage upload is non-fatal and the Windows
  job passes again.

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
