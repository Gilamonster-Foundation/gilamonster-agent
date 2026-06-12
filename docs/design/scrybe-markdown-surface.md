# Design: Scrybe as gilamonster-agent's human-facing Markdown surface

**Status:** idea / design-for-discussion — *not* a commitment to link binaries.
Captured so the shape and its one hard constraint are on record.

## TL;DR

[scrybe](https://github.com/hartsock/scrybe) is an **MCP-native, cross-platform
Markdown editor** ("the document is the conversation") that is *itself an MCP
server, built to be driven by external agents*. gilamonster-agent already
carries newt's MCP **client**. So "the agent opens a live Markdown canvas a
human reads and edits, with edits flowing back" is a **connect-two-things**
problem, not a build-from-scratch one.

gilamonster-agent is the **right home** for it (newt is not): the project's own
thesis is *"newt is the cell; gilamonster is the organism; the extension point
is a separate binary, not a plugin slot,"* and it explicitly names **"rich
settings / dashboard surfaces"** as what the organism adds. A rich GUI surface
belongs on the organism, never on the lean cell.

A single **MONSTER binary** (agent + Markdown GUI in one artifact) is real and
desirable for the **local-first single-user desktop** story — but only as a
**feature-gated desktop build**, because scrybe's UI is Tauri (a system
webview) and the fleet charter forbids GUI system deps in containers / headless
pods.

## What the two things are (the load-bearing facts)

**scrybe** (`pip install scrybe.ai`, also crates.io) is PyO3-style — a Rust core
with Python reach — and is cleanly crate-split:

- `scrybe-core`, `scrybe-render` — document model + Markdown rendering (headless).
- `scrybe-mcp-server`, `scrybe-mcp-client`, `scrybe-rpc` — the agent-drive surface.
- `scrybe-app/src-tauri` — the **Tauri** desktop editor (one crate among many).
- `scrybe-py`, `scrybe-cli`, `scrybe-vcs`, `scrybe-mermaid` — the rest.

The split matters: the *display* (Tauri), the *MCP server*, and the *render
engine* are separate crates. You can take the headless parts without the GUI,
and the GUI is already designed to be driven over MCP.

**gilamonster-agent** is the "organism": `gila code` hands off to the inherited
newt TUI airframe; `gila matrix` is the (stubbed) multi-agent extension layer.
It runs the whole matrix under newt's one object-capability identity model
(`UserKey → AgentKey → attenuated operating key`). It is explicitly the place
the constellation parks rich surfaces that were deliberately kept *out* of lean
newt.

## Why gilamonster-agent, not newt

newt's design rule is "opinionated, not extensible" — the lean cell. A Markdown
GUI in newt would violate that and (worse) drag a Tauri webview into the fleet
airframe that runs headless in containers. gilamonster-agent is the documented
counterweight: a **separate binary** that *adds* the rich surfaces. So:

- **newt** (the cell, the fleet airframe): stays GUI-free. No scrybe.
- **gilamonster `gila`** (the organism, the operator's cockpit): the desktop
  build *may* carry a scrybe Markdown surface.

This is the cleanest resolution of the "but the fleet can't have Tauri"
tension — the tension disappears the moment the GUI lives on the organism's
desktop build and not the cell.

## Three integration shapes (increasing coupling)

### 1. MCP peer — no linking (recommended first step)
`gila` is an MCP **client** of scrybe's MCP server. The agent opens/updates a
document; scrybe renders + lets the human edit; edits flow back as MCP. Zero new
protocol — scrybe already supports exactly this, and gila already has an MCP
client. The two can even sit on **different machines over agent-mesh** (the same
thin-peer pattern as the mobile remote-control design, newt-agent#202). Lowest
effort, highest alignment, works headless-agent + GUI-elsewhere.

### 2. Link the headless crates
Pull `scrybe-core` + `scrybe-render` (+ `scrybe-mcp-client`) into `gila` for
**in-process** Markdown rendering — still no GUI, no system deps. Useful if gila
wants to render Markdown to its own TUI or to a file without a running scrybe
app.

### 3. Link the Tauri GUI — the MONSTER binary
Statically include `scrybe-app` so one `gila` binary both runs the agent loop
and opens the Markdown editor window. This is the true monolith. See the
constraint below.

## The "one MONSTER binary" question

**Feasible? Yes — but it cannot be the *only* artifact.** scrybe-app is Tauri =
a webview = system GUI deps (webkit2gtk on Linux). The airship charter forbids
Tauri system deps in fleet containers, and a headless worker/pod has no display.
A single always-GUI binary therefore won't run where the fleet runs.

The honest, good shape:

- **Feature-gated desktop build.** One codebase; a `gila-desktop` feature (or a
  `gila display` subcommand) compiles the scrybe GUI **in** for the desktop
  artifact and **out** for the lean/headless artifact. The desktop one *is* your
  MONSTER: one download = agentic coder + live Markdown cockpit. The headless one
  stays fleet-safe.
- **PyO3-level "monster."** Both ship Python wheels, so a softer monolith is a
  single `pip install` environment where gila can `import scrybe` and pop a doc —
  same one-install feel without static-linking Tauri.

## Why this is more than a hack — the convergence

The constellation keeps landing on the same idea wearing different UIs:

- scrybe: **"the document is the conversation."**
- newt Phase 15 / gilabot#1887: **"a folder is a conversation."**
- mobile remote-control (newt-agent#202): a terminal-like chat that is a mesh peer.

A Markdown document the agent and human **co-edit live**, edits flowing both ways
over MCP, is that same thesis as a first-class surface. It's the natural home for
gila's `/plan` decomposition, an airship "morning debrief" (airship#14), a design
doc under review, or a long agentic run's narrated output.

## Recommendation

1. **Start at shape #1 (MCP peer).** It delivers ~90% of the magic for ~10% of
   the effort and is the most aligned with how the constellation already
   composes (MCP + mesh, thin peers). Prove "gila drives a scrybe doc a human
   edits, both ways" end-to-end first.
2. **Then, if the local-first desktop story earns it, add the feature-gated
   MONSTER build (shape #3)** as a `gila-desktop` artifact — never replacing the
   lean/headless build.
3. **Keep newt out of it.** The cell stays opinionated and GUI-free.

## Open questions

- **Drive surface:** scrybe's MCP server vs. its `scrybe-rpc` — which is the
  right control channel for an in-process or co-located agent?
- **Identity:** does scrybe authenticate/authorize its MCP/RPC peers? gila runs
  under newt's ocap caveats; ideally a gila→scrybe session is bounded by the same
  `AgentKey` authority, not an unauthenticated localhost socket.
- **Co-edit conflict model:** when both the agent and the human edit the same
  doc, what's the merge/locking story? (scrybe-vcs may already have a stance.)
- **Cross-machine:** is the mesh-peer variant (gila here, scrybe on the
  operator's laptop) worth wiring now, or is localhost-first enough?
- **Packaging:** is the MONSTER a Rust static-link (`gila-desktop` feature) or a
  PyO3 `pip install` bundle — or both, per platform?

## Non-goals

- Putting any GUI or Tauri dep in newt, or in any fleet/headless artifact.
- Forking scrybe — this is consumption (MCP peer or published-crate dep), the
  same "inherits and extends" discipline gila already uses for newt.
- Inventing a new wire format — reuse MCP (and, cross-machine, agent-mesh).
