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

## Rust-native rewrites completion log (2026-08-12)

Branch: `feat/gila-parity-phase-3` (4 commits, one per batch). **Pushed**; draft PR **#72** (base `main`, stacked on #70/#71). This is the implementation plan's **Phase 3: Rust-Native Rewrites** — it graduates the medium/low-complexity commands out of the shell-delegate fallback into pure-Rust `src/gila_*.rs` modules, cutting across the tracker's knowledge/external commands (the deterministic, non-networked ones).

Shipped (22 commands, logic unit-tested in the library, thin `main.rs` arms own only HOME resolution + file/subprocess effect):

- **Batch 1** — `version`, `daily`, `ideas`, `todos`, `projects`, `board`, `cache`.
- **Batch 2** — `logs`, `prompt` (list/show/create), `commit-msg`, `completion`, `init`, `update`.
- **Batch 3** — `meeting`, `top5`, `standup`, `checkpoint`, `insights`, `dev`, `wsl`.
- **Batch 4** — `log` (`activity collect`, `prompt create`), `worktree` (list/add/remove).

Design choices:

- **git2 for reads** (`checkpoint`, `insights`, `log activity`) — no subprocess; **`git` CLI for worktree mutations** (add/remove) for behavior parity.
- **`top5`/`standup`** ship the deterministic markdown scaffold only; the interactive interview stays in the assistant/pyo3 layer.
- **No new heavy deps**: `completion` generates a static bash/zsh script (no `clap_complete`); date handling uses the `date` binary (no `chrono`).

Test state: 338 lib tests pass (incl. 67 new `gila_*` unit tests across the batches); `tests/cli.rs` 17/18 pass — the one failure is the **pre-existing macOS Seatbelt sandbox flake** (`capabilities_run_engages_...`), reproduced at Phase-1 HEAD pre-pyo3, unrelated to this work. End-to-end smoke tests verified against a scratch `$HOME` (file effects), real git history (`insights`/`log activity`), and 21 real repos (`checkpoint`).

Remaining from the implementation plan's Phase 3 list (deferred, still shell-delegate): `content` (the tracker maps it to Phase 2's content CRUD — has a networked/publication surface), and the `assistant`/`gemini`/`ollama`/`review`/`doc`/`confluence`/`jira`/`slack`/`pagerduty`/`calendar`/`mcp` high-complexity commands (Phase 2 pyo3-routed per #71, not Rust-native targets).

Model: nvidia/moonshotai/eccn-kimi-k3-max-preview | Harness: newt-agent v0.8.0 | Operator: Shawn Hartsock | Time: 19:46 EDT | Date: 2026-08-12

---

Model: nvidia/moonshotai/eccn-kimi-k3-max-preview | Harness: newt-agent v0.8.0 | Operator: Shawn Hartsock | Time: 18:30 EDT | Date: 2026-08-12
