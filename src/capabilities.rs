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

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use newt_core::mcp::{McpServerEntry, TransportKind};
use newt_mcp_client::{McpConnection, StdioTransport};
use serde_json::json;

use crate::manifest::Manifest;
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
        transport: TransportKind::Stdio,
        command: Some(g.program.clone()),
        args: g.argv(&["mcp", name]),
        env: Default::default(),
        url: None,
        headers: Default::default(),
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

/// `gila capabilities run <name> <tool> [--args '{…}']` — invoke one capability
/// tool through the `gilacap` multiplexer and stream its result.
///
/// Confinement seam: today this spawns `gilacap` directly — for a human at the
/// prompt that is correct (they run under their own authority). Once
/// agent-bridle#55 lands (and gila's `[patch.crates-io]` agent-bridle rev is
/// bumped past it), the *agent-exposed* path will mint per-capability caveats via
/// `newt_identity::delegate_for_plugin` and spawn through
/// `agent_bridle_core::spawn_confined_subprocess` instead of this bare spawn.
pub fn run(name: &str, tool: &str, args: Option<String>) -> Result<()> {
    let g = gilacap_for(name);
    let argv = run_argv(&g, name, tool, args.as_deref());
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
    let transport = StdioTransport::spawn(&entry).with_context(|| {
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
}
