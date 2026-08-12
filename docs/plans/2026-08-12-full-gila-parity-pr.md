# PR: Full Gila Command Parity — Phases 1–4

**Tracking issue**: #68
**Full plan**: [`docs/plans/2026-08-12-full-gila-parity-plan.md`](docs/plans/2026-08-12-full-gila-parity-plan.md)
**Branch strategy**: one branch per phase, merged in order; this document tracks the roll-up.

## Summary

Port all 50+ `gila` (gilabot, Python) subcommands to `gilamonster-agent` (Rust) using a hybrid strategy:

| Approach | Where | Why |
|---|---|---|
| **Rust-native** | Git ops, file transforms, MCP client, content pipeline | Performance, type safety, no runtime dep |
| **Python-vendored** | Confluence/JIRA API adapters, LLM provider fan-out | Complex external SDKs not worth re-implementing |
| **Shell-delegate** | `gila` fallback for unported commands during transition | Zero-gap parity from day one |

## Phase tracking

| Phase | Scope | Commands | Status | PR |
|---|---|---|---|---|
| **1 — Foundation & Core Git** | Command registry, dispatch skeleton, shell-delegate fallback, `git commit`/`git-tend` Rust-native | 8 | ☐ not started | — |
| **2 — Knowledge & Content** | `log prompt`, `log activity`, `doc export`, `meeting create`, `content` CRUD | 12 | ☐ not started | — |
| **3 — External Services** | `confluence *`, `jira *`, `mcp-client *`, `calendar *` | 15 | ☐ not started | — |
| **4 — LLM & Agent** | `llm` fan-out, `agent` orchestration, `morning`/`top5` roll-ups | 15+ | ☐ not started | — |

## Definition of done (per phase)

- [ ] All commands in scope respond to `gila <subcommand>` via gilamonster-agent
- [ ] Each command either: (a) runs Rust-native, (b) delegates to vendored Python, or (c) shells to system `gila` with a deprecation warning
- [ ] `--help` output matches gilabot's argparse output (snapshot-tested)
- [ ] Unit tests for dispatch logic; integration tests for Rust-native commands
- [ ] `cargo clippy` clean, `cargo test` green
- [ ] Plan document updated with phase completion date

## Chunking strategy

Overnight work sessions will pick up phases in order. Each phase is an independent PR against `main`. The shell-delegate fallback (Phase 1) ships first so that **every** gilabot command works immediately — later phases replace delegates with native implementations incrementally.

---

Model: nvidia/moonshotai/eccn-kimi-k3-max-preview | Harness: newt-agent v0.8.0 | Operator: Shawn Hartsock | Time: 15:21 EDT | Date: 2026-08-12
