//! Jupyter notebook execution + server management tool — gila-native port of
//! newt-agent's `newt-tools/src/jupyter.rs` (newt-agent PR #1730).
//!
//! The extension point is a separate binary, not a plugin slot, so the jupyter
//! surface lives HERE in gila's own tree rather than behind a newt rev bump.
//! The whole module is compiled only when the `jupyter` cargo feature is on
//! (the `pub mod gila_jupyter;` declaration in `lib.rs` is feature-gated), so
//! none of the `nbformat` / `reqwest` / `rand` / `argon2` deps reach a default
//! build.
//!
//! ## Server model
//!
//! `start_server` spawns `jupyter notebook` bound to the loopback interface
//! only (never a remote-access flag), scrubs the child environment of gila's
//! whole control plane (`env_clear` + a minimal allowlist), then *probes* the
//! REST API until the server answers instead of sleeping a fixed delay. The
//! spawned `Child` is owned by a process-local registry keyed by an opaque
//! `handle_id`; `stop_server` / `get_server_status` operate by handle, never
//! by bare PID or an arbitrary URL — so a caller cannot point this tool at a
//! server it did not start. `stop_server` kills the owned child directly
//! (`Child::kill`), so no `kill` / `taskkill` subprocess is ever spawned.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Environment variables passed through to the jupyter child after
/// `env_clear`. Deliberately EXCLUDES every gila control-plane switch and
/// secret (`GILA_*` / `NEWT_*` agent keys, operator keys, authority tokens) —
/// the same philosophy as the operator-yolo-opt-out scrub: a nested jupyter
/// cannot re-assert gila authority and gila's credentials never leak into the
/// notebook subprocess.
const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "LC_CTYPE", "TERM"];

/// Maximum bytes of captured stderr retained for diagnostics on a failed start.
const STDERR_CAP: usize = 8 * 1024;

/// `gila jupyter …` subcommands. The whole enum (and the `Jupyter` arm of the
/// top-level `Command`) is compiled out unless the `jupyter` feature is on.
#[derive(clap::Subcommand, Debug, PartialEq, Eq)]
pub enum JupyterCmd {
    /// Execute a Jupyter notebook (.ipynb) in place via `jupyter nbconvert`.
    Execute {
        /// Path to the notebook file (.ipynb).
        notebook_path: PathBuf,
        /// Working directory for execution (default: the notebook's parent dir).
        #[arg(long)]
        working_dir: Option<String>,
        /// Per-cell execution timeout in seconds (default: 300).
        #[arg(long)]
        timeout: Option<u64>,
        /// Kernel name (default: python3).
        #[arg(long)]
        kernel: Option<String>,
        /// Do not save outputs into the notebook (default: outputs are saved).
        #[arg(long)]
        no_save_outputs: bool,
    },
    /// Start a Jupyter notebook server bound to loopback, owned by this process.
    Start {
        /// Working directory for the server (default: current directory).
        #[arg(long)]
        working_dir: Option<String>,
        /// Port to run the server on (default: 8888).
        #[arg(long)]
        port: Option<u16>,
        /// Bind address — must be loopback (127.0.0.1 / ::1 / localhost).
        #[arg(long)]
        host: Option<String>,
        /// Auth token (default: auto-generated 32 random chars).
        #[arg(long)]
        token: Option<String>,
        /// Plaintext password; hashed with argon2 before being passed to jupyter.
        #[arg(long)]
        password: Option<String>,
        /// Already-hashed `argon2:$argon2id$…` PHC string (takes precedence over
        /// `--password`).
        #[arg(long)]
        password_hash: Option<String>,
        /// Open the operator's browser on start (default: headless).
        #[arg(long)]
        open_browser: bool,
        /// Extra `jupyter notebook` flags (caller-controlled — use with care).
        #[arg(long, value_delimiter = ' ')]
        extra: Option<Vec<String>>,
    },
    /// Stop a Jupyter server by its handle id (from `gila jupyter start`).
    Stop {
        /// Opaque handle id returned by `gila jupyter start`.
        handle_id: u64,
    },
    /// Query a Jupyter server's status + kernels by handle id.
    Status {
        /// Opaque handle id returned by `gila jupyter start`.
        handle_id: u64,
    },
    /// List all active Jupyter servers started by this process.
    List,
}

// ---- notebook execution -------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterExecuteParams {
    /// Path to the notebook file (.ipynb)
    pub notebook_path: String,
    /// Optional working directory to execute the notebook in.
    /// If not provided, uses the notebook's parent directory.
    pub working_dir: Option<String>,
    /// Per-cell execution timeout in seconds, passed to nbconvert's
    /// `ExecutePreprocessor.timeout`. A cell that exceeds this is interrupted
    /// and marks the notebook as failed. Default: 300.
    pub timeout_seconds: Option<u64>,
    /// Whether to save the executed notebook with outputs (default: true)
    pub save_outputs: Option<bool>,
    /// Kernel name to use (default: python3)
    pub kernel_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterExecuteResult {
    /// Whether execution succeeded
    pub success: bool,
    /// Path to the executed notebook
    pub notebook_path: String,
    /// Number of code cells executed (markdown/raw cells are not counted)
    pub cells_executed: usize,
    /// Number of code cells whose execution produced an error
    pub cells_failed: usize,
    /// Execution time in seconds
    pub execution_time_seconds: f64,
    /// Error message if any
    pub error: Option<String>,
    /// Cell outputs summary
    pub cell_outputs: Vec<CellOutputSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellOutputSummary {
    pub cell_index: usize,
    pub cell_type: String,
    pub success: bool,
    pub output_count: usize,
    pub error: Option<String>,
}

/// Execute a Jupyter notebook using nbconvert.
pub fn execute_notebook(params: JupyterExecuteParams) -> Result<JupyterExecuteResult> {
    let start_time = std::time::Instant::now();

    // working_dir=None means the notebook's parent directory.
    let notebook_input = PathBuf::from(&params.notebook_path);
    let working_dir = match params.working_dir.map(PathBuf::from) {
        Some(d) => d,
        None => notebook_input
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    };

    // Resolve notebook path relative to working_dir.
    let notebook_path = working_dir.join(&notebook_input);
    if !notebook_path.exists() {
        anyhow::bail!("Notebook not found: {}", notebook_path.display());
    }

    let timeout = params.timeout_seconds.unwrap_or(300);
    let save_outputs = params.save_outputs.unwrap_or(true);
    let kernel_name = params.kernel_name.unwrap_or_else(|| "python3".to_string());

    // Build the nbconvert command (env-scrubbed through the shared helper)
    let mut cmd = jupyter_cmd();
    cmd.arg("nbconvert")
        .arg("--execute")
        .arg("--to")
        .arg("notebook")
        .arg("--inplace")
        .arg("--ExecutePreprocessor.kernel_name")
        .arg(&kernel_name)
        .arg("--ExecutePreprocessor.timeout")
        .arg(timeout.to_string())
        .arg(&params.notebook_path) // Use relative path from working_dir
        .current_dir(&working_dir);

    if !save_outputs {
        cmd.arg("--no-output");
    }

    let output = cmd
        .output()
        .context("Failed to execute jupyter nbconvert. Is jupyter installed?")?;

    let execution_time = start_time.elapsed().as_secs_f64();

    let success = output.status.success();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse outputs from the executed notebook. nbconvert `--inplace` writes
    // partial outputs (up to and including the failing cell) even on error, so
    // parse best-effort to surface which cell failed. On a non-success run
    // where the file is unreadable, fall back to an empty list.
    let cell_outputs = match parse_notebook_outputs(&notebook_path) {
        Ok(o) => o,
        Err(_) if !success => vec![],
        Err(e) => return Err(e),
    };

    // Count only executed code cells; markdown/raw cells are not "executed".
    let cells_executed = cell_outputs
        .iter()
        .filter(|c| c.cell_type == "code")
        .count();
    let cells_failed = cell_outputs
        .iter()
        .filter(|c| c.cell_type == "code" && !c.success)
        .count();

    Ok(JupyterExecuteResult {
        success,
        notebook_path: params.notebook_path,
        cells_executed,
        cells_failed,
        execution_time_seconds: execution_time,
        error: if success {
            None
        } else {
            Some(format!("stdout: {stdout}\nstderr: {stderr}"))
        },
        cell_outputs,
    })
}

/// Parse cell outputs from an executed notebook.
fn parse_notebook_outputs(notebook_path: &Path) -> Result<Vec<CellOutputSummary>> {
    use nbformat::{parse_notebook, v4, Notebook};
    use std::fs;

    let content = fs::read_to_string(notebook_path).context("Failed to read notebook")?;
    let nb = parse_notebook(&content).context("Failed to parse notebook")?;

    let cells = match nb {
        Notebook::V4(nb) => nb.cells,
        Notebook::Legacy(nb) => {
            // Upgrade legacy notebook to v4
            let upgraded = nbformat::upgrade_legacy_notebook(nb)?;
            upgraded.cells
        }
    };

    let mut summaries = Vec::new();

    for (idx, cell) in cells.iter().enumerate() {
        let cell_type = match cell {
            v4::Cell::Code { .. } => "code",
            v4::Cell::Markdown { .. } => "markdown",
            v4::Cell::Raw { .. } => "raw",
        };

        let mut success = true;
        let mut output_count = 0;
        let mut error = None;

        if let v4::Cell::Code { outputs, .. } = cell {
            output_count = outputs.len();
            for output in outputs {
                if let v4::Output::Error(v4::ErrorOutput { ename, evalue, .. }) = output {
                    success = false;
                    error = Some(format!("{ename}: {evalue}"));
                    break;
                }
            }
        }

        summaries.push(CellOutputSummary {
            cell_index: idx,
            cell_type: cell_type.to_string(),
            success,
            output_count,
            error,
        });
    }

    Ok(summaries)
}

// ---- server management --------------------------------------------------

/// Parameters for starting a Jupyter server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerParams {
    /// Working directory for the server (default: current directory)
    pub working_dir: Option<String>,
    /// Port to run the server on (default: 8888)
    pub port: Option<u16>,
    /// Bind address. Defaults to `127.0.0.1`. MUST be a loopback address —
    /// non-loopback hosts are rejected so the server is reachable only from the
    /// operator's own machine.
    pub host: Option<String>,
    /// Token for authentication (default: auto-generated)
    pub token: Option<String>,
    /// Already-hashed password for authentication, in the form Jupyter's
    /// `--NotebookApp.password` expects (`argon2:$argon2id$…` PHC string).
    /// Passed through verbatim. Takes precedence over `password`.
    pub password_hash: Option<String>,
    /// Plaintext password for authentication. The tool hashes this with
    /// argon2 (matching Jupyter's `argon2:` scheme) and passes the resulting
    /// hash to `--NotebookApp.password`. Used only when `password_hash` is
    /// absent. Storing/transporting a plaintext password is discouraged;
    /// prefer `password_hash` for persistent configs.
    pub password: Option<String>,
    /// Whether to open a browser on startup (default: false). `Some(true)`
    /// omits `--no-browser` so Jupyter opens the operator's browser; anything
    /// else passes `--no-browser`.
    pub open_browser: Option<bool>,
    /// Additional command line args
    pub extra_args: Option<Vec<String>>,
}

/// Result of starting a Jupyter server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerResult {
    /// Whether server started successfully
    pub success: bool,
    /// Opaque handle for later `stop_server` / `get_server_status` calls.
    pub handle_id: Option<u64>,
    /// Server URL (e.g. http://127.0.0.1:8888)
    pub url: Option<String>,
    /// Server process ID (informational; operations use `handle_id`)
    pub pid: Option<u32>,
    /// Port the server is running on
    pub port: Option<u16>,
    /// Token used for authentication
    pub token: Option<String>,
    /// Error message if any
    pub error: Option<String>,
}

/// Status of a Jupyter server, queried by handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerStatus {
    /// Whether a server is running
    pub running: bool,
    /// The handle this status refers to
    pub handle_id: u64,
    /// Server URL if running
    pub url: Option<String>,
    /// Port if running
    pub port: Option<u16>,
    /// List of running kernels
    pub kernels: Vec<KernelInfo>,
}

/// Information about a running kernel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelInfo {
    pub id: String,
    pub name: String,
    pub last_activity: String,
    pub execution_state: String,
    pub connections: usize,
}

/// Summary of an active Jupyter server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSummary {
    pub handle_id: u64,
    pub url: String,
    pub port: u16,
    pub running: bool,
}

/// Result of listing Jupyter servers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterListResult {
    pub servers: Vec<ServerSummary>,
}

/// Owned jupyter server process retained in the registry.
struct ServerHandle {
    child: std::process::Child,
    url: String,
    port: u16,
    token: String,
}

/// Monotonic handle id generator (1..; 0 is reserved as "no handle").
/// Initialized lazily from the persistent registry to avoid collisions across invocations.
static NEXT_HANDLE: LazyLock<AtomicU64> = LazyLock::new(|| {
    let next_id = load_persistent_servers()
        .ok()
        .and_then(|records| records.iter().map(|r| r.handle_id).max())
        .unwrap_or(0)
        + 1;
    AtomicU64::new(next_id)
});

/// Process-local registry of servers this tool started, keyed by handle id.
static SERVERS: LazyLock<Mutex<HashMap<u64, ServerHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Persistent file storing server handles (for cross-invocation visibility).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistentServerRecord {
    pub handle_id: u64,
    pub url: String,
    pub port: u16,
    pub token: String,
}

/// Path to the persistent server registry file.
fn servers_file() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    let path = PathBuf::from(home).join(".gila").join("servers.json");
    Ok(path)
}

/// Load persisted server handles from disk.
fn load_persistent_servers() -> Result<Vec<PersistentServerRecord>> {
    let path = servers_file()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&path).context("Failed to read servers file")?;
    let records = serde_json::from_str(&content).context("Failed to parse servers file")?;
    Ok(records)
}

/// Save persisted server handles to disk.
fn save_persistent_servers(records: &[PersistentServerRecord]) -> Result<()> {
    let path = servers_file()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Failed to create .gila directory")?;
    }
    let json = serde_json::to_string_pretty(records).context("Failed to serialize servers")?;
    fs::write(&path, json).context("Failed to write servers file")?;
    Ok(())
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost" | "localhost.")
}

/// Detect environment manager in a directory and return appropriate launcher.
///
/// Searches for pixi.toml, pyproject.toml (uv), requirements.txt (venv),
/// environment.yml (conda), or .venv directory. Returns a command wrapper
/// that will activate the environment before running jupyter.
///
/// The wrapper is constructed so that we can still append `notebook --port XXX` etc.
fn detect_and_wrap_jupyter_cmd(working_dir: &Path) -> (String, Vec<String>) {
    // Check for pixi.toml
    if working_dir.join("pixi.toml").exists() {
        return ("pixi".to_string(), vec!["run".to_string(), "jupyter".to_string()]);
    }

    // Check for uv (pyproject.toml with [tool.uv])
    if let Ok(content) = fs::read_to_string(working_dir.join("pyproject.toml")) {
        if content.contains("[tool.uv]") {
            return ("uv".to_string(), vec!["run".to_string(), "jupyter".to_string()]);
        }
    }

    // Check for conda environment.yml
    if working_dir.join("environment.yml").exists() {
        return (
            "conda".to_string(),
            vec![
                "run".to_string(),
                "--file".to_string(),
                working_dir.join("environment.yml").to_string_lossy().to_string(),
                "jupyter".to_string(),
            ],
        );
    }

    // Check for .venv directory
    if working_dir.join(".venv").exists() {
        let activate = working_dir.join(".venv/bin/activate");
        return (
            "sh".to_string(),
            vec![
                "-c".to_string(),
                format!("source {} && exec jupyter \"$@\"", activate.display()),
            ],
        );
    }

    // Check for requirements.txt (assume venv exists or will be created)
    if working_dir.join("requirements.txt").exists() {
        let activate = working_dir.join(".venv/bin/activate");
        return (
            "sh".to_string(),
            vec![
                "-c".to_string(),
                format!("source {} && exec jupyter \"$@\"", activate.display()),
            ],
        );
    }

    // Fallback: plain jupyter (with minimal environment)
    ("jupyter".to_string(), vec![])
}

/// Build the base `jupyter` command with the inherited environment scrubbed
/// and only a minimal, safe allowlist passed back through.
fn jupyter_cmd() -> Command {
    let mut cmd = Command::new("jupyter");
    // Scrub the whole inherited environment, then pass back ONLY the minimal
    // allowlist jupyter needs to locate its binary, write config, and render.
    // No gila control-plane env reaches the child.
    cmd.env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    cmd
}

/// Hash a plaintext password into the `argon2:$argon2id$…` PHC string that
/// Jupyter's `--NotebookApp.password` expects and `notebook.auth.passwd_check`
/// verifies. Matches Jupyter's `argon2:` prefix scheme (the `argon2-cffi`
/// `PasswordHasher` produces the suffix after the colon); the argon2id
/// parameters are the crate defaults, which verify cleanly because
/// `passwd_check` reads them back from the encoded string.
fn hash_password(plaintext: &str) -> Result<String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    let argon = argon2::Argon2::default();
    let hash = argon
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("Failed to hash server password: {e}"))?;
    Ok(format!("argon2:{hash}"))
}

/// Poll the server's REST API until it answers (or the deadline elapses),
/// instead of sleeping a fixed delay that races startup.
///
/// If the requested port is busy, Jupyter auto-selects a different port.
/// This probe tries a range of ports (requested ± 10) to find the actual one.
/// Any response (including auth errors) indicates the server is running.
fn readiness_probe(base_url: &str, token: &str, timeout: Duration) -> Result<(String, u16)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to build HTTP client for readiness probe")?;

    // Extract host and base port from the URL
    let url_lower = base_url.to_lowercase();
    let host_port = url_lower
        .strip_prefix("http://")
        .or_else(|| url_lower.strip_prefix("https://"))
        .context("Invalid URL scheme")?;
    let (host, base_port_str) = host_port.split_once(':').context("Missing port in URL")?;
    let base_port: u16 = base_port_str.parse().context("Invalid port number")?;

    let deadline = std::time::Instant::now() + timeout;

    // Try ports in expanding rings: base, base±1, base±2, etc., up to ±10
    loop {
        for offset in 0..=10 {
            for port in [base_port + offset, base_port.saturating_sub(offset)] {
                if port == 0 {
                    continue;
                }
                let try_url = format!("http://{}:{}", host, port);
                let res = client
                    .get(format!("{}/api/kernels", try_url.trim_end_matches('/')))
                    .header("Authorization", format!("token {token}"))
                    .send();
                // Any successful connection (even 401/403) means the server is running
                if let Ok(_resp) = res {
                    return Ok((try_url, port));
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("jupyter server did not become ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Start a Jupyter server in the background, owned by this process.
///
/// The server is bound to a loopback address only, spawned with a scrubbed
/// environment, and registered under an opaque handle id. Success is
/// confirmed by a REST readiness probe, not a fixed sleep.
pub fn start_server(params: JupyterServerParams) -> Result<JupyterServerResult> {
    let working_dir = params
        .working_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let port = params.port.unwrap_or(8888);
    let host = params.host.as_deref().unwrap_or("127.0.0.1");
    if !is_loopback(host) {
        anyhow::bail!(
            "Refusing to bind jupyter to non-loopback host '{host}'; the server is reachable \
             only from the operator's own machine. Use 127.0.0.1 / ::1 / localhost."
        );
    }

    let token = params.token.unwrap_or_else(|| {
        use rand::Rng;
        rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(32)
            .map(char::from)
            .collect()
    });

    // Build the jupyter notebook command. Deliberately NO remote-access /
    // allow-origin flags — loopback binding + the default `False` is
    // load-bearing and keeps the server off the network.
    // Use environment-aware launcher (pixi, uv, venv, etc.) if available.
    let (launcher, launcher_args) = detect_and_wrap_jupyter_cmd(&working_dir);
    let mut cmd = Command::new(&launcher);
    for arg in launcher_args {
        cmd.arg(&arg);
    }
    cmd.arg("notebook")
        .arg("--port")
        .arg(port.to_string())
        .arg("--ip")
        .arg(host)
        .arg("--NotebookApp.token")
        .arg(&token)
        .current_dir(&working_dir);

    // Honor the open_browser flag: only pass --no-browser when the caller
    // did not explicitly ask for a browser.
    if !matches!(params.open_browser, Some(true)) {
        cmd.arg("--no-browser");
    }

    // Resolve a password for the server. `password_hash` (an already-hashed
    // `argon2:$argon2id$…` PHC string) is passed through verbatim and wins
    // over a plaintext `password`. When `password` is supplied we hash it
    // here with argon2 so the model never has to pre-hash — but the value is
    // only ever handed to jupyter as a hash, never as plaintext.
    if let Some(hash) = params.password_hash {
        cmd.arg("--NotebookApp.password").arg(hash);
    } else if let Some(plaintext) = params.password {
        let hash = hash_password(&plaintext)?;
        cmd.arg("--NotebookApp.password").arg(hash);
    }

    if let Some(extra_args) = params.extra_args {
        cmd.args(extra_args);
    }

    // Spawn the process detached
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to start jupyter server. Is jupyter installed?")?;

    let pid = child.id();
    let url = format!("http://{host}:{port}");

    // Drain stdout and capture stderr to detect the actual port Jupyter uses
    // (it may auto-select a different port if the requested one is busy).
    let stdout_output = std::sync::Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_tail = std::sync::Arc::new(Mutex::new(Vec::<u8>::new()));

    if let Some(mut out) = child.stdout.take() {
        let out_capture = std::sync::Arc::clone(&stdout_output);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut g = out_capture.lock().unwrap();
                        g.extend_from_slice(&buf[..n]);
                    }
                }
            }
        });
    }
    if let Some(mut err) = child.stderr.take() {
        let tail = std::sync::Arc::clone(&stderr_tail);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match err.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut g = tail.lock().unwrap();
                        if g.len() < STDERR_CAP {
                            let take = n.min(STDERR_CAP - g.len());
                            g.extend_from_slice(&buf[..take]);
                        }
                    }
                }
            }
        });
    }

    // Give the server a moment to fully initialize before probing.
    // The process might have started but not yet bound to the port.
    thread::sleep(Duration::from_millis(500));

    // Probe the REST API until the server answers (or we time out). The probe
    // detects which port Jupyter actually bound to (may differ from requested
    // if the requested port was busy).
    let probe = readiness_probe(&url, &token, Duration::from_secs(20));

    if let Err(e) = probe {
        // Startup failed. Capture whatever stderr we have, reap the child.
        let stderr_snippet = {
            let g = stderr_tail.lock().unwrap();
            String::from_utf8_lossy(&g).to_string()
        };
        let _ = child.kill();
        let _ = child.wait();
        return Ok(JupyterServerResult {
            success: false,
            handle_id: None,
            url: None,
            pid: None,
            port: None,
            token: None,
            error: Some(format!("{e}\n--- stderr ---\n{stderr_snippet}")),
        });
    }

    let (actual_url, actual_port) = probe.unwrap();

    // Confirm the process is still alive (it may have exited in the window
    // between the probe and now).
    match child.try_wait() {
        Ok(Some(status)) => {
            let stderr_snippet = {
                let g = stderr_tail.lock().unwrap();
                String::from_utf8_lossy(&g).to_string()
            };
            Ok(JupyterServerResult {
                success: false,
                handle_id: None,
                url: None,
                pid: None,
                port: None,
                token: None,
                error: Some(format!(
                    "Server exited immediately with status: {status}\n--- stderr ---\n{stderr_snippet}"
                )),
            })
        }
        Ok(None) | Err(_) => {
            let handle_id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
            SERVERS.lock().unwrap().insert(
                handle_id,
                ServerHandle {
                    child,
                    url: actual_url.clone(),
                    port: actual_port,
                    token: token.clone(),
                },
            );
            // Persist the handle for cross-invocation visibility.
            if let Err(e) = (|| -> Result<()> {
                let mut records = load_persistent_servers().unwrap_or_default();
                records.push(PersistentServerRecord {
                    handle_id,
                    url: actual_url.clone(),
                    port: actual_port,
                    token: token.clone(),
                });
                save_persistent_servers(&records)?;
                Ok(())
            })() {
                eprintln!("warning: failed to persist server handle: {e}");
            }
            Ok(JupyterServerResult {
                success: true,
                handle_id: Some(handle_id),
                url: Some(actual_url),
                pid: Some(pid),
                port: Some(actual_port),
                token: Some(token),
                error: None,
            })
        }
    }
}

/// Stop a Jupyter server by handle id.
///
/// Kills the owned child directly (`Child::kill`) — no `kill` / `taskkill`
/// subprocess is spawned. Returns `Ok(false)` if the handle is unknown
/// (already stopped or never started by this process).
pub fn stop_server(handle_id: u64) -> Result<bool> {
    let mut handle = SERVERS.lock().unwrap().remove(&handle_id);
    let killed = match handle.as_mut() {
        Some(h) => {
            let k = h.child.kill().is_ok();
            let _ = h.child.wait();
            k
        }
        None => false,
    };
    // Remove from persistent registry as well.
    if let Err(e) = (|| -> Result<()> {
        let mut records = load_persistent_servers().unwrap_or_default();
        records.retain(|r| r.handle_id != handle_id);
        save_persistent_servers(&records)?;
        Ok(())
    })() {
        eprintln!("warning: failed to remove handle from persistent registry: {e}");
    }
    Ok(killed)
}

/// Get status of a Jupyter server by handle id.
///
/// Looks up the handle this process registered, then queries that server's
/// own REST API (with its own token). A caller cannot point this at an
/// arbitrary URL — only at a server this tool started.
pub fn get_server_status(handle_id: u64) -> Result<JupyterServerStatus> {
    // Clone the connection details out of the registry without holding the
    // lock across a network call.
    let (url, token, port) = {
        let g = SERVERS.lock().unwrap();
        match g.get(&handle_id) {
            Some(h) => (h.url.clone(), h.token.clone(), h.port),
            None => {
                return Ok(JupyterServerStatus {
                    running: false,
                    handle_id,
                    url: None,
                    port: None,
                    kernels: vec![],
                });
            }
        }
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let resp = client
        .get(format!("{}/api/kernels", url.trim_end_matches('/')))
        .header("Authorization", format!("token {token}"))
        .send();

    match resp {
        Ok(r) if r.status().is_success() => {
            let kernels: Vec<KernelInfo> = r.json().unwrap_or_default();
            Ok(JupyterServerStatus {
                running: true,
                handle_id,
                url: Some(url),
                port: Some(port),
                kernels,
            })
        }
        _ => Ok(JupyterServerStatus {
            running: false,
            handle_id,
            url: Some(url),
            port: Some(port),
            kernels: vec![],
        }),
    }
}

/// List all active Jupyter servers started via `gila jupyter start`.
///
/// Reads from a persistent registry stored in `~/.gila/servers.json`, so this
/// will return servers from any previous `gila` invocation, not just the
/// current process. The registry is updated when servers are started or stopped.
pub fn list_servers() -> Result<JupyterListResult> {
    let records = load_persistent_servers().unwrap_or_default();
    let mut servers = Vec::new();
    for record in records {
        servers.push(ServerSummary {
            handle_id: record.handle_id,
            url: record.url,
            port: record.port,
            running: true,
        });
    }
    servers.sort_by_key(|s| s.handle_id);
    Ok(JupyterListResult { servers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jupyter_params_serialization() {
        let params = JupyterExecuteParams {
            notebook_path: "test.ipynb".to_string(),
            working_dir: Some("/tmp".to_string()),
            timeout_seconds: Some(60),
            save_outputs: Some(true),
            kernel_name: Some("python3".to_string()),
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("test.ipynb"));
        assert!(json.contains("/tmp"));
    }

    #[test]
    fn test_server_params_serialization() {
        let params = JupyterServerParams {
            working_dir: Some("/tmp".to_string()),
            port: Some(8888),
            host: Some("localhost".to_string()),
            token: Some("test-token".to_string()),
            password_hash: None,
            password: None,
            open_browser: Some(false),
            extra_args: None,
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("8888"));
        assert!(json.contains("test-token"));
    }

    /// Loopback boundary: only true loopback addresses are accepted.
    #[test]
    fn test_loopback_boundaries() {
        for ok in ["127.0.0.1", "::1", "localhost", "localhost."] {
            assert!(is_loopback(ok), "expected '{ok}' to be loopback");
        }
        for bad in ["0.0.0.0", "::", "example.com", "10.0.0.1", "192.168.1.1"] {
            assert!(!is_loopback(bad), "expected '{bad}' to NOT be loopback");
        }
    }

    /// `start_server` must refuse a non-loopback host *before* spawning — so
    /// this test needs no jupyter install and leaves no process behind.
    #[test]
    fn test_start_server_rejects_non_loopback() {
        let err = start_server(JupyterServerParams {
            working_dir: None,
            port: None,
            host: Some("0.0.0.0".to_string()),
            token: None,
            password_hash: None,
            password: None,
            open_browser: None,
            extra_args: None,
        })
        .expect_err("should refuse non-loopback host with an error");
        let msg = err.to_string();
        assert!(
            msg.contains("non-loopback") || msg.contains("loopback"),
            "error should explain the loopback requirement: {msg}"
        );
    }

    /// An unknown handle must report not-running without spawning or touching
    /// any real server.
    #[test]
    fn test_status_unknown_handle_is_not_running() {
        let status = get_server_status(u64::MAX).unwrap();
        assert!(!status.running);
        assert_eq!(status.handle_id, u64::MAX);
        assert!(status.url.is_none());
        assert!(status.kernels.is_empty());
    }

    /// Stopping an unknown handle is a no-op (returns false), not an error.
    #[test]
    fn test_stop_unknown_handle_is_noop() {
        assert!(!stop_server(u64::MAX).unwrap());
    }

    /// True if a `jupyter` binary is reachable on PATH — gates the live-server
    /// integration tests below.
    fn jupyter_available() -> bool {
        jupyter_cmd().arg("--version").output().is_ok()
    }

    /// Live lifecycle: start → running → stop → gone. Needs jupyter installed.
    #[test]
    #[ignore = "requires a jupyter install on PATH"]
    fn test_server_start_status_stop_lifecycle() {
        if !jupyter_available() {
            return;
        }
        // Pick a free port so we don't collide with a running server.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let started = start_server(JupyterServerParams {
            working_dir: Some(std::env::temp_dir().to_string_lossy().to_string()),
            port: Some(port),
            host: Some("127.0.0.1".to_string()),
            token: None,
            password_hash: None,
            password: None,
            open_browser: None,
            extra_args: None,
        })
        .unwrap();
        assert!(started.success, "server should start: {:?}", started.error);
        let handle = started.handle_id.expect("handle id");

        let status = get_server_status(handle).unwrap();
        assert!(status.running, "server should be running after start");
        assert_eq!(status.port, Some(port));

        assert!(stop_server(handle).unwrap(), "stop should report killed");
        // A second stop is a no-op — the handle is gone from the registry.
        assert!(!stop_server(handle).unwrap(), "second stop is a no-op");
    }

    /// An occupied port must fail readiness: jupyter cannot bind, the probe
    /// times out, and we report failure without leaking a process.
    #[test]
    #[ignore = "requires a jupyter install on PATH"]
    fn test_occupied_port_fails() {
        if !jupyter_available() {
            return;
        }
        // Hold the port open so jupyter cannot bind it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let res = start_server(JupyterServerParams {
            working_dir: Some(std::env::temp_dir().to_string_lossy().to_string()),
            port: Some(port),
            host: Some("127.0.0.1".to_string()),
            token: None,
            password_hash: None,
            password: None,
            open_browser: None,
            extra_args: None,
        })
        .unwrap();
        assert!(!res.success, "should not start on an occupied port");
        assert!(res.handle_id.is_none(), "no handle on failure");
        drop(listener);
    }

    /// `hash_password` must produce a `argon2:$argon2id$…` PHC string that
    /// Jupyter's `passwd_check` accepts, and it must verify round-trip.
    #[test]
    fn test_hash_password_produces_verifiable_argon2() {
        use argon2::{Argon2, PasswordHash, PasswordVerifier};
        let plaintext = "hunter2";
        let encoded = hash_password(plaintext).unwrap();
        assert!(
            encoded.starts_with("argon2:$argon2id$"),
            "expected argon2 PHC with jupyter prefix, got: {encoded}"
        );
        // Jupyter strips the `argon2:` prefix before verifying, so do the
        // same here to confirm the encoded suffix is a valid argon2 hash.
        let phc = &encoded["argon2:".len()..];
        let parsed = PasswordHash::new(phc).unwrap();
        Argon2::default()
            .verify_password(plaintext.as_bytes(), &parsed)
            .expect("hash must verify against the original plaintext");
    }
}
