# Decision: FleetView is gila's full-screen crew-monitor dashboard, realized as `gila matrix`

**Status:** Accepted (decided by Shawn Hartsock, 2026-06-26).
**Date:** 2026-06-26
**Related:**
`newt:docs/decisions/plain_scroller_tui.md` (the load-bearing rule that sends
"panes, live status, dashboards" to gilamonster-agent),
`newt:docs/decisions/lean_rich_tui_morphologies.md` (#527 — newt's LeanTUI /
RichTUI morphologies behind the `InputSurface` seam),
the FleetView design doc (`knowledge:board/papers/fleetview-design.md`).

---

## TL;DR

The **FleetView** dashboard — a live, navigable crew monitor (plan → phases →
agent rows → drill-in) — lives in **gilamonster-agent**, built on the cowork
scaffold gila already ships, and is surfaced as the **`gila matrix`** command
(`--mock` opens it over a canned roster while live sources are built out). It is
a **standalone, alternate-screen sibling subcommand**: it does **not** embed
newt's chat surface, does **not** host newt's REPL, and adds **no**
alternate-screen / pane / dashboard code to newt.

## Why here and not newt (the Revisit trigger)

newt's chat surface is one plain-scroller code path behind the `InputSurface`
seam, with **no** alternate screen, panes, or live dashboards
(`newt:docs/decisions/plain_scroller_tui.md`). That doc is explicit on two
points this decision leans on:

1. **The tier table** assigns "Advanced TUI: panes, live status, dashboards, the
   feature matrix" to **gilamonster-agent** / monitor agents — not newt.
2. **The Revisit trigger:** "if newt genuinely cannot express a needed
   interaction as scrolled lines, write a new decision doc … do not land the
   surface change first."

A left rail + a sortable agent table + arrow-key drill-in **cannot** be
expressed as scrolled inline lines. So, per newt's own rule, it must not land in
newt. This doc is that new decision doc — recorded on the gila side, where the
surface lives.

## Why standalone, not "EnhancedTUI embeds RichSurface"

The literal "gila wraps newt's `RichSurface` and flips to a dashboard mid-chat"
shape is **not** buildable without a behavioral newt change, and was rejected for
Phase 1:

- `newt_tui::run_code` (the only public entrypoint) takes **no** surface
  parameter and builds its own `Box<dyn InputSurface>` internally; the
  surface-selection point (`run_chat`) is **private**.
- `RichSurface` is `pub(crate)` and gated behind the **off-by-default**
  `rich-tui` cargo feature, which gila's `newt-tui` dependency does not enable —
  so `RichSurface` is not even compiled into gila today.

Embedding it would require a new public surface-injection entrypoint in newt-tui
**and** enabling/gating the `rich-tui` feature — a behavioral newt change. We
keep FleetView standalone instead: gila "extends newt" here by **composition
over newt's published crates** (`TurnDriver::poll`, `transcript_lines`, and — in
later phases — a new scheduler observer over `newt-scheduler`/`newt-core`), not
by embedding a private object. The inline chat path stays **100% newt**
(`gila code` → `newt_tui::run_code`). A literal mid-chat flip remains a deferred
option if it ever becomes a real requirement; it is not on the critical path.

## Why `gila matrix`

`Command::Matrix` was the reserved "(not-yet built) extension layer" scaffold.
FleetView **is** the realization of that extension layer — the live multi-agent
matrix the operator watches — so it builds out `matrix` rather than adding a
separate `fleet` command. Bare `gila matrix` keeps a side-effect-free scaffold
notice (no surprise alternate screen); `gila matrix --mock` opens the dashboard.

## Honest metrics (a downstream invariant)

Every per-agent metric the dashboard renders is an `Option`. A metric the
producer cannot report — a remote mesh peer carries no token/tool count over the
wire; a long local turn exposes no sub-turn idle clock — renders as a dim `—`,
**never** a fabricated `0`. We never make the instrument lie about the sky. This
is enforced in `src/fleet.rs` (`fmt_tokens` / `fmt_tools` / `fmt_idle` take
`Option`s) and is why later phases that add live data also add explicit newt
instrumentation rather than inventing the numbers.

## Consequences

- A new `docs/decisions/` directory is established in gilamonster-agent; future
  gila surface decisions live alongside this one.
- FleetView uses the alternate screen, panes, and (from Phase 2) stateful
  `List` / `Table` selection — all of which newt's plain-scroller rule forbids in
  newt, and all of which are correct here, because gila is the tier where they
  belong.
- **gila's own Revisit trigger:** any *further* surface that newt's published
  crates cannot feed must be reconsidered here, not bolted onto newt.

## Implementation phases (summary)

1. **Phase 1 (this PR):** standalone `gila matrix --mock` dashboard over a canned
   roster + this decision doc — zero newt changes.
2. **Phase 2:** navigation state machine + drill-in (stateful `List` / `Table`).
3. **Phase 3:** watch N independently-attached local agents (cowork generalized)
   — zero newt changes; honest `—` for metrics newt cannot yet emit.
4. **Phase 4a–c:** upstream newt scheduler-observer PRs → the first true crew
   monitor (live tokens/tools/phase from `run_crew`).
5. **Phase 5:** remote mesh peers (operator roster, honest degradation).
6. **Phase 6:** freeze / save / replay persistence.
