---
name: jupyter-server
description: "Start, stop, list, and monitor durable loopback Jupyter servers through opaque, instance-bound handles and authenticated HTTP lifecycle requests."
version: 1.2.0
license: Apache-2.0
when_to_use: Need a local interactive Jupyter server, a durable handle that survives the launching CLI process, or authenticated status and stop operations without trusting a bare PID or arbitrary URL.
caveats:
  exec:
    only:
      - "jupyter"
  fs_read:
    only: []
  fs_write:
    only: []
  net:
    only:
      - "127.0.0.1:*"
      - "[::1]:*"
  max_calls: { at_most: 20 }
---

# Jupyter Server Management Skill

Use `gila jupyter` to launch and manage Jupyter servers bound to a typed
loopback address. Each successful start is registered durably under an opaque
handle and a random instance identity. Later CLI processes use that durable
record, the exact instance base path, protected runtime metadata, and
authenticated HTTP endpoints; they never operate on a caller-supplied PID or
URL.

The tool is implemented in `gilamonster_agent::gila_jupyter` and is compiled
with the `jupyter` Cargo feature.

## Prerequisites

- `jupyter` on `PATH`, supplied by Notebook 7 / Jupyter Server 2 or a supported
  Notebook 6 compatibility environment
- A build of gilamonster-agent with the `jupyter` feature
- A typed loopback bind address: `127.0.0.1` or `::1` (not `localhost`)

## Commands

| Command | Behavior |
|---|---|
| `gila jupyter start` | Starts a server, verifies its private runtime announcement and authenticated readiness, then prints a human-readable handle, URL, PID, and private log path |
| `gila jupyter status <handle>` | Authenticates to the exact registered instance and reports `running`, `unreachable`, or `not found` |
| `gila jupyter list` | Lists durable registrations with bounded concurrent status probes; unreachable records are preserved |
| `gila jupyter stop <handle>` | Verifies the exact instance, sends its authenticated shutdown request, confirms listener exit, and compare-and-swap deletes only that registration |

Start output is human-oriented. It never returns or prints the authentication
token. The PID is informational only.

## Start flags

| Flag | Default | Contract |
|---|---|---|
| `--working-dir <path>` | current directory | Existing server root |
| `--port <u16>` | `8888` | Requested loopback port; duplicate registered sockets are rejected |
| `--host <ip>` | `127.0.0.1` | Must parse as a typed IPv4 or IPv6 loopback address |
| `--token <value>` | none | Explicit headless authentication token |
| `--password <value>` | none | Plaintext input hashed with Argon2 before spawn; prefer a precomputed hash where practical |
| `--password-hash <value>` | none | Existing Jupyter-compatible Argon2 password hash; takes precedence over `--password` |
| `--open-browser` | false | Allows Gila to generate private browser authentication when no explicit credential was supplied |
| `--extra <args>` | none | Exact allowlist: only canonical `--ServerApp.default_url VALUE` or `--ServerApp.default_url=VALUE`; Gila consumes it rather than forwarding it |

A headless start must supply `--token`, `--password`, or `--password-hash`.
Only `--open-browser` permits a start with no explicit credential. Positionals,
subcommands, help/config/version flags, Traitlets aliases or abbreviations, and
all other extra arguments are rejected before spawn.

## Examples

```bash
# Explicit headless authentication.
gila jupyter start --working-dir /path/to/project --port 8888 --token "$TOKEN"

# Browser flow: Gila may generate the private token, but does not print it.
gila jupyter start --working-dir /path/to/project --open-browser

gila jupyter status 3
gila jupyter list
gila jupyter stop 3
```

A second stop after successful cleanup reports `not running`. An unreachable
status is not proof that a server is dead and does not delete its registration.
Legacy records without instance identity are never probed with stored
credentials and never receive a shutdown POST; only a definite refused socket
may be cleaned with a compare-and-swap delete.

## Durable lifecycle and security model

- Every instance uses an exact `/__gila/<32-lowercase-hex>/` base path across
  modern and legacy Jupyter application classes.
- Jupyter receives its token through `JUPYTER_TOKEN_FILE`, its protected
  password/base-path configuration through Gila's final `--config`, and a
  unique private `JUPYTER_RUNTIME_DIR`. Secrets are absent from argv.
- Registration occurs only after exactly one bounded, regular, non-symlink
  runtime file matches the token, exact base path, typed loopback URL, port,
  and numeric PID and passes an authenticated readiness probe.
- The registry is durable and lock-protected. Concurrent starts allocate and
  insert atomically; stops use instance-aware compare-and-swap cleanup so a
  stale handle cannot delete a replacement instance.
- Status/list HTTP bodies, aggregate probe time, runtime-directory work, and
  diagnostic log scanning are bounded.
- Registry files, logs, ownership markers, token/config files, and runtime
  directories are private (`0700`/`0600` on Unix and current-user-only
  protected ACLs on Windows). Token/config files are removed after confirmed
  registration and on rollback. Durable logs are private and diagnostic tails
  redact raw and URL-encoded credentials.
- Cleanup validates the per-instance ownership marker and preserves unknown
  replacement directories, symlinks, and Windows reparse points.
- The child environment is cleared and rebuilt from a small platform-safe
  allowlist. Proxy, Python injection, Gila/Newt, and provider credential
  variables are not inherited.

Use an SSH tunnel when another machine must reach the server. Do not weaken the
typed-loopback bind or pass remote-access flags through `--extra`.
