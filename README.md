<p align="center">
  <img src="docs/logos/gilly-256.png" alt="Gilly, the Gilamonster mascot" width="256" height="256">
</p>

# gilamonster-agent

> Experimental Rust agent cockpit built on [newt-agent](https://github.com/Gilamonster-Foundation/newt-agent).

`gila` embeds pinned Newt crates; it is not a Newt plugin. It adds a
human-owned PTY, read-only observation, capability packages, and
`cockpit`/FleetView previews. Every `gila` surface runs with the operator's
own host filesystem, network, and command authority on the native shell.
There is no OCAP mode: Gila trades confinement for pragmatism; Newt remains
the confined airframe. See the
[authority policy](docs/decisions/ambient_native_shell_default.md).

## Install

Builds require Rust 1.91+ and Python 3. The Unix recipe also requires
[`just`](https://github.com/casey/just).

```bash
git clone https://github.com/Gilamonster-Foundation/gilamonster-agent
cd gilamonster-agent
PYO3_PYTHON="$(command -v python3)" just install "$HOME/.local/bin"
export PATH="$HOME/.local/bin:$PATH"
gila --help
```

On Windows, set `PYO3_PYTHON` to `python.exe` and run `cargo install --path .`.

### Prebuilt binaries

Each release on the [Releases page](https://github.com/Gilamonster-Foundation/gilamonster-agent/releases)
carries `gila` and `gila-headless` for Linux x86_64 (tar.gz, .deb, .rpm),
macOS arm64 (tar.gz, Homebrew formula `gila.rb`) and Windows x86_64 (zip,
Chocolatey `.nupkg`). These are **bridge-less** builds: the in-process Python
bridge cannot be prebuilt portably, so the Python-backed command groups
delegate to a `gila` from the Python gilabot package if one is on PATH. Build
from source with `just install` for the in-process bridge.

## Use

```bash
gila                       # ambient coder in the current directory
gila code ./project        # ambient coder in a project
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

## Documents

| Document | Covers |
|---|---|
| [ROADMAP.md](ROADMAP.md) | Release line, milestones, links to the cockpit/authority design docs |
| [CHANGELOG.md](CHANGELOG.md) | What shipped in each version |
| [docs/decisions/ambient_native_shell_default.md](docs/decisions/ambient_native_shell_default.md) | The ambient-authority policy summarized above |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | How the hybrid Rust/Python command dispatch works |
| [docs/COMMANDS.md](docs/COMMANDS.md) | Per-command routing: Rust-native, in-process Python, or shell-delegate |
| [docs/MIGRATION.md](docs/MIGRATION.md) | Switching a Python gilabot daily driver to `gila` |
| [docs/BENCHMARKS.md](docs/BENCHMARKS.md) | Python gilabot vs Rust `gila` startup speed |
| [docs/terminal-bench.md](docs/terminal-bench.md) | Newt's Terminal-Bench scoreboard (gila has no published result yet) |

## License

[Apache-2.0](LICENSE).
