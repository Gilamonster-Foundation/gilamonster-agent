# gilamonster-agent — task runner
#
# PIPELINE PARITY: this justfile is the local mirror of the CI pipeline at
# .github/workflows/ci.yml. The pre-push hook at .githooks/pre-push calls
# `just check` and `just cov-ci` — keep all three in lock-step.
#
# Mirrored from newt-agent's justfile (gila inherits newt's gate, same 80%
# coverage floor). gila is a single-package crate, not a workspace, so the
# recipes target the package rather than `--workspace`; the bar is identical.
#
# Quick reference:
#   just              — list available recipes (default)
#   just check        — full local gate (fmt + clippy + test)
#   just cov          — local coverage with HTML report
#   just cov-ci       — coverage with the 80% gate, lcov output (CI mode)
#   just install-hooks — wire .githooks/ as the repo's hooks path
#   just overlay-on   — point the newt-* git deps at a local newt-agent checkout
#   just overlay-off  — drop the local overlay (CI-equivalent: pinned git rev)

set windows-shell := ["powershell.exe", "-NoProfile", "-Command"]

default:
    @just --list

# --- Build ---

# Default debug build.
build:
    cargo build

# Optimized release build.
release:
    cargo build --release

# Install the `gila` release binary to DEST (default: ~/bin).
# Override: just install /usr/local/bin
[unix]
install dest=`echo $HOME/bin`:
    cargo build --release --bin gila
    mkdir -p {{dest}}
    cp target/release/gila {{dest}}/gila
    @echo "Installed: {{dest}}/gila"
    @case ":$PATH:" in *":{{dest}}:"*) ;; *) echo "Note: {{dest}} is not in PATH — add:  export PATH={{dest}}:\$PATH" ;; esac

# Remove all Cargo build artefacts.
clean:
    cargo clean

# --- Test ---

test:
    cargo test

# --- Lint & format ---

fmt:
    cargo fmt --all

lint:
    cargo clippy --all-targets -- -D warnings

# Regenerate Cargo.lock from scratch (authoritative resolution).
lock:
    cargo generate-lockfile

# fmt-check, lint, and test — the local equivalent of CI.
# PIPELINE PARITY: must match .github/workflows/ci.yml. Runs all three even if
# an earlier one fails (a fmt failure must not mask a clippy failure); exits
# non-zero if any failed.
[unix]
check:
    #!/usr/bin/env bash
    set -uo pipefail
    rc=0
    cargo fmt --all -- --check || rc=1
    cargo clippy --all-targets -- -D warnings || rc=1
    cargo test || rc=1
    exit $rc

[windows]
check:
    $rc = 0; cargo fmt --all -- --check; if ($LASTEXITCODE -ne 0) { $rc = 1 }; cargo clippy --all-targets -- -D warnings; if ($LASTEXITCODE -ne 0) { $rc = 1 }; cargo test; if ($LASTEXITCODE -ne 0) { $rc = 1 }; exit $rc

# --- Coverage ---
#
# Coverage is gated at 80% — gila inherits newt's floor. `cov` is for local
# exploration (HTML report under target/llvm-cov/html/index.html); `cov-ci` is
# what the pipeline runs.

cov:
    cargo llvm-cov --html
    @echo "HTML report at target/llvm-cov/html/index.html"

# CI-mode coverage: emit lcov + enforce the 80% floor.
# PIPELINE PARITY: must match the coverage job in .github/workflows/ci.yml.
#
# Why we don't rely on cargo-llvm-cov's --fail-under-lines: newt-agent#100
# caught it silently exit-0'ing on a sub-floor commit (cargo-llvm-cov 0.8.5
# ignores --fail-under-lines when --lcov --output-path is set). We parse the
# TOTAL line from `report --summary-only` and gate in shell — deterministic,
# version-independent, the measured percentage is always visible.
[unix]
cov-ci:
    #!/usr/bin/env bash
    set -euo pipefail
    floor=80
    cargo llvm-cov --no-report
    cargo llvm-cov report --lcov --output-path lcov.info
    summary=$(cargo llvm-cov report --summary-only)
    echo "$summary"
    # TOTAL row columns: regions missed cov% funcs missed cov% lines missed cov% ...
    # Line coverage is column 10 (3rd "Cover" column).
    line_cov=$(printf '%s\n' "$summary" | awk '$1 == "TOTAL" { gsub("%", "", $10); print $10 }')
    if [ -z "${line_cov:-}" ]; then
        echo "ERROR: could not parse line coverage from cargo-llvm-cov summary" >&2
        exit 1
    fi
    echo "measured line coverage: ${line_cov}% (floor: ${floor}%)"
    if awk -v cov="$line_cov" -v floor="$floor" 'BEGIN { exit !(cov + 0 < floor + 0) }'; then
        echo "ERROR: line coverage ${line_cov}% is below the ${floor}% floor" >&2
        exit 1
    fi
    echo "coverage gate OK: ${line_cov}% >= ${floor}%"

[windows]
cov-ci:
    $floor = 80; cargo llvm-cov --no-report; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; cargo llvm-cov report --lcov --output-path lcov.info; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $summary = cargo llvm-cov report --summary-only; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $summary; $total = $summary | Where-Object { $_ -match '^TOTAL\s+' } | Select-Object -First 1; if (-not $total) { Write-Error 'ERROR: could not parse line coverage'; exit 1 }; $cols = $total -split '\s+'; $line_cov = [double]($cols[9].TrimEnd('%')); Write-Output "measured line coverage: $line_cov% (floor: $floor%)"; if ($line_cov -lt $floor) { Write-Error "ERROR: line coverage $line_cov% is below the $floor% floor"; exit 1 }; Write-Output "coverage gate OK: $line_cov% >= $floor%"

# --- Local newt overlay (dev-only path override) ---
#
# CI builds against the pinned `rev` git dep in Cargo.toml. For local iteration
# against an in-flight newt-agent checkout, `overlay-on` drops a git-ignored
# .cargo/config.toml that path-overrides the newt-* crates onto your local tree
# (see .cargo/config.toml.template). `overlay-off` removes it.

overlay-on:
    cp .cargo/config.toml.template .cargo/config.toml
    @echo "local newt overlay ON — .cargo/config.toml path-overrides newt-* onto the local checkout."
    @echo "edit the paths in .cargo/config.toml if your newt-agent lives elsewhere, then rebuild."

overlay-off:
    rm -f .cargo/config.toml
    @echo "local newt overlay OFF — builds resolve the pinned git rev in Cargo.toml (CI-equivalent)."

# --- Hook installation ---

# Point this repo at .githooks/ for pre-push gating, and rewrite GitHub pushes
# to HTTPS (newt-agent#276: the ~minutes-long gate outlives GitHub's SSH idle
# timeout, so SSH pushes die with SIGPIPE after the gate passes; HTTPS uploads
# the pack as a fresh request). Idempotent — safe to re-run.
[unix]
install-hooks:
    #!/usr/bin/env bash
    set -euo pipefail
    git config core.hooksPath .githooks
    git config url."https://github.com/".pushInsteadOf "git@github.com:"
    echo "core.hooksPath -> .githooks (pre-push gate wired)"
    echo "pushes to git@github.com:* rewritten to https://github.com/*"
    if git config --get-urlmatch credential.helper https://github.com >/dev/null 2>&1; then
        echo "credential helper for https://github.com: OK"
    elif command -v gh >/dev/null 2>&1; then
        echo "WARNING: no git credential helper for https://github.com — run: gh auth setup-git" >&2
    else
        echo "WARNING: no git credential helper for https://github.com and 'gh' not installed." >&2
    fi

[windows]
install-hooks:
    git config core.hooksPath .githooks; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; git config url."https://github.com/".pushInsteadOf "git@github.com:"; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; Write-Output "core.hooksPath -> .githooks (pre-push gate wired)"
