# Overnight Session — Gila Parity Phases 1–4

**Kickoff date**: 2026-08-12
**Tracking issue**: Gilamonster-Foundation/gilamonster-agent#68
**Plan doc**: `docs/plans/2026-08-12-full-gila-parity-plan.md`
**PR tracker**: `docs/plans/2026-08-12-full-gila-parity-pr.md`

## How to resume

Each phase is an independent chunk. Start with the first phase whose
`Status` in the PR tracker is `☐ not started`. One phase = one PR = one
branch, merged in order.

| Phase | Branch | Base |
|---|---|---|
| 1 — Foundation & Core Git | `feat/gila-parity-phase-1-foundation-core-git` | `origin/main` |
| 2 — Knowledge & Content | `feat/gila-parity-phase-2-knowledge-content` | `origin/main` |
| 3 — External Services | `feat/gila-parity-phase-3-external-services` | `origin/main` |
| 4 — LLM & Agent | `feat/gila-parity-phase-4-llm-agent` | `origin/main` |

Branches are already created locally (not pushed). If the phase stack
needs rebasing after an earlier phase merges: later phases rebase onto
the updated `origin/main` (or onto the previous phase branch if its PR
is still open).

## Per-phase loop

1. `git checkout feat/gila-parity-phase-N-<name>`
2. Implement the commands in scope (see plan doc § Phase N for the
   command list and the Rust-native / vendored / shell-delegate decision
   per command).
3. Definition of done (from the PR tracker):
   - `gila <subcommand>` dispatches via gilamonster-agent
   - `--help` snapshot parity with gilabot argparse output
   - unit tests for dispatch; integration tests for Rust-native commands
   - `cargo clippy` clean, `cargo test` green
4. Commit in small increments; do NOT push without operator approval.
5. Update the phase row in `docs/plans/2026-08-12-full-gila-parity-pr.md`
   (`Status` → `☑ done`, add date, fill in PR number once opened).
6. Open the PR against `main`, link `Closes`/`Refs #68`.

## Guardrails

- **Phase 1 must ship the shell-delegate fallback first** so every
  gilabot command works from day one; later phases replace delegates
  incrementally.
- Do not commit with `--no-verify`; do not amend pushed commits.
- Pre-push email check: GitHub remote →
  `33919+hartsock@users.noreply.github.com`.
- Every LLM-authored artifact (docs, commit messages, PR text) carries
  the authorship footer per `docs/LLM_AUTHORSHIP_POLICY.md`.
- Existing dirty files on `feat/gila-compat-launcher`
  (`src/lib.rs`, `src/compat.rs`, `src/launcher.rs`) belong to a
  different workstream — leave them untouched unless the operator says
  otherwise.

## Current state at handoff

- Plan + PR docs committed: `246a947` on `feat/gila-compat-launcher`
  (docs-only commit; consider cherry-picking onto `main` or letting it
  ride with whichever phase merges first — operator decision).
- Issue #68 created and linked from both docs.
- Phase branches cut from `origin/main` @ `d9b490d`.

---

Model: nvidia/moonshotai/eccn-kimi-k3-max-preview | Harness: newt-agent v0.8.0 | Operator: Shawn Hartsock | Time: 15:21 EDT | Date: 2026-08-12
