# Migrating from Python gilabot to gilamonster-agent

For users switching their daily-driver `gila` from the Python gilabot CLI to
the Rust gilamonster-agent binary.

## TL;DR

Install gilamonster-agent so its `gila` binary shadows the Python one on
PATH. Everything you run today keeps working — commands not yet ported fall
back to the Python CLI automatically (with a one-line stderr notice).

## What you keep

- **Config**: same config directory (`~/.gila/…`), same files, no migration.
- **Credentials**: Confluence/JIRA/Slack tokens stay where gilabot put them.
- **Command surface**: every command runs. See `docs/COMMANDS.md` for which
  route each takes today.
- **Exit codes**: preserved (click's 2-for-usage quirk included).

## What changes

| Area | Python gilabot | gilamonster-agent |
|---|---|---|
| Startup | interpreter + plugin import on every invocation | Rust: instant; in-process Python only for the 9 bridged commands |
| `git commit`/`git tend` | subprocess git | libgit2 in-process |
| `completion` | not available | `gila completion bash|zsh` |
| Agent authority | n/a | Ambient by default; `gila --ocap` confines |
| Version identity | Python package version | SemVer + 12-character Gila commit (`-dirty` when modified) |
| Unported commands | direct | auto-delegate to Python `gila` + stderr notice |
| Install | `pip install -e` into a venv | `cargo install` / prebuilt binary; bridge needs `PYO3_PYTHON` at build time |

## Install

```bash
# Build against the venv that has the gila-plugin-* editable installs:
PYO3_PYTHON="$HOME/venv/bin/python" just install "$HOME/.local/bin"
```

Ensure the resulting `gila` appears **before** the pyenv shim `gila` on PATH
(e.g. `~/.local/bin` ahead of `~/.pyenv/shims`). Verify with:

```bash
gila version        # Rust-native; prints SemVer + exact Gila source commit
gila confluence --help   # in-process Python bridge smoke test
gila --ocap --help  # confirms the optional confinement switch is installed
```

The fallback carries an internal recursion guard across wrappers, so a pyenv
shim that resolves back to Rust Gila is skipped on the next hop while the real
Python gilabot remains available later on PATH. If an older Rust Gila also
appears on PATH, set `GILABOT_BIN` to the exact Python entry point:

```bash
export GILABOT_BIN="/path/to/python-gilabot/bin/gila"
```

## Rolling back

Reorder PATH so the pyenv shim `gila` wins, or uninstall the cargo binary.
No state is owned by gilamonster-agent, so rollback is non-destructive.

## Reporting parity gaps

If a command behaves differently between the two CLIs, run both and attach
outputs to the tracking issue (#68) or the Phase 4 PR:

```bash
GILABOT_BIN="$(pyenv which gila)" cargo test --test parity -- --nocapture
```

---

Model: nvidia/moonshotai/eccn-kimi-k3-max-preview | Harness: newt-agent v0.8.0 | Operator: Shawn Hartsock | Time: 19:58 EDT | Date: 2026-08-12

<!-- markdownlint-disable-next-line MD013 -->
Model: OpenAI GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 10:11 EDT | Date: 2026-08-14
