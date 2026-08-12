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
| **1 — Foundation & Core Git** | Command registry, dispatch skeleton, shell-delegate fallback, `git commit`/`git-tend` Rust-native | 8 | ☑ implemented (2026-08-12, branch `feat/gila-parity-phase-1-foundation-core-git`) | TBD (code branch ready, not yet pushed) |
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

## Phase 1 completion log (2026-08-12)

Branch: `feat/gila-parity-phase-1-foundation-core-git` (3 commits: delegate fallback, Rust-native git, e2e tests). **Not yet pushed** — awaiting operator approval.

Shipped:

- **Shell-delegate fallback** (`src/delegate.rs`, `src/lib.rs`, `src/main.rs`): clap `external_subcommand` catch-all routes any unported `gila <cmd> [args…]` to `run_delegate`, which resolves the *other* `gila` on `PATH` (skips our own exe — no recursion), prints a stderr deprecation-style warning naming the resolved gilabot path, execs it, and exits with the child's status. **Every gilabot command works from day one.** 4 unit tests.
- **`gila git commit -m MSG [--path P]`** (`src/gila_git.rs`): stage `-A` + commit via libgit2 (`git2`), no subprocess. Clean-tree / empty-repo returns the "nothing to commit" non-error carve-out, matching the Python engine's commit-step handling.
- **`gila git tend [--config P] [--dry-run] [--profile P]`**: profile-driven repo maintenance, config-compatible with the operator's existing `git-tend.yaml` (same schema as `gila-plugin-git-tend`). Profile steps run through the git CLI exactly like the Python engine — deliberate choice: fetch/pull/push/porcelain semantics are subtle, and the parity goal is identical behavior, not a libgit2 rewrite of git. Includes a POSIX-shlex-like `shell_split` (fixes quoted `-m` messages being whitespace-split). 7 unit tests + 4 e2e CLI tests against real temp repos.

Test state: 266 lib tests pass, cli.rs 17/18 pass. The one failure (`capabilities_run_engages_the_confined_path_when_the_manifest_marks_it`) is **pre-existing and environment-dependent** (Seatbelt sandbox + caps venv fixture) — it fails identically on clean HEAD, unrelated to this work. `cargo clippy --all-targets` clean.

Deliberately deferred within Phase 1 scope:

- **Command registry** as a separate module: the existing clap `Command` enum already *is* the typed registry; a redundant indirection layer was skipped. Dispatch skeleton = the `match` in `main.rs` + the `External` catch-all.
- **pyo3 Python bridge** (mentioned in the original plan doc): not needed for Phase 1 — shell-delegate requires no embedded interpreter. Deferred to the phase that actually vendors Python (per the PR tracker + handoff, which are the operator-approved operational docs).
- Python gilabot's multi-mode `bulk-commit` (interactive wizard / YAML staging / auto) and the wider git-tend plugin surface (pr / agentic fix / workspace branches / board): the Phase-1 tend loop covers the deployed backup/refresh/publish profile flow; the interactive and forge-integrated surfaces delegate to Python gilabot for now.

Definition-of-done status for Phase 1: all four boxes met *except* the `--help` snapshot-parity check (not yet captured — recommended as the first task of Phase 2).

---

Model: nvidia/moonshotai/eccn-kimi-k3-max-preview | Harness: newt-agent v0.8.0 | Operator: Shawn Hartsock | Time: 18:30 EDT | Date: 2026-08-12
