# `gila` Command Reference — gilamonster-agent

How each `gila` subcommand is executed in the hybrid gilamonster-agent CLI:
**Rust-native** (pure Rust, no Python), **in-process Python** (pyo3 bridge,
no subprocess), or **shell-delegate** (execs the Python `gila` on PATH).

The routing decision is data, not code: Rust-native commands are the clap
subcommands in `src/lib.rs`; the in-process set is
`src/python_bridge.rs::PYO3_ROUTED_COMMANDS`; everything else falls through
to `src/delegate.rs`.

## Routing summary

| Command | Route | Notes |
| --- | --- | --- |
| `gila` / `code` | Rust-native | Ambient; `--ocap` confines |
| `cowork` | Rust-native | Confined agent; human-owned PTY |
| `follow` / `hotseat` / `cockpit` | Rust-native | Always confined |
| `capabilities` | Rust-native | MCP is opt-in and admission-gated |
| `git commit` | Rust-native | libgit2, incl. `--bulk` |
| `git tend` | Rust-native | git2-based profiles |
| `version` | Rust-native | |
| `daily` | Rust-native | |
| `ideas` | Rust-native | |
| `todos` | Rust-native | |
| `projects` | Rust-native | |
| `board` | Rust-native | |
| `cache` | Rust-native | |
| `logs` | Rust-native | |
| `prompt` | Rust-native | |
| `commit-msg` | Rust-native | |
| `completion` | Rust-native | **Rust-only** — Python gilabot has no `completion` command |
| `init` | Rust-native | |
| `update` | Rust-native | |
| `meeting` | Rust-native | |
| `top5` | Rust-native | |
| `standup` | Rust-native | |
| `checkpoint` | Rust-native | |
| `insights` | Rust-native | |
| `dev` | Rust-native | |
| `wsl` | Rust-native | |
| `log activity` | Rust-native | |
| `log prompt` | Rust-native | |
| `worktree` | Rust-native | |
| `confluence` | In-process Python | pyo3 bridge into `gilabot.main()` |
| `jira` | In-process Python | |
| `slack` | In-process Python | |
| `mcp` | In-process Python | |
| `assistant` | In-process Python | |
| `doc` | In-process Python | |
| `calendar` | In-process Python | |
| `review` | In-process Python | |
| `pagerduty` | In-process Python | |
| *(anything else)* | Shell-delegate | execs Python `gila` from PATH with a deprecation warning on stderr |

## Discovering the route at runtime

Run any command and watch stderr: shell-delegated commands print

```text
gila: `<cmd>` is delegated to Python gilabot (<path>) — not yet Rust-native.
```

Rust-native and in-process-Python commands print nothing extra.

## Exit codes

- Rust-native commands follow standard CLI conventions (0 success, 1 error,
  2 usage).
- In-process Python commands return the exit code requested by
  `gilabot.main()` via `SystemExit` (click: 0 success, 2 usage error).
  Note: some gilabot group `--help` invocations exit 2 (a click quirk carried
  over from Python gilabot), which the parity/bridge test suites accept.
- Shell-delegated commands propagate the delegated process's exit status
  verbatim; 1 if the delegate was killed by a signal.

---

Model: nvidia/moonshotai/eccn-kimi-k3-max-preview | Harness: newt-agent v0.8.0 | Operator: Shawn Hartsock | Time: 19:58 EDT | Date: 2026-08-12

<!-- markdownlint-disable-next-line MD013 -->
Model: OpenAI GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 19:34 EDT | Date: 2026-08-13
