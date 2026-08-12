#!/usr/bin/env bash
# Performance benchmark: Python gilabot vs Rust gilamonster-agent.
#
# Measures startup (--help) and a representative Rust-native command
# (version) for both CLIs, plus the in-process bridged `confluence --help`
# for the Rust side vs the Python subprocess equivalent.
#
# Usage:  scripts/benchmark_parity.sh [ITERATIONS]
# Output: Markdown table to stdout (suitable for docs/BENCHMARKS.md).
set -u

ITER="${1:-20}"
RUST_BIN="$(cargo metadata --format-version=1 --no-deps 2>/dev/null | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/debug/gila"
[ -x "$RUST_BIN" ] || RUST_BIN="./target/debug/gila"
PY_BIN="${GILABOT_BIN:-$(command -v gila-py || command -v gilabot || true)}"

if [ -z "$PY_BIN" ]; then
  echo "No Python gilabot found (set GILABOT_BIN); skipping benchmark." >&2
  exit 0
fi

# median wall-clock ms over ITER runs of "$@"
bench() {
  local label="$1"; shift
  local times=()
  local i start end
  for ((i = 0; i < ITER; i++)); do
    start=$(python3 -c 'import time; print(int(time.perf_counter()*1000))')
    "$@" >/dev/null 2>&1
    end=$(python3 -c 'import time; print(int(time.perf_counter()*1000))')
    times+=($((end - start)))
  done
  printf '%s\n' "${times[@]}" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'
}

printf '| Benchmark (median of %s runs) | Python gilabot | Rust gila | Speedup |\n' "$ITER"
printf '|---|---|---|---|\n'

for spec in "startup: --help" "version: version" "bridged-help: confluence --help"; do
  name="${spec%%:*}"; args="${spec#*: }"
  py_ms=$(bench "$name py" "$PY_BIN" $args)
  rs_ms=$(bench "$name rs" "$RUST_BIN" $args)
  speedup=$(awk -v p="$py_ms" -v r="$rs_ms" 'BEGIN{ if (r>0) printf "%.1fx", p/r; else print "n/a" }')
  printf '| %s (`%s`) | %s ms | %s ms | %s |\n' "$name" "$args" "$py_ms" "$rs_ms" "$speedup"
done
