<p align="center">
  <img src="docs/logos/gilly-256.png" alt="Gilly, the Gilamonster mascot" width="256" height="256">
</p>

# gilamonster-agent

> Experimental Rust agent cockpit built on [newt-agent](https://github.com/Gilamonster-Foundation/newt-agent).

`gila` embeds pinned Newt crates; it is not a Newt plugin. It adds a
human-owned PTY, read-only observation, capability packages, and
`cockpit`/FleetView previews. `gila` starts its coder with host filesystem,
network, and command authority. `--ocap` uses Newt's configured OCAP posture.
`cowork`, `follow`, `hotseat`, and companion panes do not inherit the ambient
default. See the
[authority policy](docs/decisions/ambient_native_shell_default.md).

## Terminal-Bench

No `gila` result is published in
[`gilamonster-bench@44f49eb`](https://github.com/Gilamonster-Foundation/gilamonster-bench/tree/44f49eb504db05ca234b91e1595f175b528a0686).
The table reproduces the
[`newt-agent@5ddc969`](https://github.com/Gilamonster-Foundation/newt-agent/blob/5ddc9693aff8d0230d70bfbebdacbedf772eb423/README.md#terminal-bench)
scoreboard; these are Newt results.

_Best recorded Newt run per model and lane. Measured rows use
[Terminal-Bench](https://github.com/harbor-framework/terminal-bench) `tb-30` at
context 65,536. OCAP off/on means unconfined/confined. Metadata is shown per
lane because the two maxima may come from different releases._

| Model | OCAP off | OCAP on |
|-------|----------|---------|
| `deepseek-v4-pro`<br><sub>deepseek</sub> | 56.7% (17/30)<br><sub>v0.8.0 · 2026-08-06</sub> | 50.0% (15/30)<br><sub>v0.8.0 · 2026-08-06</sub> |
| `nemotron-3-super`<br><sub>nemotron</sub> | 36.7% (11/30)<br><sub>v0.8.0 · 2026-08-05</sub> | 26.7% (8/30)<br><sub>v0.8.0 · 2026-08-05</sub> |
| `ornith-1.0-35b-q8`<br><sub>ornith</sub> | _pending_ | 36.7% (11/30)<br><sub>v0.7.6 · 2026-07-29</sub> |
| `qwen3.6_35b`<br><sub>qwen</sub> | 20.0% (6/30)<br><sub>v0.7.5 · 2026-07-28</sub> | 26.7% (8/30)<br><sub>v0.7.6 · 2026-07-29</sub> |
| `o4-mini`<br><sub>openai</sub> | 13.3% (4/30)<br><sub>v0.8.0 · 2026-08-05</sub> | 16.7% (5/30)<br><sub>v0.8.0 · 2026-08-05</sub> |
| `qwen3-coder_30b`<br><sub>qwen</sub> | 10.0% (3/30)<br><sub>v0.7.5 · 2026-07-28</sub> | 13.3% (4/30)<br><sub>v0.7.6 · 2026-07-29</sub> |
| `gpt-oss_120b`<br><sub>openai</sub> | 10.0% (3/30)<br><sub>v0.8.0 · 2026-08-05</sub> | 10.0% (3/30)<br><sub>v0.8.0 · 2026-08-05</sub> |
| `kimi-linear_48b`<br><sub>kimi</sub> | _pending_ | 10.0% (3/30)<br><sub>v0.7.6 · 2026-07-31</sub> |
| `nemotron-3-nano_30b`<br><sub>nemotron</sub> | 6.7% (2/30)<br><sub>v0.7.5 · 2026-07-29</sub> | _pending_ |
| `glm-4.7-flash`<br><sub>glm</sub> | _pending_ | 3.3% (1/30)<br><sub>v0.7.6 · 2026-07-31</sub> |
| `gpt-4.1-mini`<br><sub>openai</sub> | 0.0% (0/30)<br><sub>v0.8.0 · 2026-08-05</sub> | 3.3% (1/30)<br><sub>v0.8.0 · 2026-08-05</sub> |
| `kimi-k2.7-code`<br><sub>kimi</sub> | _queued_ | _queued_ |
| `nemotron-3-ultra`<br><sub>nemotron</sub> | _queued_ | _queued_ |
| `ornith-1.0-397b-iq1_m`<br><sub>ornith</sub> | _queued_ | _queued_ |

[Score records](https://github.com/Gilamonster-Foundation/newt-agent/blob/5ddc9693aff8d0230d70bfbebdacbedf772eb423/scripts/eval/bench-results.jsonl)
and [July survey notes](https://github.com/Gilamonster-Foundation/newt-agent/blob/5ddc9693aff8d0230d70bfbebdacbedf772eb423/docs/findings/2026-07-29-dgx-spark-terminal-bench-survey.md)
are pinned to the same Newt revision.

## Install

Builds require Rust 1.88+ and Python 3. The Unix recipe also requires
[`just`](https://github.com/casey/just).

```bash
git clone https://github.com/Gilamonster-Foundation/gilamonster-agent
cd gilamonster-agent
PYO3_PYTHON="$(command -v python3)" just install "$HOME/.local/bin"
export PATH="$HOME/.local/bin:$PATH"
gila --help
```

On Windows, set `PYO3_PYTHON` to `python.exe` and run `cargo install --path .`.

## Use

```bash
gila                       # ambient coder in the current directory
gila code ./project        # ambient coder in a project
gila --ocap code ./project # Newt's configured OCAP posture
gila cowork ./project      # agent chat above a PTY shell
gila follow session.log    # read-only shell observer
gila cap --help            # optional capability packages
gila cockpit               # multiplexer preview
gila matrix --mock         # FleetView preview
```

See `gila --help` for all commands.

## Develop

```bash
cargo build
just check                 # format, clippy, tests
just cov-ci                # coverage gate
just install-hooks
```

For local Newt changes, run `just overlay-on`, edit the generated
`.cargo/config.toml`, then run `just overlay-off` to restore the pinned build.

## License

[Apache-2.0](LICENSE).
