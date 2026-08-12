# gilamonster-agent Hybrid Architecture

How the Rust `gila` binary runs gilabot's full command surface while the
Rust-native port proceeds command-by-command.

## Dispatch flow

```text
gila <cmd> [args…]
      │
      ▼
┌──────────────┐
│ clap parser  │  src/lib.rs — Cli + Command enum
└──────┬───────┘
       │
       ▼
┌─────────────────────────────┐
│ main.rs dispatch            │
│ match on Command variants   │
└──┬───────────┬──────────┬───┘
   │           │          │
   ▼           ▼          ▼
┌───────┐ ┌─────────┐ ┌──────────────┐
│Rust-  │ │In-process│ │Shell-delegate│
│native │ │Python    │ │(delegate.rs) │
│(clap  │ │(pyo3     │ │              │
│arms)  │ │bridge.rs)│ │              │
└───┬───┘ └────┬────┘ └──────┬───────┘
    │          │             │
    ▼          ▼             ▼
 git2,    gilabot.main()  exec `gila` from
 file ops in-process      PATH (Python)
```

Three tiers, checked in order:

1. **Rust-native** — a clap `Command` variant exists; runs entirely in Rust
   (git2, file I/O, `~/.gila` config shared with gilabot).
2. **In-process Python** — command name is in
   `python_bridge::PYO3_ROUTED_COMMANDS`; runs `gilabot.main()` inside the
   embedded CPython interpreter (no subprocess startup cost).
3. **Shell-delegate** — anything else; `delegate.rs` finds the Python `gila`
   on PATH (excluding this binary), warns on stderr, and `exec`s it with the
   original argv.

## The pyo3 bridge (`src/python_bridge.rs`)

- **Build time**: `PYO3_PYTHON=/Users/shartsock/venv/bin/python` selects the
  venv interpreter carrying the editable `gila-plugin-*` installs (the pyenv
  shim `python3` does not have them).
- **Run time**: `ensure_sys_path` prepends the venv's `site-packages` (via
  `site.addsitedir`, so editable `.pth` finders activate) and the gilabot
  source root to `sys.path`.
- **The call**: set `sys.argv = [cmd, args…]`, `import gilabot`, call
  `main()`, map `SystemExit.code` to the process exit code. Non-`SystemExit`
  exceptions print a traceback and exit 1.
- **Constraint**: the embedded interpreter is process-global and click
  dispatch mutates global state, so exactly one stateful dispatch per process
  is supported. `--help` short-circuits safely; anything else must not be
  called twice in one process. This is exercised and documented in
  `tests/python_bridge.rs`.

## Shell delegation (`src/delegate.rs`)

- Resolves `gila`/`gilabot`/`gila-py` on PATH via `std::env::current_exe()`
  exclusion (never recurses into this binary).
- Prints the deprecation warning to stderr and `exec`s, preserving exit
  status. Missing delegate → actionable error naming the command and the
  install hint.

## What the phases delivered

| Phase | Content | PR |
|---|---|---|
| 1 | Command registry, dispatch skeleton, shell-delegate fallback, Rust-native `git commit`/`git tend` | #70 |
| 2 | pyo3 in-process bridge for the 9 high-complexity commands | #71 |
| 3 | 22 low/medium-complexity commands ported to Rust (4 batches) | #72 |
| 4 | Parity suite, bridge integration tests, docs, benchmarks, CI | #73 (this) |

---

Model: nvidia/moonshotai/eccn-kimi-k3-max-preview | Harness: newt-agent v0.8.0 | Operator: Shawn Hartsock | Time: 19:58 EDT | Date: 2026-08-12
