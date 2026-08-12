# Gilamonster-Agent Full Gila Command Parity — Implementation Plan

**Model:** nvidia/moonshotai/eccn-kimi-k3-max-preview | **Harness:** newt-agent v0.8.0 | **Operator:** Shawn Hartsock | **Time:** 14:52 EDT | **Date:** 2026-08-12

---

## Objective

Reproduce **every** gilabot Python subcommand inside `gilamonster-agent` so the `gila` binary (Rust) exposes the complete gilabot command surface. Prefer Rust-native implementations where feasible; Python vendoring is acceptable for complex integrations.

---

## Catalog of Gilabot Subcommands

### Core Commands (gilabot/gilamonster/commands/)

| Command | Subcommands | Complexity | Implementation Strategy |
|---------|-------------|------------|------------------------|
| `gila assistant` | `chat`, `complete`, `config`, `eval`, `exec`, `install-completion`, `mcp`, `models`, `permissions`, `plugin-marketplace`, `respond`, `session`, `skill`, `skills`, `stream`, `tools` | High | Python-vendored (LLM integrations) |
| `gila board` | — | Medium | Rust-native candidate |
| `gila cache` | — | Low | Rust-native |
| `gila calendar` | `copy`, `sync` | Medium | Python-vendored (external APIs) |
| `gila checkpoint` | `create`, `delete`, `diff`, `list`, `restore` | Medium | Rust-native candidate |
| `gila commit-msg` | — | Low | Rust-native |
| `gila completion` | — | Low | Rust-native |
| `gila confluence` | `create`, `fetch`, `list`, `publish`, `pull`, `push`, `search` | High | Python-vendored (MCP integration) |
| `gila content` | — | Medium | Rust-native candidate |
| `gila daily` | — | Low | Rust-native |
| `gila dev` | — | Low | Rust-native |
| `gila doc` | `export`, `publish` | High | Python-vendored (format conversion) |
| `gila gemini` | — | Medium | Python-vendored (LLM API) |
| `gila git` | `commit`, `tend` | Medium | Rust-native (git2 crate) |
| `gila ideas` | — | Low | Rust-native |
| `gila init` | — | Low | Rust-native |
| `gila insights` | — | Medium | Rust-native candidate |
| `gila jira` | — | High | Python-vendored (MCP integration) |
| `gila log` | `activity`, `prompt` | Medium | Rust-native |
| `gila logs` | — | Low | Rust-native |
| `gila mcp` | `client`, `server` | High | Python-vendored (MCP protocol) |
| `gila meeting` | `create` | Low | Rust-native |
| `gila ollama` | — | Medium | Python-vendored (LLM API) |
| `gila pagerduty` | — | Medium | Python-vendored (MCP integration) |
| `gila plugins` | — | Medium | Rust-native (plugin discovery) |
| `gila projects` | — | Low | Rust-native |
| `gila prompt` | — | Medium | Rust-native |
| `gila review` | — | High | Python-vendored (LLM analysis) |
| `gila slack` | — | High | Python-vendored (MCP integration) |
| `gila standup` | — | Medium | Rust-native |
| `gila todos` | — | Low | Rust-native |
| `gila top5` | — | Medium | Rust-native |
| `gila update` | — | Low | Rust-native |
| `gila version` | — | Low | Rust-native |
| `gila wsl` | — | Low | Rust-native (platform-specific) |

### Plugin Commands (gila-plugin-*/)

| Plugin | Commands | Complexity | Strategy |
|--------|----------|------------|----------|
| `gila-plugin-confluence` | `confluence` | High | Python-vendored |
| `gila-plugin-git` | `git` | Medium | Rust-native |
| `gila-plugin-jira` | `jira` | High | Python-vendored |
| `gila-plugin-mcp` | `mcp` | High | Python-vendored |
| `gila-plugin-slack` | `slack` | High | Python-vendored |
| `gila-plugin-standup` | `standup` | Medium | Rust-native |
| `gila-plugin-top5` | `top5` | Medium | Rust-native |
| `gila-plugin-doc` | `doc` | High | Python-vendored |
| `gila-plugin-calendar` | `calendar` | Medium | Python-vendored |
| `gila-plugin-meeting` | `meeting` | Low | Rust-native |
| `gila-plugin-log` | `log` | Medium | Rust-native |
| `gila-plugin-worktree` | `worktree` | Medium | Rust-native |

### Already Implemented in gilamonster-agent (Rust)

| Command | Location | Notes |
|---------|----------|-------|
| `gila code` | `cli/code.rs` | Rust-native |
| `gila follow` | `cli/follow.rs` | Rust-native |
| `gila cowork` | `cli/cowork.rs` | Rust-native |
| `gila hotseat` | `cli/hotseat.rs` | Rust-native |
| `gila capabilities` | `cli/capabilities.rs` | Rust-native |
| `gila matrix` | `cli/matrix.rs` | Rust-native |
| `gila cockpit` | `cli/cockpit.rs` | Rust-native |
| `gila scrybe` | `cli/scrybe.rs` | Rust-native |

---

## Implementation Phases

### Phase 1: Foundation & Infrastructure (Week 1-2)

**Goal**: Establish command registry and Python vendoring infrastructure.

**Tasks**:
1. Create `CommandRegistry` in `gilamonster_agent/cli/registry.rs`
   - Support both Rust-native and Python-vendored commands
   - Dynamic dispatch based on command name
   - Help text aggregation from all registered commands
2. Extend `gilamonster_agent.venv` for embedded Python execution
   - Add `pyo3` dependency for Python-Rust interop
   - Create `PythonCommandRunner` that invokes Python modules
   - Handle virtualenv activation and PATH management
3. Create `pyproject.toml` for gilamonster-agent
   - Declare all gila-plugin-* dependencies
   - Support editable installs for development
4. Implement `gila plugins` command (Rust-native)
   - List available plugins
   - Show plugin status and health

**Deliverables**:
- [ ] `src/cli/registry.rs` — command registry
- [ ] `src/python_bridge.rs` — pyo3-based Python execution
- [ ] `pyproject.toml` — vendored dependencies
- [ ] `gila plugins` command working

**PR**: `phase-1-foundation`

---

### Phase 2: Python-Vendored Commands (Week 3-4)

**Goal**: Port high-complexity commands via Python vendoring.

**Commands to Port**:
1. `gila confluence` (all subcommands)
2. `gila git` (Python fallback for complex operations)
3. `gila doc` (export, publish)
4. `gila mcp` (client, server)
5. `gila assistant` (chat, complete, config, etc.)
6. `gila jira`
7. `gila slack`
8. `gila pagerduty`
9. `gila calendar`
10. `gila review`

**Implementation Pattern**:
```rust
// src/cli/confluence.rs
pub fn run_confluence(args: &[String]) -> Result<()> {
    PythonCommandRunner::new("gila_plugin_confluence.commands.confluence")
        .with_args(args)
        .run()
}
```

**Deliverables**:
- [ ] All high-complexity commands accessible via `gila <command>`
- [ ] Python vendoring working end-to-end
- [ ] Help text shows correct usage for vendored commands

**PR**: `phase-2-python-vendored`

---

### Phase 3: Rust-Native Rewrites (Week 5-6)

**Goal**: Rewrite medium/low-complexity commands in pure Rust.

**Commands to Rewrite**:
1. `gila git` — use `git2` crate
   - `commit` — conventional commit builder
   - `tend` — repository maintenance
2. `gila worktree` — use `git2` + custom worktree logic
3. `gila log` — file-based logging
   - `activity collect`
   - `prompt create`
4. `gila meeting` — markdown template generation
5. `gila top5` — interview + formatting
6. `gila standup` — standup notes generation
7. `gila board` — board file operations
8. `gila cache` — cache management
9. `gila checkpoint` — checkpoint create/list/restore
10. `gila commit-msg` — commit message validation
11. `gila completion` — shell completion generation
12. `gila daily` — daily notes
13. `gila dev` — dev environment checks
14. `gila ideas` — idea capture
15. `gila init` — project initialization
16. `gila insights` — analytics
17. `gila logs` — log viewing
18. `gila projects` — project listing
19. `gila prompt` — prompt management
20. `gila todos` — todo management
21. `gila update` — self-update
22. `gila version` — version info
23. `gila wsl` — WSL utilities

**Deliverables**:
- [ ] All listed commands implemented in pure Rust
- [ ] Feature parity with Python versions
- [ ] Unit tests for each command

**PR**: `phase-3-rust-native`

---

### Phase 4: Testing, Validation & Documentation (Week 7-8)

**Goal**: Ensure full parity and document the system.

**Tasks**:
1. Create command parity test suite
   - Run each command in both gilabot and gilamonster-agent
   - Compare outputs (stdout, exit codes, file changes)
2. Create integration tests for Python-vendored commands
3. Write user documentation
   - `docs/COMMANDS.md` — full command reference
   - `docs/ARCHITECTURE.md` — hybrid architecture explanation
   - `docs/MIGRATION.md` — guide for users switching from gilabot
4. Performance benchmarking
   - Startup time comparison
   - Command execution time
5. CI/CD updates
   - Ensure gilamonster-agent builds and tests pass in CI

**Deliverables**:
- [ ] Parity test suite passing
- [ ] Documentation complete
- [ ] Performance benchmarks documented
- [ ] CI/CD green

**PR**: `phase-4-testing-docs`

---

## Technical Architecture

### Command Dispatch Flow

```
User runs: gila confluence publish doc.md --space SPACE
                │
                ▼
        ┌───────────────┐
        │  clap parser  │  (main.rs)
        └───────┬───────┘
                │
                ▼
        ┌───────────────┐
        │ CommandRegistry│  (registry.rs)
        │  lookup("confluence")
        └───────┬───────┘
                │
        ┌───────┴───────┐
        │               │
        ▼               ▼
   ┌─────────┐    ┌─────────────┐
   │Rust-native│   │Python-vendored│
   │  (native) │   │ (pyo3 bridge) │
   └────┬────┘    └──────┬──────┘
        │                │
        ▼                ▼
   ┌─────────┐    ┌─────────────┐
   │ git2,   │    │ gilabot venv│
   │ file ops│    │ + plugins   │
   └─────────┘    └─────────────┘
```

### Python Vendoring Details

**Dependency Management**:
- gilamonster-agent will have its own `pyproject.toml`
- All gila-plugin-* packages declared as dependencies
- Installed into `~/.local/share/gilamonster-agent/venv/` (or platform equivalent)
- Reuse existing `gilamonster_agent.venv` module for venv lifecycle

**Python Execution**:
- Use `pyo3` for in-process Python execution (no subprocess overhead)
- GIL management for thread safety
- Error propagation from Python to Rust

**Configuration**:
- Reuse `~/.gila/` config directory (same as gilabot)
- No migration needed for users

---

## File Structure (New Files)

```
gilamonster-agent/
├── docs/
│   └── plans/
│       └── 2026-08-12-full-gila-parity-plan.md  (this file)
├── src/
│   ├── cli/
│   │   ├── registry.rs          # Command registry
│   │   ├── confluence.rs        # Python-vendored
│   │   ├── git.rs               # Rust-native
│   │   ├── worktree.rs          # Rust-native
│   │   ├── log.rs               # Rust-native
│   │   ├── meeting.rs           # Rust-native
│   │   ├── top5.rs              # Rust-native
│   │   ├── standup.rs           # Rust-native
│   │   ├── board.rs             # Rust-native
│   │   ├── cache.rs             # Rust-native
│   │   ├── checkpoint.rs        # Rust-native
│   │   ├── commit_msg.rs        # Rust-native
│   │   ├── completion.rs        # Rust-native
│   │   ├── daily.rs             # Rust-native
│   │   ├── dev.rs               # Rust-native
│   │   ├── doc.rs               # Python-vendored
│   │   ├── ideas.rs             # Rust-native
│   │   ├── init.rs              # Rust-native
│   │   ├── insights.rs          # Rust-native
│   │   ├── jira.rs              # Python-vendored
│   │   ├── logs.rs              # Rust-native
│   │   ├── mcp.rs               # Python-vendored
│   │   ├── projects.rs          # Rust-native
│   │   ├── prompt.rs            # Rust-native
│   │   ├── review.rs            # Python-vendored
│   │   ├── slack.rs             # Python-vendored
│   │   ├── todos.rs             # Rust-native
│   │   ├── update.rs            # Rust-native
│   │   ├── version.rs           # Rust-native
│   │   └── wsl.rs               # Rust-native
│   ├── python_bridge.rs         # pyo3 Python execution
│   └── lib.rs                   # Re-export command modules
├── pyproject.toml               # Python dependencies
├── tests/
│   ├── parity_tests.rs          # Command parity tests
│   └── python_vendored_tests.rs # Python command tests
└── Cargo.toml                   # Add pyo3, git2, etc.
```

---

## Dependencies to Add

### Cargo.toml
```toml
[dependencies]
# Existing
clap = { version = "4", features = ["derive"] }
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
tracing = "0.1"
tracing-subscriber = "0.3"

# New for Python vendoring
pyo3 = { version = "0.22", features = ["auto-initialize"] }
pyo3-asyncio = { version = "0.22", features = ["tokio-runtime"] }

# New for Rust-native commands
git2 = "0.19"
walkdir = "2"
chrono = "0.4"
comfy-table = "7"
dialoguer = "0.11"
indicatif = "0.17"
shell-words = "1"
```

### pyproject.toml (gilamonster-agent)
```toml
[project]
name = "gilamonster-agent"
version = "0.1.0"
requires-python = ">=3.11,<4"
dependencies = [
    "gilamonster-core",
    "gila-plugin-core",
    "gila-plugin-confluence",
    "gila-plugin-git",
    "gila-plugin-jira",
    "gila-plugin-mcp",
    "gila-plugin-slack",
    "gila-plugin-standup",
    "gila-plugin-top5",
    "gila-plugin-doc",
    "gila-plugin-calendar",
    "gila-plugin-meeting",
    "gila-plugin-log",
    "gila-plugin-worktree",
]

[project.optional-dependencies]
dev = [
    "pytest",
    "pytest-cov",
    "black==26.1.0",
    "ruff==0.15.2",
]
```

---

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| Python vendoring performance | Use pyo3 in-process execution; benchmark against subprocess |
| GIL contention | Single-threaded command execution; document limitation |
| Plugin version drift | Pin exact versions in pyproject.toml; CI checks |
| Config conflicts | Reuse ~/.gila/; no changes to config format |
| Platform-specific issues | Test on macOS, Linux, Windows (WSL) |

---

## Success Criteria

1. `gila --help` shows all commands (Rust + Python)
2. Every gilabot command works identically in gilamonster-agent
3. No regression in existing gilamonster-agent commands
4. Startup time < 500ms for Rust-native commands
5. Python-vendored commands execute within 2s overhead

---

## Timeline

| Phase | Duration | PR |
|-------|----------|-----|
| Phase 1: Foundation | Week 1-2 | `phase-1-foundation` |
| Phase 2: Python-vendored | Week 3-4 | `phase-2-python-vendored` |
| Phase 3: Rust-native | Week 5-6 | `phase-3-rust-native` |
| Phase 4: Testing & Docs | Week 7-8 | `phase-4-testing-docs` |

**Total**: 8 weeks

---

## Open Questions

1. Should `gila assistant` be fully vendored or partially rewritten in Rust (using async-openai)?
2. Do we need to support `gila mcp server` (MCP server hosting) in gilamonster-agent, or is that out of scope?
3. Should we maintain a compatibility shim so `gila` can fall back to `gilabot` for unimplemented commands during transition?

---

**Status**: Ready for implementation. Awaiting operator approval to begin Phase 1.
