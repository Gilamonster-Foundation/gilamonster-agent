# Gilamonster Agent — the 0.x version line

> Gilamonster is primarily a **UX surface that binds multiple newt-agents
> together in panes.** newt-agent is the airframe (chat, agentic coding,
> identity, tools); gila is the cockpit that seats many of them side by side,
> under one authority map, in front of one human.

This roadmap turns that sentence into a release line: **v0.3.1 → v0.3.12**, one
tagged version per ratchet milestone, all under the **`v0.3.x` cockpit line**
(so a twelve-step epic doesn't burn through twelve minors). The engineering
spine is the merged cockpit design
([docs/design/cockpit-tmux-multiplexer.md](docs/design/cockpit-tmux-multiplexer.md),
PR #37) — its phases 0–10 map to v0.3.1–v0.3.10 — and the line is capped by the
**pane-drive** capstone (v0.3.11–v0.3.12): the point where agents themselves
become first-class operators of the panes. After the capstone, `v0.4.0` opens
the next epic.

> **Numbering note.** Milestone _N_ is `v0.3.N` (patch = milestone number).
> Milestones 1 & 2 shipped before this scheme and remain tagged **`v0.1.0`**
> (≡ v0.3.1) and **`v0.2.0`** (≡ v0.3.2) as historical facts; the live
> `v0.3.N` tag series runs from `v0.3.3` (Layout) onward.

> **Renumbering note (2026-07-29).** The `0.4.x` line opens **early**, ahead
> of the pane-drive capstone: the newt airframe re-pin (0.7.5-line main,
> +1,150 commits — the `[patch.crates-io]` era ended, MSRV 1.88, the
> `Option`-shaped backend config, the partial-trajectory turn contract) plus
> the `gila chain` LangChain surface are a bigger platform step than any
> single cockpit milestone, and the manifest/tag drift (Cargo.toml pinned at
> 0.1.0 through v0.3.3) needed a clean break. Tags v0.1.0/v0.2.0/v0.3.3 stand
> as history; the **unshipped cockpit milestones (v0.3.4–v0.3.12 above)
> continue unchanged inside `0.4.x`** — read "v0.3.N" below as "the Nth
> milestone", now landing as 0.4.x patch/minor bumps.

## Baseline: newt-agent 0.8.0

The line drives off the **newt-agent 0.8.0** airframe — the release carrying
the multi-conversation system (`/start` / `/resume` / `/end`, `live_owners`
claims) and the #1030 "Plans within Plans" roadmap tree
(`Roadmap→Phase→Plan→Task`, objective evaluators, headless `TreeDriver` /
`/roadmap drive`). Gila consumes newt over pinned git revs (four crates
lockstep: `newt-tui` / `newt-core` / `newt-identity` / `newt-mcp-client`);
"0.8.0" here means *that rev line*, bumped deliberately at v0.3.6 (below), not
tracked continuously.

Upstream seam PRs (small, FleetView-4a/4b-style) run as a parallel track in
newt-agent and gate only the milestones that name them:
`TurnDriver::with_tools` (gates v0.3.6), turn delta channel (upgrade whenever
it lands), channel `PermissionGate` (gates the modal half of v0.3.8),
non-blocking cancel (deletes the reaper workaround whenever it lands).

## The version line

| Version | Milestone | Design phase | Depends on / closes |
|---|---|---|---|
| **v0.3.1** _(tagged v0.1.0)_ | **Keys & doctrine.** Cockpit ADR split from the design doc + `keys.rs` (pure prefix-table dispatcher, key-string parser, crossterm variance matrix). Versioning discipline starts here: tag + `Cargo.toml` bump + CHANGELOG on every milestone merge. | 0 + 1 | extends #11 |
| **v0.3.2** _(tagged v0.2.0)_ | **Kill the leak.** Dispatcher wired into the existing cowork loop before `encode_key`; `Ctrl+B` no longer writes `0x02` into the user's shell. | 2 | part of #10 |
| **v0.3.3** | **Layout.** `layout.rs` arena tree, split/resize/zoom algorithms, presets; proptest invariant suite. | 3 | — |
| **v0.3.4** | **Cockpit + authority — first light.** `run_cockpit` with tabs; N concurrent companion chat panes (one `TurnDriver` per tab — *this is the first release that binds multiple newt-agents into panes*); ambient shell pane with the observe-only guarantee proven three ways; `authority.rs` lands in the same PR as the first driver; follow-me. `gila cowork` becomes a preset alias. | 4 | closes #10; extends #11, #24 |
| **v0.3.5** | **MCP actor.** Per-tab `mcp_actor` + `McpProxy`, honestly reporting "tools: pending upstream" over `NoMcp`. | 5 | extends #20 |
| **v0.3.6** | **newt 0.8.x pin-bump + workbench panes.** Lockstep rev bump (own no-feature PR), then `PaneKind::Workbench`: grant-display modal, `workspace_caveats()`, default-deny rendering. | 6 | upstream `with_tools`; extends #11, #20 |
| **v0.3.7** | **Jupyter + browser.** `jupyter.rs` venv/port/runtime-file management + status pane; `browser.rs` open chokepoint with `token=` pre-scrub. | 7 | — |
| **v0.3.8** | **Copy/scroll + escalation.** Copy mode, vt100 scrollback, OSC 52 yank with paste-buffer fallback; permission-escalation modal (grants meet the pane ceiling, expire per request). | 8 | upstream `PermissionGate` (modal half splits out if it lags) |
| **v0.3.9** | **Save & resume.** `Ctrl+B d` confirm + session serde; `--resume` via `with_transcript`; fresh PTYs; honest modal copy. | 9 | — |
| **v0.3.10** | **Fleet tab.** `Tab::Fleet(FleetModel)`; `gila matrix` aliases `cockpit --layout fleet`. The parallel FleetView track (#31–#36) converges here. | 10 | closes #21 |
| **v0.3.11** | **pane-drive tool-set.** The agent-facing pane API, exposed as cockpit MCP tools: `pane_open` / `pane_read_screen` / `pane_write` / `pane_wait_for` / `pane_status` / `pane_rename` / `pane_close`. Every hand-maintained guard in newt's `tmux-drive` skill becomes a structural guarantee: handles are returned at creation (never "active pane" lookup), self-targeting and ambient-shell writes are unrepresentable, `wait_for` replaces sleep-and-capture polling, panes open unfocused. The tmux-drive gotcha list ships as the adversarial regression suite. | capstone | builds on v0.3.4–v0.3.6 |
| **v0.3.12** | **pane-drive skill + capstone.** Bundled `pane-drive` skill (derived from tmux-drive; the "one rule that will bite you" section shrinks to *"the API won't let you"*). Multi-newt orchestration demo: a roadmap-driving newt (`/roadmap drive`, TreeDriver) dispatches Blocked nodes to worker newts seated in cockpit panes via pane-drive. The story this line was built to tell, working end to end. | capstone | v0.3.11 |

## The capstone story (why the line ends where it does)

`tmux-drive` (newt's bundled skill) taught an agent to operate TTY programs
through real tmux — and every one of its hard-won rules is a *convention* the
agent must remember: never target your own pane, guard empty `-t`, sleep
before capture, don't steal focus. The cockpit's whole thesis is converting
those conventions into *structure*: the authority lattice decides which panes
accept agent writes, the non-`Clone` `PtyWriter` makes observe-only a type-level
fact, and the pane API can't express the mistakes the skill warns about.

v0.3.11 builds that API; v0.3.12 teaches agents to use it and closes the loop —
multiple newt-agents, bound together in panes, one of them driving the roadmap
tree that scheduled the others. Gilamonster's diff against tmux-drive *is* the
evidence for the authority-first design.

## Working agreements

- One milestone = one version = a short stack of one-issue/one-PR ratchets;
  tag `v0.3.N` when milestone _N_'s last PR merges. A fix between milestones
  folds into the next milestone's release (the patch slots are the milestones);
  a true can't-wait hotfix takes the next patch number and the pending
  milestones shift up by one.
- Branch lifetime hours-to-days; merge-on-green; delete after merge.
- Pure-function core, thin raw loop; 80% coverage floor; zero warnings.
- This file states *what and why*; the design doc owns *how*. When they
  disagree, fix one of them in the same PR that exposed the disagreement.
