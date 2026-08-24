# Gila Jupyter Pixi-First Refactor Plan

## Objectives
1. Make Pixi the first-class environment manager when available
2. Fix readiness detection to reliably track launched servers
3. Improve lifecycle tracking across CLI invocations
4. Add bootstrap workflow for Pixi manifest modernization
5. Maintain security (loopback, auth, no env leakage)
6. Preserve legacy fallback for non-Pixi projects

## Implementation Strategy

### Phase 1: Create Pixi Integration Module
**File:** `src/gila_pixi.rs`
- Detect pixi.toml and Pixi availability
- Parse pixi.toml to find declared tasks (lab-local, jupyter, etc.)
- Bootstrap workflow: preview + confirm modernization to [workspace]
- Command building: properly forward environment scrubbing through Pixi

### Phase 2: Fix Readiness Detection
**Location:** Update `readiness_probe` in `gila_jupyter.rs`
- **Problem:** Port sweep can't distinguish child from unrelated local servers
- **Solution:** Parse actual endpoint from child's stdout/stderr
- **Implementation:**
  1. Capture full stdout from child process
  2. Parse "Jupyter Server X.X.X is running at: http://HOST:PORT" message
  3. Extract actual host:port and token from output
  4. Use that for readiness check (single probe, not sweep)
  5. Timeout if output parsing fails (server didn't announce properly)

### Phase 3: Lifecycle & Server Registry
**Location:** Update persistent server tracking
- **Problem:** Servers aren't accurately tracked across invocations
- **Solution:** 
  1. Include actual runtime port/URL from server announcement
  2. On list: validate servers are still running (can be drifty, OK to report stale)
  3. On stop: verify handle exists before claiming "stopped"
  4. Document limitations: no durable ownership cross-process

### Phase 4: Environment Scrubbing Through Pixi
**Location:** Update command construction
- **Problem:** Child process inherits gila's control-plane env when Pixi wraps it
- **Solution:**
  1. Build jupyter args list separately from launcher
  2. When using Pixi, use `pixi run --clean-env jupyter notebook ...` 
  3. Verify Pixi supports --clean-env or equivalent
  4. Fall back to manual env_clear if needed

### Phase 5: Testing & Validation
- Unit tests for Pixi detection and task parsing
- Integration tests for bootstrap workflow
- End-to-end test: start server with Pixi, verify endpoint, list, stop
- Test fallback paths (no Pixi, no pixi.toml)
- Test environment scrubbing (no gila env leaks)

## Key Design Decisions

### Pixi Task Selection
- If pixi.toml has declared task (lab-local, jupyter, etc.), use it
- Don't auto-discover arbitrary tasks
- Require explicit --task flag or config entry
- Fallback: use `pixi run jupyter notebook ...` if no task declared

### Bootstrap Workflow
```
$ gila jupyter bootstrap
preview: Would update pixi.toml:
  - [project] -> [workspace]
  - Add modern structure
confirm? (y/n): y
✓ Updated pixi.toml to [workspace] structure
```

### Endpoint Derivation
Parse Jupyter's startup line:
```
[I 2026-08-24 12:00:00.000 ServerApp] Jupyter Server 2.20.0 is running at:
[I 2026-08-24 12:00:00.000 ServerApp] http://127.0.0.1:8888/tree?token=abc123
```
Extract: host=127.0.0.1, port=8888, token=abc123

### Lifecycle Limits
Document in help/error messages:
- "Server list is local to this gila process (not durable across invocations)"
- "Use handle ID to manage specific server within same session"
- Persistent registry is best-effort, may show stale servers

## Files to Modify
1. `src/gila_jupyter.rs` - Main refactor, readiness, environment
2. `src/gila_pixi.rs` - NEW, Pixi integration
3. `src/main.rs` - Add bootstrap subcommand
4. `tests/jupyter_*.rs` - Add integration tests
5. docs/skills/jupyter.md - Update documentation

## Testing Checklist
- [ ] Pixi detection works (pixi.toml present/absent)
- [ ] Bootstrap workflow (preview → confirm → update)
- [ ] Server starts with Pixi environment
- [ ] Endpoint parsing from Jupyter output
- [ ] Port collision handling (still works)
- [ ] Token extraction from startup message
- [ ] Environment scrubbing (no gila env vars leak)
- [ ] Legacy fallback (no pixi.toml case)
- [ ] Server listing across invocations
- [ ] Server stop cleans up properly
- [ ] R kernel detection (if pixi has R)
