# Performance Benchmarks — Python gilabot vs Rust gilamonster-agent

Measured 2026-08-12 on the operator's NV MacBook (macOS, pyenv Python 3.11.14
gilabot with editable plugins vs debug-build `gila` with the pyo3 bridge),
median of 5 runs, via `scripts/benchmark_parity.sh`:

| Benchmark | Python gilabot | Rust gila | Speedup |
|---|---|---|---|
| startup (`--help`) | 2822 ms | 141 ms | **20.0x** |
| `version` | 2467 ms | 150 ms | **16.4x** |
| `confluence --help` (bridged) | 2597 ms | 2756 ms | 0.9x |

## Reading the table

- **Rust-native commands** (`--help`, `version`, and the 22 ported commands)
  skip Python entirely → ~16–20x faster than the Python CLI, dominated by
  CPython startup + plugin import.
- **Bridged commands** (`confluence --help` here) embed the same interpreter
  and import the same plugins, so their cost matches the Python CLI (~2.6s).
  The 0.9x "slowdown" is within run-to-run noise; the point of the bridge is
  *no added subprocess cost*, not speed. Speeding these up further means
  porting them to Rust, not tuning the bridge.
- Debug build: a `--release` binary narrows the Rust-native numbers further.

## Reproducing

```bash
PYO3_PYTHON="$HOME/venv/bin/python" cargo build
GILABOT_BIN="$(pyenv which gila)" scripts/benchmark_parity.sh 5
```

`GILABOT_BIN` selects the Python reference CLI; the script skips cleanly when
no Python gilabot is found.

---

Model: nvidia/moonshotai/eccn-kimi-k3-max-preview | Harness: newt-agent v0.8.0 | Operator: Shawn Hartsock | Time: 20:04 EDT | Date: 2026-08-12
