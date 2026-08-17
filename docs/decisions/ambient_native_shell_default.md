# ADR: Make Gilamonster ambient-first and keep OCAP opt-in

Author: Shawn Hartsock
Proposed: 2026-08-13
Accepted: TBD
Status: Editing
Audience: Gilamonster Foundation maintainers and operators

## Problem / Context

Newt and Gilamonster serve different trust postures. Newt is the confined
airframe: object-capability policy, a kernel-backed shell boundary where the
host supports it, and interactive permission escalation. Gilamonster is the
operator's monster agent: it optimizes for function in an already trusted
local environment.

Today `gila code` embeds newt's TUI but inherits newt's confined default. That
makes the products operationally indistinguishable at the point where their
intended tradeoff matters. It also tempts Gilamonster to grow a second shell
implementation even though newt already exposes the required launch seams.

The decision is whether Gilamonster should keep newt's default, introduce an
unrelated executor, or invert the launch policy while retaining newt's OCAP
path as an explicit option.

## Proposal

Gilamonster will invert the inherited coder's launch default. Newt remains
confined by default; Gilamonster's inherited coder surface is ambient by
default.

- `newt`: existing confined default, unchanged.
- `gila` and `gila code [PATH]`: full ambient authority with native host
  command execution.
- `gila --ocap` and `gila code --ocap [PATH]`: Newt's configured OCAP posture.
- `gila cowork [PATH]`: configured Newt confinement with a separate human PTY.
- `gila follow`, `gila hotseat`, and `gila cockpit`: always confined.
- Utility and delegated commands: no ambient agent authority.

The ambient baseline is a composition of existing newt switches, applied
before any async runtime or inherited newt component starts:

- `NEWT_DISABLE_OCAP=1` selects unconfined host-shell dispatch.
- `NEWT_FULL_ACCESS=1` lifts filesystem, network, and execution policy.
- `NEWT_NO_ROUTE=1` prevents common reads from being rewritten to built-ins.
- `NEWT_SHELL_ENGINE` selects Newt's platform-aware full-access engine:
  `host` on Unix and `brush` on Windows.

Gilamonster then resolves and freezes `LaunchAuthority` once. The explicit
`--ocap` path removes inherited widening switches before freezing, so a parent
environment cannot silently defeat the operator's confinement request.

This is a launch baseline, not an irrevocable grant. A plan phase, persona,
operating mode, or specialized surface may attenuate it. Agent-facing MCP
capabilities also remain opt-in through the Gila capability manifest; full
ambient shell access does not auto-mount them.

### What “native shell” means

Agent commands are one-shot host-shell executions, not an interactive login
shell. On Unix, newt dispatches through `bash -c` when available and otherwise
`sh -c`. The human's persistent `$SHELL` in the cowork PTY is a different
surface and remains structurally unwritable by the agent.

The spawned command receives the operator's ordinary environment. Newt strips
its own authority switches and harness-owned secrets before launching the
child, but arbitrary credentials already present in the operator environment
may still be reachable. The cowork human PTY mirrors that scrub so a nested
agent cannot inherit its parent's authority choice. Ambient mode is therefore
appropriate only on a host the operator intentionally trusts with that
authority.

### Observability

Ambient sessions arm newt's append-only shadow-OCAP flight recorder unless the
operator explicitly disables it with `NEWT_FLIGHT_RECORDER=off` or `0`. This
does not confine the session; it records the authority a leash would have
needed, providing evidence for later OCAP profiles.

## Implementation and rollout plan

1. **Launch inversion and upstream adoption — this PR.** Re-pin all inherited
   newt crates to one current `main` revision, introduce the typed launch
   posture, freeze it before Tokio starts, add global `--ocap`, retain the
   observer/triage clamps, and adapt MCP admission to newt's current API.
2. **Local dual-install acceptance — this PR.** Install current `newt` and
   `gila` as separate executables, verify exact versions and code signatures,
   and prove PATH resolves each intended binary.
3. **Live authority BAT.** With a deterministic tool-calling backend, verify an
   ambient Gila turn returns a real `run_command` result for native shell work
   outside the workspace. Repeat under `--ocap` and verify the same action is
   confined or denied. Model prose is not evidence.
4. **Cross-platform OCAP parity — this PR.** Package newt's
   `newt-net-guard` beside `gila` so Linux deny-all subprocesses can resolve the
   egress guard. Keep macOS Seatbelt and Windows behavior in their platform
   CI/UAT lanes.
5. **Cockpit authority expansion.** Keep companion, follow, hotseat, and
   observe-only panes confined. Extend ambient authority only to typed
   workbench panes, after their grant/ceiling UI makes the distinction visible.
6. **Release ratchet.** Gate the release on the repository's full check and
   coverage contracts, an overlay-disabled pinned-revision build, installed
   smoke tests, and a rollback test using `gila --ocap`.

Rollback is per invocation: `gila --ocap`. A release-wide rollback changes the
typed default to OCAP without changing newt, configuration files, capability
manifests, or user data.

## Test Strategy

- Unit-test the exact launch environment plan. Widening values must be exactly
  `1`; the OCAP plan must remove every inherited widening switch.
- Unit-test the ambient allowlist: inherited coder only.
- Prove cowork defaults to read-only, workspace-fenced caveats and its human
  PTY strips Newt authority controls and harness secrets.
- Exercise `--ocap` both before and after the `code` subcommand through clap.
- Prove Gila-created MCP entries pass newt's enabled/trusted admission gate.
- Run `just check` and `just cov-ci` with the local newt overlay, then repeat a
  release build with the overlay removed so a bad revision cannot be masked.
- Smoke-test the installed `newt`, `newt-mcp-server`, and `gila` artifacts by
  absolute path and through PATH. Verify macOS signatures.
- Treat the live shell BAT as required evidence before release, but not as a
  deterministic per-PR unit test.

## Scenarios / Use Cases / Customer Stories

- As an operator on a trusted workstation, I can run `gila` and let the agent
  use the same native tools and filesystem I can, without approving every
  routine action.
- As an operator handling untrusted code, I can run `gila --ocap` and get the
  inherited newt confinement posture for that invocation.
- As an on-call operator, I can use follow or hotseat without the ambient coder
  default invalidating their read-only promises.
- As a policy author, I can inspect the ambient flight record and turn observed
  behavior into a deliberate optional OCAP profile.

## Alternatives considered

1. **Keep newt's confined default.** Rejected because it erases the intended
   product distinction and does not satisfy the monster-agent use case.
2. **Fork or rewrite the shell executor.** Rejected because newt already owns
   command output, TUI integration, environment scrubbing, and both host and
   confined dispatch. A second executor would drift.
3. **Persist `full_access` in Gila configuration.** Rejected for the first
   release. Authority is a startup act and the explicit inverse flag is easier
   to audit than another persistent configuration source.
4. **Make every Gila subcommand ambient.** Rejected because it would falsify the
   documented guarantees of follow, hotseat, companion, and cockpit surfaces.

## Resources

- [`src/launch.rs`](../../src/launch.rs) — typed launch policy and freeze point
- [`src/authority.rs`](../../src/authority.rs) — cockpit pane clamps
- [`docs/decisions/cockpit_tmux_multiplexer.md`](cockpit_tmux_multiplexer.md)
- [newt launch authority](https://github.com/Gilamonster-Foundation/newt-agent/blob/8cde29f0b16cab9206fb76d94925b1ea49ee68bc/newt-core/src/launch_authority.rs)
- [newt CLI authority switches](https://github.com/Gilamonster-Foundation/newt-agent/blob/8cde29f0b16cab9206fb76d94925b1ea49ee68bc/newt-cli/src/lib.rs)

---

<!-- markdownlint-disable-next-line MD013 -->
Model: OpenAI GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 19:34 EDT | Date: 2026-08-13
