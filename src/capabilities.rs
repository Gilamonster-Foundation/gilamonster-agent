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

use anyhow::{Context, Result};
use newt_core::mcp::{McpServerEntry, TransportKind};
use newt_mcp_client::{McpConnection, StdioTransport};
use serde_json::json;

/// The MCP console-script command for a capability (the packaging convention).
fn mcp_command(name: &str) -> String {
    format!("gila-cap-{name}-mcp")
}

/// The stdio [`McpServerEntry`] for a capability's MCP server.
fn entry_for(name: &str) -> McpServerEntry {
    McpServerEntry {
        name: name.to_string(),
        transport: TransportKind::Stdio,
        command: Some(mcp_command(name)),
        args: Vec::new(),
        env: Default::default(),
        url: None,
        headers: Default::default(),
    }
}

/// `gila capabilities list` — enumerate installed `gilamonster.capabilities`.
///
/// Shells to `python3` to read the entry-point group (the authoritative registry
/// a `pip install` populates). Empty/absent Python → a friendly hint.
pub fn list() -> Result<()> {
    const PY: &str = r#"import json
from importlib.metadata import entry_points
eps = entry_points()
sel = eps.select(group="gilamonster.capabilities") if hasattr(eps, "select") else eps.get("gilamonster.capabilities", [])
print(json.dumps(sorted(e.name for e in sel)))"#;

    let out = std::process::Command::new("python3")
        .arg("-c")
        .arg(PY)
        .output()
        .context("running python3 to enumerate the gilamonster.capabilities entry points")?;
    if !out.status.success() {
        anyhow::bail!(
            "python3 capability discovery failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let names: Vec<String> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    if names.is_empty() {
        println!("no capabilities installed.");
        println!("  install one, e.g.: pip install gila-cap-mogul");
    } else {
        println!("installed capabilities:");
        for n in &names {
            println!("  - {:<16} (mcp: {})", n, mcp_command(n));
        }
        println!("\ncheck one:  gila capabilities check <name>");
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
    let entry = entry_for(name);
    println!(
        "→ spawning `{}` and connecting via newt's MCP client …",
        mcp_command(name)
    );
    let transport = StdioTransport::spawn(&entry).with_context(|| {
        format!(
            "spawning `{}` — is it on PATH? (try `pip install gila-cap-{}`)",
            mcp_command(name),
            name
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

/// `gila capabilities enable <name>` — print the newt config snippet that wires
/// the capability's MCP server into every gila session (its tools then arrive
/// namespaced `<name>__<tool>` through newt's MCP client).
pub fn enable(name: &str) -> Result<()> {
    println!("Add this to ~/.newt/config.toml to wire '{name}' into gila sessions:\n");
    println!("[[mcp_servers]]");
    println!("name = \"{name}\"");
    println!("command = \"{}\"", mcp_command(name));
    println!("\nThen `gila code` exposes its tools (namespaced `{name}__<tool>`).");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_command_follows_the_packaging_convention() {
        assert_eq!(mcp_command("mogul"), "gila-cap-mogul-mcp");
    }

    #[test]
    fn entry_is_a_valid_stdio_server() {
        let e = entry_for("mogul");
        assert_eq!(e.transport, TransportKind::Stdio);
        assert_eq!(e.command.as_deref(), Some("gila-cap-mogul-mcp"));
        assert!(e.is_valid());
    }
}
