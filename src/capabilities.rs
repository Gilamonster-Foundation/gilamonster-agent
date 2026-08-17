//! `gila capabilities` — discover and exercise installed Gilamonster capabilities.
//!
//! A *capability* (see `Gilamonster-Foundation/gilamonster-capabilities`) ships
//! as a Python package that registers a `gilamonster.capabilities` entry point
//! and a `gila-cap-<name>-mcp` MCP server. gila consumes it the way it consumes
//! any MCP surface — over newt's MCP client:
//!
//! - [`list`]   — enumerate installed capabilities (the entry-point group).
//! - [`check`]  — connect to one over newt's MCP client and exercise its tools.
//! - [`enable`] — print the `[[mcp_servers]]` snippet to wire it into a session.
//!
//! This is the host side of the capability framework: a pip-installed capability
//! becomes reachable in a gila session with no gila rebuild.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_bridle_core::{Caveats, ConfinedCommand, Gate, Scope, Tool, ToolContext, ToolResult};
use anyhow::{Context, Result};
use newt_core::mcp::{McpServerEntry, McpTrust, TransportKind};
use newt_core::Config;
use newt_mcp_client::{McpConnection, StdioTransport};
use serde_json::json;

use crate::manifest::{CapabilityEntry, Expose, Manifest};
use crate::venv::{self, GilacapCmd};

/// Load the selection manifest (best-effort: a missing file or unset HOME yields
/// the empty, CLI-only-default manifest).
fn manifest() -> Manifest {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    Manifest::load(&Manifest::default_path(&home)).unwrap_or_default()
}

/// Resolve the `gilacap` that serves capability `name` — its venv override, else
/// the manifest global, else the env / managed-default chain ([`venv::resolve`]).
fn gilacap_for(name: &str) -> GilacapCmd {
    let venv_override = manifest().venv_for(name).map(str::to_string);
    venv::from_env(venv_override.as_deref())
}

/// Resolve the `gilacap` for registry-wide ops (`list`) — the manifest global.
fn gilacap_global() -> GilacapCmd {
    let venv_override = manifest().venv.clone();
    venv::from_env(venv_override.as_deref())
}

/// The stdio [`McpServerEntry`] that serves capability `name` via
/// `gilacap mcp <name>`. Pure in `g`/`name`, so the entry shape is unit-tested.
fn entry_for_with(g: &GilacapCmd, name: &str) -> McpServerEntry {
    McpServerEntry {
        name: name.to_string(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some(g.program.clone()),
        args: g.argv(&["mcp", name]),
        env: Default::default(),
        url: None,
        headers: Default::default(),
        request_timeout_secs: None,
        // gila-owned wiring (the manifest the operator edited) — trusted config.
        trust: McpTrust::Trusted,
    }
}

/// The argv for `gila cap run` → `gilacap run <name> <tool> [--args <json>]`.
/// Pure so the dispatch is unit-tested without spawning a subprocess.
fn run_argv(g: &GilacapCmd, name: &str, tool: &str, args: Option<&str>) -> Vec<String> {
    let mut argv = g.argv(&["run", name, tool]);
    if let Some(a) = args {
        argv.push("--args".to_string());
        argv.push(a.to_string());
    }
    argv
}

/// `gila capabilities list` — enumerate installed capabilities through the
/// `gilacap` multiplexer (`gilacap list` reads the `gilamonster.capabilities`
/// registry in the resolved venv — the managed `~/.gila/caps-venv` by default).
pub fn list() -> Result<()> {
    let g = gilacap_global();
    let status = Command::new(&g.program)
        .args(g.argv(&["list"]))
        .status()
        .with_context(|| {
            format!(
                "running `{}` — is the caps venv set up? (e.g. \
                 `python3 -m venv ~/.gila/caps-venv && \
                 ~/.gila/caps-venv/bin/pip install gilamonster-capability`)",
                g.program
            )
        })?;
    if !status.success() {
        anyhow::bail!("`gilacap list` exited with {status}");
    }
    Ok(())
}

/// A trivial leash-bearing tool used only to mint a [`ToolContext`] for a
/// host-initiated confined spawn. `ToolContext` is constructible solely inside a
/// gate, so the host authorizes itself through the gate to obtain the spawn token
/// — the same discipline a real tool follows.
struct CapRunTool;

#[async_trait::async_trait]
impl Tool for CapRunTool {
    fn name(&self) -> &str {
        "gila-cap-run"
    }
    fn schema(&self) -> serde_json::Value {
        json!({})
    }
    async fn invoke(
        &self,
        _args: serde_json::Value,
        _cx: &ToolContext,
    ) -> ToolResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

/// The default caveats for a manifest-confined capability spawn: it may exec only
/// its own launcher, and (the L3-enforced axis) write only under the working dir
/// and the temp dir (caps commonly use tempfiles). `fs_read`/`net` are not
/// L3-governed yet, so they stay unrestricted/advisory (a `net` allow-list per
/// cap is a manifest follow-up; the real external-effect mitigation is a scoped
/// token, not the net axis).
fn confined_caveats(program: &str, cwd: &Path) -> Caveats {
    Caveats {
        exec: Scope::only([program.to_string()]),
        fs_write: Scope::only([
            cwd.to_string_lossy().into_owned(),
            std::env::temp_dir().to_string_lossy().into_owned(),
        ]),
        ..Caveats::top()
    }
}

/// Build the child's environment allow-list (it inherits **nothing** else): the
/// manifest's granted vars (creds/config) plus the few essentials the interpreter
/// and the `~/.confluence/token`-style fallbacks need. `get` is injected for
/// testability.
fn env_allow_with(
    entry: Option<&CapabilityEntry>,
    get: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for var in entry.map(|e| e.env.as_slice()).unwrap_or(&[]) {
        if let Some(v) = get(var) {
            out.push((var.clone(), v));
        }
    }
    for essential in ["HOME", "PATH", "LANG"] {
        if let Some(v) = get(essential) {
            out.push((essential.to_string(), v));
        }
    }
    out
}

fn env_allow(entry: Option<&CapabilityEntry>) -> Vec<(String, String)> {
    env_allow_with(entry, &|k| std::env::var(k).ok())
}

/// Spawn the capability **confined** by agent-bridle: a Landlock `fs_write`
/// domain + a scrubbed environment, applied before exec. Fails closed if the
/// platform cannot enforce the restriction (the `ConfinedCommand` contract).
fn run_confined(
    g: &GilacapCmd,
    argv: &[String],
    name: &str,
    tool: &str,
    entry: Option<&CapabilityEntry>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("resolving the working directory")?;
    let caveats = confined_caveats(&g.program, &cwd);
    // Mint the spawn token through the gate (the only mint site).
    let cx = Gate::new(0)
        .authorize(&CapRunTool, &caveats)
        .map_err(|e| anyhow::anyhow!("authorizing confined spawn: {e}"))?;

    let mut cmd = ConfinedCommand::new(g.program.as_str());
    for a in argv {
        cmd = cmd.arg(a);
    }
    for (k, v) in env_allow(entry) {
        cmd = cmd.env(k, v);
    }
    let mut spawned = cmd
        .spawn(&cx)
        .map_err(|e| anyhow::anyhow!("confined spawn of `{}` ({name}:{tool}): {e}", g.program))?;
    eprintln!(
        "→ {name}:{tool} running confined (sandbox: {:?})",
        spawned.sandbox_kind
    );
    let status = spawned
        .child
        .wait()
        .context("waiting for the confined capability")?;
    if !status.success() {
        anyhow::bail!("capability '{name}' tool '{tool}' exited with {status}");
    }
    Ok(())
}

/// `gila capabilities run <name> <tool> [--args '{…}']` — invoke one capability
/// tool through the `gilacap` multiplexer and stream its result.
///
/// If the manifest marks the capability `confined`, the spawn goes through
/// agent-bridle (`ConfinedCommand` → Landlock `fs_write` + env scrub, fail-closed
/// when the OS cannot enforce). Otherwise it is a bare spawn — a human at the
/// prompt runs under their own authority, which needs no leash. (The next layer,
/// a signed `newt_identity::delegate_for_plugin` envelope passed to caps that
/// themselves re-enter bridle, is a follow-up.)
pub fn run(name: &str, tool: &str, args: Option<String>) -> Result<()> {
    let m = manifest();
    let g = gilacap_for(name);
    let argv = run_argv(&g, name, tool, args.as_deref());
    let entry = m.entry(name);
    if entry.map(|e| e.confined).unwrap_or(false) {
        return run_confined(&g, &argv, name, tool, entry);
    }
    let status = Command::new(&g.program)
        .args(&argv)
        .status()
        .with_context(|| format!("spawning `{}` to run {name}:{tool}", g.program))?;
    if !status.success() {
        anyhow::bail!("capability '{name}' tool '{tool}' exited with {status}");
    }
    Ok(())
}

/// `gila capabilities check <name>` — connect to the capability's MCP server
/// over newt's MCP client and exercise it (list tools + a sample call).
///
/// This is the end-to-end proof that a capability works *through gila*: it
/// spawns `gila-cap-<name>-mcp` and speaks the same stdio JSON-RPC the inherited
/// TUI speaks.
pub async fn check(name: &str) -> Result<()> {
    let g = gilacap_for(name);
    let entry = entry_for_with(&g, name);
    println!(
        "→ spawning `{} mcp {name}` and connecting via newt's MCP client …",
        g.program
    );
    // The leash the capability server is spawned under — the operator's own
    // configured permissions (else newt's read-only default), exactly the
    // confinement a live session applies. Mirrors `newt doctor` / `newt mcp
    // probe` (Config::mcp_probe_caveats — never `top()` in a dispatch path).
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let leash = Config::resolve()
        .unwrap_or_default()
        .mcp_probe_caveats(&workspace);
    // Newt's admission witness makes disabled/untrusted entries
    // unrepresentable at the spawn boundary. Gila-created entries are enabled
    // and trusted, but still pass through the same gate as the inherited TUI.
    let admitted = newt_core::mcp::admit(&entry)
        .map_err(|denied| anyhow::anyhow!("MCP server admission denied: {denied}"))?;
    let transport = StdioTransport::spawn(&admitted, &leash).with_context(|| {
        format!(
            "spawning `{} mcp {name}` — is the caps venv set up? \
             (try `~/.gila/caps-venv/bin/pip install gila-cap-{name}`)",
            g.program
        )
    })?;
    let mut conn = McpConnection::new(transport);
    conn.initialize().await.context("MCP initialize")?;

    let tools = conn.list_tools().await.context("tools/list")?;
    println!("✓ connected — {} tool(s):", tools.len());
    for t in &tools {
        println!("  - {:<22} {}", t.name, t.description);
    }

    // Exercise a real round-trip if the storyboard validator is present.
    if tools.iter().any(|t| t.name == "storyboard_validate") {
        let args = json!({
            "storyboard": {"title": "capabilities check", "scenes": [{"id": "a", "narration": "hello"}]}
        });
        let res = conn
            .call_tool("storyboard_validate", args)
            .await
            .context("tools/call storyboard_validate")?;
        let text = res
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("(no text content)");
        println!("\n✓ called storyboard_validate →\n  {text}");
    }

    println!("\ncapability '{name}' is reachable and callable through gila.");
    Ok(())
}

/// `gila capabilities enable <name>` — print the newt `[[mcp_servers]]` snippet
/// that wires the capability into every gila session (its tools then arrive
/// namespaced `<name>__<tool>` through newt's MCP client). The interactive
/// `gila cap config` selector (which writes `~/.gila/capabilities.toml`) lands
/// next; this stays the copy-paste path.
pub fn enable(name: &str) -> Result<()> {
    let g = gilacap_for(name);
    let args = g.argv(&["mcp", name]);
    println!("Add this to ~/.newt/config.toml to wire '{name}' into gila sessions:\n");
    println!("[[mcp_servers]]");
    println!("name = \"{name}\"");
    println!("command = \"{}\"", g.program);
    println!("args = {args:?}");
    println!("\nThen `gila code` exposes its tools (namespaced `{name}__<tool>`).");
    Ok(())
}

/// Parse capability names from `gilacap list` output (each line is
/// `NAME<TAB>description`). Pure, so the parsing is unit-tested.
fn parse_list_names(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|l| l.split('\t').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Enumerate installed capability names via `gilacap list`.
fn installed_names(g: &GilacapCmd) -> Result<Vec<String>> {
    let out = Command::new(&g.program)
        .args(g.argv(&["list"]))
        .output()
        .with_context(|| format!("running `{} list`", g.program))?;
    if !out.status.success() {
        anyhow::bail!(
            "`gilacap list` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_list_names(&String::from_utf8_lossy(&out.stdout)))
}

/// Drive the interactive selection over `names`, prompting on `out` and reading
/// answers from `input`. Pure over the injected streams (no TTY), so the choice
/// logic is unit-tested. Default is CLI-only; agent exposure is opt-in.
fn select_manifest(
    names: &[String],
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> io::Result<Manifest> {
    writeln!(out, "Configure capabilities → ~/.gila/capabilities.toml")?;
    writeln!(out, "(default is CLI-only; agent tools are opt-in)\n")?;
    let mut caps = Vec::new();
    for name in names {
        write!(
            out,
            "{name}: expose [c]li / [a]gent / [b]oth / [o]ff? (cli) "
        )?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            break; // EOF — keep what we have
        }
        let expose = match line.trim().chars().next().map(|c| c.to_ascii_lowercase()) {
            Some('a') => Expose::Agent,
            Some('b') => Expose::Both,
            Some('o') => Expose::Off,
            _ => Expose::Cli,
        };
        let confined = if expose.is_agent() {
            write!(out, "  confine {name} when run as an agent tool? [y/N] ")?;
            out.flush()?;
            let mut c = String::new();
            input.read_line(&mut c)?;
            matches!(
                c.trim().chars().next().map(|c| c.to_ascii_lowercase()),
                Some('y')
            )
        } else {
            false
        };
        caps.push(CapabilityEntry {
            name: name.clone(),
            expose,
            confined,
            env: Vec::new(),
            tools: None,
            venv: None,
        });
    }
    Ok(Manifest {
        venv: None,
        capabilities: caps,
    })
}

/// Write `m` to `path`, creating `~/.gila` if it does not exist.
fn write_manifest(path: &Path, m: &Manifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, m.to_toml()?).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// `gila capabilities config` (aka `gila cap config`) — interactively choose
/// which installed capabilities load as agent tools (and which run confined),
/// then write `~/.gila/capabilities.toml`. The opt-in selector for the otherwise
/// CLI-only-by-default surface.
pub fn config() -> Result<()> {
    let g = gilacap_global();
    let names = installed_names(&g)?;
    if names.is_empty() {
        println!("no capabilities installed — nothing to configure.");
        println!("  install one into the caps venv, then re-run `gila cap config`.");
        return Ok(());
    }
    let stdin = io::stdin();
    let manifest = select_manifest(&names, &mut stdin.lock(), &mut io::stdout())?;
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("HOME is not set; cannot locate ~/.gila"))?;
    let path = Manifest::default_path(&home);
    write_manifest(&path, &manifest)?;
    println!("\nwrote {}", path.display());
    Ok(())
}

/// The `[[mcp_servers]]` entries for the manifest's **agent-exposed** caps
/// (`expose = agent | both`), each served by `gilacap mcp <name>` under the cap's
/// resolved venv. Pure: the gilacap resolver is injected, so the mapping is
/// unit-tested without touching the env.
fn agent_mcp_entries_with(
    m: &Manifest,
    resolve: impl Fn(&str) -> GilacapCmd,
) -> Vec<McpServerEntry> {
    m.agent_exposed()
        .map(|c| entry_for_with(&resolve(&c.name), &c.name))
        .collect()
}

/// The agent-exposed capability MCP entries for the current manifest + env — what
/// `gila code` mounts. Empty (the default) ⇒ nothing to compose.
#[must_use]
pub fn agent_mcp_entries() -> Vec<McpServerEntry> {
    agent_mcp_entries_with(&manifest(), gilacap_for)
}

/// Overlay capability MCP `entries` onto a resolved newt [`Config`] so `gila code`
/// mounts the opted-in capabilities as agent tools. An operator-declared server
/// of the same name **wins** (no clobber/duplicate) — the same precedence as
/// `compose_hotseat_config`. Pure (consumes + returns the config).
///
/// The mounted server is `gilacap mcp <name>`, admitted and spawned by newt
/// under the session's MCP leash. The manifest's `confined` flag separately
/// governs direct `gila capabilities run` invocations.
#[must_use]
pub fn compose_agent_mcp(mut base: Config, entries: Vec<McpServerEntry>) -> Config {
    for entry in entries {
        if !base.mcp_servers.iter().any(|s| s.name == entry.name) {
            base.mcp_servers.push(entry);
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gilacap() -> GilacapCmd {
        GilacapCmd {
            program: "/v/bin/gilacap".to_string(),
            base_args: Vec::new(),
        }
    }

    #[test]
    fn entry_serves_via_gilacap_mcp() {
        let e = entry_for_with(&gilacap(), "mogul");
        assert_eq!(e.transport, TransportKind::Stdio);
        assert_eq!(e.command.as_deref(), Some("/v/bin/gilacap"));
        assert_eq!(e.args, ["mcp", "mogul"]);
        assert!(e.is_valid());
        assert!(
            newt_core::mcp::admit(&e).is_ok(),
            "gila-owned entries must satisfy newt's spawn admission gate"
        );
    }

    #[test]
    fn entry_threads_interpreter_base_args() {
        let py = GilacapCmd {
            program: "python3".to_string(),
            base_args: vec!["-m".into(), "gilamonster_capability.console".into()],
        };
        let e = entry_for_with(&py, "confluence");
        assert_eq!(e.command.as_deref(), Some("python3"));
        assert_eq!(
            e.args,
            ["-m", "gilamonster_capability.console", "mcp", "confluence"]
        );
    }

    #[test]
    fn run_argv_appends_args_json_only_when_present() {
        assert_eq!(
            run_argv(&gilacap(), "confluence", "blog", None),
            ["run", "confluence", "blog"]
        );
        assert_eq!(
            run_argv(
                &gilacap(),
                "confluence",
                "blog",
                Some("{\"space\":\"~me\"}")
            ),
            ["run", "confluence", "blog", "--args", "{\"space\":\"~me\"}"]
        );
    }

    #[test]
    fn confined_caveats_scope_exec_to_program_and_writes_to_cwd_and_tmp() {
        let cav = confined_caveats("/v/bin/gilacap", Path::new("/work"));
        match &cav.exec {
            Scope::Only(set) => assert!(set.contains("/v/bin/gilacap")),
            Scope::All => panic!("exec must be bounded to the program"),
        }
        match &cav.fs_write {
            Scope::Only(set) => {
                assert!(set.contains("/work"), "cwd must be writable");
                let tmp = std::env::temp_dir().to_string_lossy().into_owned();
                assert!(set.contains(&tmp), "temp dir must be writable");
            }
            Scope::All => panic!("fs_write must be bounded (it is the L3-enforced axis)"),
        }
        // Non-enforced axes stay unrestricted (advisory).
        assert_eq!(cav.net, Scope::All);
        assert_eq!(cav.fs_read, Scope::All);
    }

    #[test]
    fn confined_caveats_mint_a_valid_tool_context() {
        // The host must be able to authorize its own spawn token through the gate.
        let cav = confined_caveats("/v/bin/gilacap", Path::new("/work"));
        let cx = Gate::new(0)
            .authorize(&CapRunTool, &cav)
            .expect("authorize");
        assert!(cx.check_exec("/v/bin/gilacap").is_ok());
        assert!(cx.check_exec("/usr/bin/rm").is_err());
    }

    #[test]
    fn env_allow_passes_manifest_vars_and_essentials_only() {
        let entry = CapabilityEntry {
            name: "confluence".to_string(),
            expose: crate::manifest::Expose::Both,
            confined: true,
            env: vec!["CONFLUENCE_TOKEN".to_string()],
            tools: None,
            venv: None,
        };
        let fake = |k: &str| match k {
            "CONFLUENCE_TOKEN" => Some("secret".to_string()),
            "HOME" => Some("/home/op".to_string()),
            "PATH" => Some("/usr/bin".to_string()),
            "SECRET_THAT_MUST_NOT_LEAK" => Some("nope".to_string()),
            _ => None,
        };
        let allow = env_allow_with(Some(&entry), &fake);
        assert!(allow.contains(&("CONFLUENCE_TOKEN".to_string(), "secret".to_string())));
        assert!(allow.contains(&("HOME".to_string(), "/home/op".to_string())));
        assert!(allow.contains(&("PATH".to_string(), "/usr/bin".to_string())));
        // Nothing ungranted leaks.
        assert!(!allow.iter().any(|(k, _)| k == "SECRET_THAT_MUST_NOT_LEAK"));
    }

    #[test]
    fn parse_list_names_takes_the_first_tab_field() {
        let out = "confluence\tFetch, publish, blog.\nmogul\tStoryboards.\n";
        assert_eq!(parse_list_names(out), ["confluence", "mogul"]);
        assert!(parse_list_names("").is_empty());
        assert!(parse_list_names("no capabilities installed.").len() == 1); // a stray line
    }

    #[test]
    fn select_manifest_reads_choices_and_defaults_to_cli() {
        let names = vec!["confluence".to_string(), "mogul".to_string()];
        // confluence → [b]oth, then confine [y]; mogul → blank (defaults to cli).
        let mut input = std::io::Cursor::new(&b"b\ny\n\n"[..]);
        let mut out = Vec::new();
        let m = select_manifest(&names, &mut input, &mut out).expect("select");

        let c = m.entry("confluence").unwrap();
        assert_eq!(c.expose, Expose::Both);
        assert!(c.confined);
        let mg = m.entry("mogul").unwrap();
        assert_eq!(mg.expose, Expose::Cli);
        assert!(!mg.confined);
        // The prompts reached the output stream.
        assert!(String::from_utf8_lossy(&out).contains("expose"));
    }

    #[test]
    fn select_manifest_does_not_ask_to_confine_a_cli_only_cap() {
        let names = vec!["mogul".to_string()];
        let mut input = std::io::Cursor::new(&b"c\n"[..]); // cli only
        let mut out = Vec::new();
        let m = select_manifest(&names, &mut input, &mut out).expect("select");
        assert_eq!(m.entry("mogul").unwrap().expose, Expose::Cli);
        // No confine prompt for a non-agent choice.
        assert!(!String::from_utf8_lossy(&out).contains("confine"));
    }

    #[test]
    fn write_manifest_creates_dir_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".gila").join("capabilities.toml");
        let m = Manifest {
            venv: None,
            capabilities: vec![CapabilityEntry {
                name: "confluence".into(),
                expose: Expose::Agent,
                confined: true,
                env: Vec::new(),
                tools: None,
                venv: None,
            }],
        };
        write_manifest(&path, &m).expect("write");
        assert!(path.exists());
        assert_eq!(Manifest::load(&path).unwrap(), m);
    }

    #[test]
    fn agent_mcp_entries_includes_only_agent_exposed_caps() {
        let m = Manifest::parse(
            "[[capabilities]]\nname = \"confluence\"\nexpose = \"both\"\n\
             [[capabilities]]\nname = \"mogul\"\nexpose = \"cli\"\n\
             [[capabilities]]\nname = \"gl\"\nexpose = \"agent\"\n",
        )
        .unwrap();
        let entries = agent_mcp_entries_with(&m, |_| gilacap());
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["confluence", "gl"]); // mogul (cli-only) excluded
        assert_eq!(entries[0].args, ["mcp", "confluence"]); // served via gilacap mcp
    }

    #[test]
    fn compose_agent_mcp_adds_entries_and_respects_operator_precedence() {
        let mut base = Config::default();
        // The operator already declared their own `confluence` server.
        base.mcp_servers.push(McpServerEntry {
            name: "confluence".to_string(),
            enabled: true,
            transport: TransportKind::Stdio,
            command: Some("/opt/my-confluence".to_string()),
            args: Vec::new(),
            env: Default::default(),
            url: None,
            headers: Default::default(),
            request_timeout_secs: None,
            trust: McpTrust::Trusted,
        });
        let entries = vec![
            entry_for_with(&gilacap(), "confluence"),
            entry_for_with(&gilacap(), "gl"),
        ];
        let composed = compose_agent_mcp(base, entries);

        let confluence: Vec<_> = composed
            .mcp_servers
            .iter()
            .filter(|s| s.name == "confluence")
            .collect();
        assert_eq!(confluence.len(), 1, "operator entry must not be duplicated");
        assert_eq!(confluence[0].command.as_deref(), Some("/opt/my-confluence")); // theirs wins
        assert!(composed.mcp_servers.iter().any(|s| s.name == "gl")); // gl added

        // Round-trips through newt's own TOML loader (what gila writes is what newt resolves).
        let toml = toml::to_string(&composed).unwrap();
        let reloaded: Config = toml::from_str(&toml).unwrap();
        assert!(reloaded.mcp_servers.iter().any(|s| s.name == "gl"));
    }
}
