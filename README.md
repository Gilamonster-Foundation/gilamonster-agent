<p align="center">
  <img src="docs/logos/gilly-256.png" alt="Gilly, the Gilamonster mascot" width="256" height="256">
</p>

# gilamonster-agent

**The full-ambient Gilamonster agent matrix.** It *inherits*
[newt-agent](https://github.com/Gilamonster-Foundation/newt-agent)'s airframe —
chat, agentic coding, identity, tools, and optional object-capability
confinement — and *extends* it into a Hermes/Thoon-style multi-agent matrix.

> newt is the cell; gilamonster-agent is the organism. The extension point is a
> **separate binary**, not a plugin slot — which is why newt stays *opinionated,
> not extensible*.

## Status: v0.4 development — ambient-first on the newt 0.8 airframe

This is the v0.1 structure, and **it builds today.** gila is a **private
binary**, not a published library, so it consumes newt over a **git
dependency** pinned to a `newt-agent` `main` rev — *not* a crates.io version
dep. A binary never needs crates.io, so the git-dep is the correct *permanent*
shape, not a stopgap. (agent-bridle resolves from crates.io — 0.7.x, published
— so the old stub-shell `[patch.crates-io]` mirror is gone on both sides; see
newt-agent's `docs/decisions/agent_bridle_publishing.md` for the history.)

```bash
gila                 # full ambient authority + native host command execution
gila code ./project  # the same coder, rooted at a project
gila --ocap          # opt into newt's configured OCAP confinement
gila matrix          # the extension layer / fleet surface
```

`gila code` hands off to `newt_tui::run_code`. Gila deliberately inverts
newt's launch default: newt remains confined, while Gila's coder starts with
full filesystem, network, and execution authority and uses the native host
command path. Common shell reads are not rewritten to built-ins. The global
`--ocap` flag removes inherited widening switches and restores the configured
newt confinement posture for that invocation.

Cowork, observer, and triage surfaces (`cowork`, `follow`, `hotseat`, and
companion/cockpit panes) remain confined. Cowork's human-owned PTY keeps the
ordinary user environment but strips Newt authority controls and secrets so a
nested agent must make a fresh launch decision. Agent-facing MCP capabilities
remain opt-in; ambient shell authority does not auto-mount them. See the
[ambient-first decision and rollout plan](docs/decisions/ambient_native_shell_default.md).

## Build

```bash
cargo build          # resolves the pinned newt git rev
just check           # the gate: fmt + clippy -D warnings + test
just cov-ci          # coverage gate (>= 80% line floor, inherited from newt)
just install         # gila + OCAP net guard -> ~/bin (or another PATH dir)
just install-hooks   # wire .githooks/pre-push (runs the gate before every push)
```

### Local dev against an in-flight newt checkout (the overlay)

CI builds against the pinned `rev` in `Cargo.toml`. To iterate against a local
`newt-agent` working tree instead, use the **git-ignored** `.cargo/config.toml`
overlay — it `[patch]`-overrides the `newt-*` crates onto local paths:

```bash
just overlay-on      # cp .cargo/config.toml.template -> .cargo/config.toml
# edit the paths in .cargo/config.toml if your newt-agent lives elsewhere
cargo build          # now builds against the local newt tree
just overlay-off     # drop the overlay; back to the pinned git rev (CI-equivalent)
```

`.cargo/config.toml` is in `.gitignore` and can never be committed, so CI is
always reproducible from the pinned rev. Bump the rev in `Cargo.toml` to adopt
a newer newt; keep gila's `agent-bridle-core` version line on the same 0.x
line newt-core resolves, so the two halves agree on the gate types.

## What's inherited (the `newt-*` crates, over git-dep)

- `newt-tui` — the chat + agentic-coding TUI (and, transitively, `newt-core`,
  `newt-inference`, `newt-tools`, `newt-skills`, `newt-mcp-client`).
- `newt-identity` — the per-user `UserKey` → session `AgentKey` → attenuated
  operating-key chain. The whole matrix runs under one capability model.

## What gets extended (the matrix — not yet built)

- Many newt airframes composed over the **agent-mesh** airspace.
- **drake** lifecycle + orchestration.
- The rich settings / dashboard surfaces (ported from newt's git history, where
  the settings TUI was deliberately removed to keep newt lean).

## The split, recorded

newt-agent is deliberately scoped to chat + agentic coding (Codex/Claude-Code
spirit, lean). **Additional features go here.** See `newt-agent#89`.

## Contributing

This project follows the Gilamonster
[Centaur Developer](https://github.com/Gilamonster-Foundation/agents) style:
human/agent teams contributing as a single unit, with a human always in the
loop and agent contributions credited in the git record.

- Rules template: [`rules/AGENTS.md`](https://github.com/Gilamonster-Foundation/agents/blob/main/rules/AGENTS.md)
- Skills: [`rust-tdd`](https://github.com/Gilamonster-Foundation/agents/blob/main/skills/rust-tdd/SKILL.md),
  [`pyo3-wrapping`](https://github.com/Gilamonster-Foundation/agents/blob/main/skills/pyo3-wrapping/SKILL.md)

## Logos

Meet **Gilly**, the Gilamonster mascot — mirrored here from the
[agents](https://github.com/Gilamonster-Foundation/agents) repository at
standard sizes under `docs/logos/` (`gilly-16.png` … `gilly-512.png`).

## License

Apache-2.0.

---

<!-- markdownlint-disable-next-line MD013 -->
Model: OpenAI GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 19:34 EDT | Date: 2026-08-13
