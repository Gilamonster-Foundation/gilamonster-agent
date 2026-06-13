//! `gila hotseat` — the on-call / triage **cockpit** (issue #11, hotseat half).
//!
//! # What this is
//!
//! Hotseat is the operator's on-call mode: a triage assistant that can **search
//! incident-management, issue-tracker, and knowledge-base surfaces** and read
//! runbook guidance, under a **read-only / ack-only** permission posture. It is
//! the composition of three already-built pieces — gila ships only the *wiring*,
//! not any new engine:
//!
//! 1. **A read-only authority FLOOR** — newt's named permission presets (#307):
//!    a [`readonly_triage_preset`] is applied via newt's `/mode` command as a
//!    [`NamedPermissionPreset`](newt_core::NamedPermissionPreset) clamp. The
//!    preset is a *ceiling*, `meet`-ed into the session authority, so it can only
//!    ever attenuate — `--yolo`/`--disable-ocap` and interactive session-grants
//!    cannot raise it (#312). The triage agent searches, reads, and acknowledges;
//!    it never mutates.
//! 2. **A preloaded triage skill** — newt's `/mode` `skill` field loads a named
//!    triage/runbook skill body through the very same `use_skill` /
//!    `newt_skills::load_body_from` path. gila ships the wiring to name and load
//!    a triage skill; the SKILL CONTENT is operator config (a `SKILL.md` folder
//!    on the operator's skill search path), not gila code.
//! 3. **Authenticated MCP search via the modulex proxy** — the session is wired
//!    so newt discovers the **modulex stdio MCP server** as a tool surface. The
//!    agent searches the incident / issue / wiki surfaces *through* modulex,
//!    which proxies the authenticated downstream HTTP MCP servers and holds the
//!    credentials by reference. **newt only ever sees stdio and never touches a
//!    credential.**
//!
//! # The hard boundary: generic mechanism only
//!
//! This module contains **zero** enterprise-internal names, URLs, or credential
//! references — by construction. It wires modulex as *the* MCP search surface
//! generically (`command = "modulex-mcp"`, a stdio server); *which* downstream
//! servers modulex proxies and *what* credentials they need live ONLY in the
//! operator's private `~/.modulex/config.toml`, never here. The triage skill
//! *name* defaults to a generic [`DEFAULT_TRIAGE_SKILL`]; the skill *body* is
//! operator config. This is the public-discipline rule the whole gila/newt/
//! modulex stack follows.
//!
//! # How it composes (the testable seam)
//!
//! Every decision is a pure function over config so the gate can exercise it
//! without launching the TUI:
//!
//! - [`readonly_triage_preset`] — the [`NamedPermissionPreset`] clamp (read-only
//!   floor: writes + exec denied, net denied, a tool-call ceiling).
//! - [`modulex_mcp_entry`] — the stdio [`McpServerEntry`] for the modulex proxy.
//! - [`hotseat_mode`] — the [`ModeConfig`] binding skill + preset + framing that
//!   `/mode hotseat` applies atomically.
//! - [`compose_hotseat_config`] — overlay all three onto the operator's resolved
//!   newt [`Config`], leaving every other field untouched.
//! - [`triage_skill_name`] — resolve the triage skill name (CLI/env override or
//!   the generic default).
//!
//! The `gila hotseat` binary arm (`src/main.rs`) resolves the operator's config,
//! calls [`compose_hotseat_config`], writes the composed config to a session
//! file, points `$NEWT_CONFIG` at it, and hands off to the inherited newt TUI —
//! where the operator engages the floor with `/mode hotseat`. That last hand-off
//! (real-tty TUI launch + the filesystem write) is the only by-design-uncovered
//! surface, mirroring the carve-out `gila follow` / `gila cowork` use.

use newt_core::config::ModeConfig;
use newt_core::mcp::{McpServerEntry, TransportKind};
use newt_core::{Config, NamedPermissionPreset};

/// The name of the hotseat mode (`[modes.hotseat]`) the operator engages with
/// `/mode hotseat`. Also the key gila writes into the composed config.
pub const HOTSEAT_MODE_NAME: &str = "hotseat";

/// The name of the read-only triage permission preset
/// (`[permission_presets.readonly-triage]`) the hotseat mode clamps to.
pub const HOTSEAT_PRESET_NAME: &str = "readonly-triage";

/// The MCP server name (`[[mcp_servers]].name`) under which the modulex stdio
/// proxy is wired as the authenticated search surface. A generic name — it
/// refers to the *proxy*, not to any enterprise system.
pub const MODULEX_MCP_NAME: &str = "modulex";

/// The stdio executable that speaks the modulex MCP server. This is the generic
/// proxy binary (`hartsock/modulex-mcp`); it holds the enterprise creds by
/// reference in the operator's private `~/.modulex/config.toml` and proxies the
/// authenticated downstream HTTP MCPs. gila names only the binary — never a
/// downstream server or credential.
pub const MODULEX_MCP_COMMAND: &str = "modulex-mcp";

/// The default triage skill name the hotseat mode preloads. Generic on purpose:
/// the operator supplies the matching `SKILL.md` body on their skill search
/// path (`[skills].search` in newt config). Override per-invocation with
/// `gila hotseat --skill <name>` or the `GILA_HOTSEAT_SKILL` env var.
pub const DEFAULT_TRIAGE_SKILL: &str = "oncall-triage";

/// The env var that overrides the triage skill name without a CLI flag.
pub const TRIAGE_SKILL_ENV: &str = "GILA_HOTSEAT_SKILL";

/// The one-line system-prompt framing injected when hotseat mode is active.
/// Generic — describes the *posture* (on-call triage, read-only, search the
/// incident/issue/wiki surfaces via the proxy), never an enterprise system.
pub const HOTSEAT_FRAMING: &str = "On-call hotseat: triage incidents read-only. \
     Search the incident, issue-tracker, and knowledge-base surfaces through the \
     authenticated MCP proxy, read runbooks, and acknowledge — investigate and \
     report, never change production.";

/// The tool-call ceiling the triage preset imposes. A triage turn does real
/// searching (several MCP calls) but should not run unbounded; a generous-but-
/// finite ceiling keeps a runaway loop from hammering the search surfaces while
/// leaving ample room for legitimate investigation.
pub const HOTSEAT_MAX_CALLS: u64 = 40;

/// Build the **read-only triage** permission preset — the authority FLOOR the
/// hotseat mode clamps to (issue #307).
///
/// The clamp is a *ceiling*: `readonly = true` denies all filesystem writes and
/// all command execution; `deny = ["*"]` clamps the network axis to none; and a
/// [`HOTSEAT_MAX_CALLS`] tool-call ceiling bounds a turn. Filesystem **reads**
/// stay permitted so the agent can orient itself from runbooks and context, and
/// the MCP search surface (modulex over stdio) is reached through tool calls,
/// not a clamped axis — so search still works while mutation is impossible.
///
/// Because the session's effective authority is `base.meet(&preset.clamp())`,
/// this can only ever **attenuate**: it wins over `--yolo` / `--disable-ocap`
/// and over interactive session-grants (#312). That is the load-bearing safety
/// property of the cockpit — the floor holds no matter how the session was
/// launched.
#[must_use]
pub fn readonly_triage_preset() -> NamedPermissionPreset {
    NamedPermissionPreset {
        // Deny all writes; (with no exec_allow) deny all exec.
        readonly: true,
        // A read-only triage agent runs no local commands at all — search and
        // acknowledge happen through the MCP surface, not the shell.
        exec_allow: Vec::new(),
        // Clamp the remaining axis (net) to none: the agent reaches the search
        // surface through the modulex MCP tool, not by dialing hosts itself.
        deny: vec!["*".to_string()],
        // A finite tool-call ceiling per turn.
        max_calls: Some(HOTSEAT_MAX_CALLS),
    }
}

/// Build the stdio [`McpServerEntry`] for the **modulex proxy** — the
/// authenticated MCP search surface, wired generically.
///
/// This is the whole of gila's MCP wiring: point newt at the modulex stdio
/// server by binary name. modulex proxies the authenticated downstream HTTP MCP
/// servers (incident management, issue tracker, knowledge base) and holds their
/// credentials by reference in the operator's private `~/.modulex/config.toml`,
/// so **newt sees only stdio and never a credential**. No URL, no header, no
/// downstream server name appears here — those are modulex's private config.
#[must_use]
pub fn modulex_mcp_entry() -> McpServerEntry {
    McpServerEntry {
        name: MODULEX_MCP_NAME.to_string(),
        transport: TransportKind::Stdio,
        command: Some(MODULEX_MCP_COMMAND.to_string()),
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
    }
}

/// Build the hotseat [`ModeConfig`] — the atomic binding `/mode hotseat`
/// applies: preload the `skill` triage body, clamp to the [`HOTSEAT_PRESET_NAME`]
/// preset (the read-only floor), and inject the [`HOTSEAT_FRAMING`] system-prompt
/// line. All three apply together or not at all (newt's #307 guarantee), so the
/// cockpit can never half-engage (a clamp without its skill, or vice versa).
#[must_use]
pub fn hotseat_mode(skill: &str) -> ModeConfig {
    ModeConfig {
        skill: Some(skill.to_string()),
        preset: Some(HOTSEAT_PRESET_NAME.to_string()),
        framing: Some(HOTSEAT_FRAMING.to_string()),
    }
}

/// Resolve the triage skill name to preload.
///
/// Precedence: an explicit `override_name` (the `gila hotseat --skill <name>`
/// flag) wins; otherwise the [`TRIAGE_SKILL_ENV`] env var; otherwise the generic
/// [`DEFAULT_TRIAGE_SKILL`]. A blank override/env value is ignored (falls
/// through), so an empty flag never names an empty skill. Pure: the env lookup
/// is injected so it is unit-testable without touching the process environment.
#[must_use]
pub fn triage_skill_name_with(
    override_name: Option<&str>,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> String {
    if let Some(name) = override_name {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    if let Some(val) = env_lookup(TRIAGE_SKILL_ENV) {
        let val = val.trim();
        if !val.is_empty() {
            return val.to_string();
        }
    }
    DEFAULT_TRIAGE_SKILL.to_string()
}

/// Resolve the triage skill name against the real process environment — the
/// thin wrapper the binary calls. The pure core is [`triage_skill_name_with`].
#[must_use]
pub fn triage_skill_name(override_name: Option<&str>) -> String {
    triage_skill_name_with(override_name, |k| std::env::var(k).ok())
}

/// Overlay the hotseat composition onto a resolved newt [`Config`].
///
/// Takes the operator's existing config (their inference backends, skill search
/// path, any MCP servers they already configured) and adds — without disturbing
/// anything else — the three hotseat pieces:
///
/// - the [`HOTSEAT_PRESET_NAME`] permission preset ([`readonly_triage_preset`]),
/// - the [`HOTSEAT_MODE_NAME`] mode ([`hotseat_mode`]) bound to `skill`,
/// - the modulex stdio MCP server ([`modulex_mcp_entry`]), **unless** the
///   operator already declared a `[[mcp_servers]]` of that name (their entry
///   wins — they may point `modulex` at a wrapper or a different path).
///
/// The composed config is what gila writes to a session file and points
/// `$NEWT_CONFIG` at, so the inherited TUI resolves the hotseat mode/preset/MCP
/// alongside everything the operator already had. Returns the mutated config so
/// the binary can serialize it; pure apart from consuming and returning the
/// value.
#[must_use]
pub fn compose_hotseat_config(mut base: Config, skill: &str) -> Config {
    base.permission_presets
        .insert(HOTSEAT_PRESET_NAME.to_string(), readonly_triage_preset());
    base.modes
        .insert(HOTSEAT_MODE_NAME.to_string(), hotseat_mode(skill));
    // Only add the modulex entry if the operator hasn't declared one already —
    // their own `[[mcp_servers]]` named `modulex` takes precedence (they might
    // wrap it or pin a path), exactly as newt's own discovery precedence works.
    if !base.mcp_servers.iter().any(|s| s.name == MODULEX_MCP_NAME) {
        base.mcp_servers.push(modulex_mcp_entry());
    }
    base
}

/// The notice `gila hotseat` prints before handing off to the TUI, telling the
/// operator the cockpit is wired and how to engage the read-only floor. Pure
/// (returns the string) so the wording — including the read-only contract and
/// the `/mode hotseat` engage step — is unit-tested. States the posture plainly
/// so the operator knows the agent is a triager, never an actor.
#[must_use]
pub fn hotseat_notice(skill: &str) -> String {
    format!(
        "gila hotseat (on-call triage): wired the read-only floor (preset \
         '{HOTSEAT_PRESET_NAME}'), the '{skill}' triage skill, and authenticated \
         MCP search via the modulex proxy (stdio).\n\
         Engage the read-only clamp with `/mode {HOTSEAT_MODE_NAME}` once the TUI \
         is up: it preloads the runbook skill and applies the authority floor \
         (writes, exec, and direct network are denied — search and acknowledge \
         only). The floor holds over --yolo and session grants. The enterprise \
         search targets + credentials live in your private modulex config, never \
         in gila.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::{Caveats, CaveatsExt};

    // --- the read-only floor (ties to #307's clamp) -------------------------

    /// THE load-bearing security test: the hotseat preset's clamp denies writes,
    /// exec, and network, while leaving reads (orientation) permitted — and as a
    /// `meet` ceiling it can only attenuate, so an unrestricted session base is
    /// clamped to the floor (it wins over `--yolo` / session grants, #312).
    #[test]
    fn readonly_triage_preset_clamps_to_a_read_only_floor() {
        let preset = readonly_triage_preset();
        let clamp = preset.clamp();

        // Reads stay permitted; everything that mutates or reaches the world is
        // denied at the clamp itself.
        assert!(clamp.permits_fs_read("/runbooks/incident.md"));
        assert!(!clamp.permits_fs_write("/etc/anything"));
        assert!(!clamp.permits_exec("rm"));
        assert!(!clamp.permits_exec("kubectl"));
        assert!(!clamp.permits_net("any.host"));

        // The ceiling property: even a fully-unrestricted base is clamped down to
        // the read-only floor by `meet` — the floor wins regardless of how the
        // session was launched (the #307/#312 guarantee gila relies on).
        let wide_open = Caveats::top();
        let effective = wide_open.meet(&clamp);
        assert!(effective.permits_fs_read("/x"));
        assert!(!effective.permits_fs_write("/x"));
        assert!(!effective.permits_exec("sh"));
        assert!(!effective.permits_net("evil.example"));
    }

    #[test]
    fn readonly_triage_preset_carries_a_tool_call_ceiling() {
        let preset = readonly_triage_preset();
        assert_eq!(preset.max_calls, Some(HOTSEAT_MAX_CALLS));
        // The clamp summary reflects the read-only floor for `/permissions`.
        let summary = preset.summary();
        assert!(summary.contains("readonly"), "{summary}");
        assert!(summary.contains("deny=*"), "{summary}");
        assert!(
            summary.contains(&HOTSEAT_MAX_CALLS.to_string()),
            "{summary}"
        );
    }

    // --- the modulex MCP search surface (wired generically) -----------------

    #[test]
    fn modulex_mcp_entry_is_a_valid_stdio_server() {
        let entry = modulex_mcp_entry();
        assert_eq!(entry.name, MODULEX_MCP_NAME);
        assert_eq!(entry.transport, TransportKind::Stdio);
        assert_eq!(entry.command.as_deref(), Some(MODULEX_MCP_COMMAND));
        // A stdio proxy: no URL, no headers, no env — newt sees only stdio.
        assert!(entry.url.is_none());
        assert!(entry.headers.is_empty());
        assert!(entry.env.is_empty());
        // It is a connectable entry by newt's own validity rule.
        assert!(entry.is_valid());
    }

    // --- the hotseat mode binding (preset + skill + framing) ----------------

    #[test]
    fn hotseat_mode_binds_preset_skill_and_framing() {
        let mode = hotseat_mode("oncall-triage");
        assert_eq!(mode.preset.as_deref(), Some(HOTSEAT_PRESET_NAME));
        assert_eq!(mode.skill.as_deref(), Some("oncall-triage"));
        let framing = mode.framing.expect("framing present");
        // Generic posture wording only.
        assert!(framing.contains("triage"));
        assert!(framing.contains("read-only"));
        assert!(framing.to_lowercase().contains("never change production"));
    }

    // --- skill-name resolution ----------------------------------------------

    #[test]
    fn triage_skill_name_defaults_when_no_override_or_env() {
        let got = triage_skill_name_with(None, |_| None);
        assert_eq!(got, DEFAULT_TRIAGE_SKILL);
    }

    #[test]
    fn triage_skill_name_prefers_explicit_override() {
        let got = triage_skill_name_with(Some("custom-runbook"), |_| Some("env-skill".to_string()));
        assert_eq!(got, "custom-runbook");
    }

    #[test]
    fn triage_skill_name_falls_back_to_env_when_no_override() {
        let got = triage_skill_name_with(None, |k| {
            if k == TRIAGE_SKILL_ENV {
                Some("env-skill".to_string())
            } else {
                None
            }
        });
        assert_eq!(got, "env-skill");
    }

    #[test]
    fn triage_skill_name_ignores_blank_override_and_env() {
        // Blank override → blank env → default.
        let got = triage_skill_name_with(Some("   "), |_| Some("  ".to_string()));
        assert_eq!(got, DEFAULT_TRIAGE_SKILL);
        // Blank override but a real env value → env wins.
        let got2 = triage_skill_name_with(Some(""), |k| {
            if k == TRIAGE_SKILL_ENV {
                Some("real".to_string())
            } else {
                None
            }
        });
        assert_eq!(got2, "real");
    }

    #[test]
    fn triage_skill_name_reads_the_real_env_wrapper() {
        // The wrapper path: with the env unset, the default is returned. (We do
        // not mutate the process env here — the pure core covers the override/env
        // branches; this just exercises the wrapper line.)
        let got = triage_skill_name(Some("explicit"));
        assert_eq!(got, "explicit");
    }

    // --- compose_hotseat_config (the overlay) -------------------------------

    #[test]
    fn compose_adds_preset_mode_and_modulex_without_disturbing_base() {
        let base = Config::default();
        // `BackendConfig` is not `PartialEq`; capture a stable projection instead.
        let backends_before: Vec<(String, String)> = base
            .backends
            .iter()
            .map(|b| (b.name.clone(), b.endpoint.clone()))
            .collect();
        let composed = compose_hotseat_config(base, "oncall-triage");

        // The preset is present and is the read-only floor.
        let preset = composed
            .permission_presets
            .get(HOTSEAT_PRESET_NAME)
            .expect("preset added");
        assert!(preset.readonly);
        assert!(preset.clamp().permits_fs_read("/x"));
        assert!(!preset.clamp().permits_fs_write("/x"));

        // The mode binds skill + preset + framing.
        let mode = composed.modes.get(HOTSEAT_MODE_NAME).expect("mode added");
        assert_eq!(mode.preset.as_deref(), Some(HOTSEAT_PRESET_NAME));
        assert_eq!(mode.skill.as_deref(), Some("oncall-triage"));
        assert!(mode.framing.is_some());

        // The modulex stdio search surface is wired.
        let modulex = composed
            .mcp_servers
            .iter()
            .find(|s| s.name == MODULEX_MCP_NAME)
            .expect("modulex MCP entry added");
        assert_eq!(modulex.command.as_deref(), Some(MODULEX_MCP_COMMAND));
        assert_eq!(modulex.transport, TransportKind::Stdio);

        // Nothing else was disturbed (the operator's backends carry through).
        let backends_after: Vec<(String, String)> = composed
            .backends
            .iter()
            .map(|b| (b.name.clone(), b.endpoint.clone()))
            .collect();
        assert_eq!(backends_after, backends_before);
    }

    #[test]
    fn compose_does_not_duplicate_an_operator_modulex_entry() {
        // An operator who already declared a `modulex` server (e.g. pinned to an
        // absolute path) keeps THEIR entry — gila does not clobber or duplicate.
        let mut base = Config::default();
        base.mcp_servers.push(McpServerEntry {
            name: MODULEX_MCP_NAME.to_string(),
            transport: TransportKind::Stdio,
            command: Some("/opt/bin/modulex-mcp".to_string()),
            args: vec!["--config".to_string(), "/etc/modulex.toml".to_string()],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
        });
        let composed = compose_hotseat_config(base, "oncall-triage");

        let modulex: Vec<_> = composed
            .mcp_servers
            .iter()
            .filter(|s| s.name == MODULEX_MCP_NAME)
            .collect();
        assert_eq!(modulex.len(), 1, "operator entry must not be duplicated");
        // Their command wins, not gila's bare default.
        assert_eq!(modulex[0].command.as_deref(), Some("/opt/bin/modulex-mcp"));
    }

    /// The composed config round-trips through newt's own TOML loader — proving
    /// what gila writes to the session file is exactly what newt will resolve
    /// (the preset, the mode, and the modulex MCP entry all survive).
    #[test]
    fn composed_config_round_trips_through_newt_toml() {
        let composed = compose_hotseat_config(Config::default(), "oncall-triage");
        let toml = toml::to_string(&composed).expect("serialize");
        let reloaded: Config = toml::from_str(&toml).expect("newt parses it back");

        assert!(reloaded
            .permission_presets
            .contains_key(HOTSEAT_PRESET_NAME));
        assert!(reloaded.modes.contains_key(HOTSEAT_MODE_NAME));
        assert!(reloaded.mcp_servers.iter().any(
            |s| s.name == MODULEX_MCP_NAME && s.command.as_deref() == Some(MODULEX_MCP_COMMAND)
        ));
    }

    // --- NVIDIA-clean by construction ---------------------------------------

    /// THE public-discipline test: everything **gila contributes** to the hotseat
    /// composition — the preset, the mode, the modulex MCP entry, the framing, the
    /// notice — contains NO enterprise-internal names, URLs, hostnames, or
    /// credential references. Only generic mechanism terms appear.
    ///
    /// The haystack is gila's contribution ONLY: we compose onto a base with the
    /// operator's backends cleared, so the operator's own (legitimate) inference
    /// endpoint URL is not conflated with gila's added wiring. If any enterprise
    /// specific ever leaks into what gila ADDS, this fails.
    #[test]
    fn composed_config_contains_no_enterprise_specifics() {
        // Clear the operator's pre-existing backends so the serialized config is
        // purely gila's contribution (preset + mode + modulex entry). The
        // operator's own inference endpoint is THEIRS, not a gila leak.
        let mut base = Config::default();
        base.backends.clear();
        let composed = compose_hotseat_config(base, DEFAULT_TRIAGE_SKILL);
        let toml = toml::to_string(&composed).expect("serialize");
        let notice = hotseat_notice(DEFAULT_TRIAGE_SKILL);
        let haystack = format!("{toml}\n{notice}").to_lowercase();

        // No URLs / endpoints of any kind in what gila adds (modulex is stdio;
        // the downstream HTTP servers live in modulex's PRIVATE config).
        assert!(
            !haystack.contains("http://") && !haystack.contains("https://"),
            "a URL leaked into gila's hotseat composition: {haystack}"
        );
        // No credential/secret references.
        for token in [
            "bearer",
            "authorization",
            "api_key",
            "apikey",
            "token=",
            "secret",
            "password",
            "oauth",
            "cookie",
        ] {
            assert!(
                !haystack.contains(token),
                "credential reference '{token}' leaked: {haystack}"
            );
        }
        // No enterprise / vendor proper nouns. (A representative scrub list — the
        // built config is generic mechanism only; these must never appear.)
        for token in [
            "nvidia",
            "nvda",
            "eci",
            "jira",
            "servicenow",
            "pagerduty",
            "confluence",
            "splunk",
            ".com/",
            "internal",
            "corp",
        ] {
            assert!(
                !haystack.contains(token),
                "enterprise-specific token '{token}' leaked: {haystack}"
            );
        }
        // What SHOULD be there: the generic proxy mechanism.
        assert!(haystack.contains("modulex"));
        assert!(haystack.contains("modulex-mcp"));
    }

    #[test]
    fn hotseat_notice_states_the_read_only_contract_and_engage_step() {
        let notice = hotseat_notice("oncall-triage");
        assert!(notice.contains("read-only"));
        assert!(notice.contains(&format!("/mode {HOTSEAT_MODE_NAME}")));
        assert!(notice.contains("oncall-triage"));
        assert!(notice.contains("modulex"));
        // Names the floor and that it holds over yolo/grants.
        assert!(notice.contains("--yolo"));
        // The private-config boundary is stated to the operator.
        assert!(notice.to_lowercase().contains("private modulex config"));
    }
}
