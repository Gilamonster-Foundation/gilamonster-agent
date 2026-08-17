//! The cockpit's authority spine (#54 — cockpit design phase 4).
//!
//! Every chat pane that carries a [`TurnDriver`](newt_core::agentic::TurnDriver)
//! gets its caveats from exactly one place — [`caveats_for`] — and every driver
//! config is minted by exactly one function — [`driver_config`]. This is the
//! design's load-bearing rule: **no unclamped driver can exist**, because there
//! is no other way to build one.
//!
//! This module is the authority *spine* — the caveat lattice and the sole
//! minting function. Making it the literal only `TurnDriverConfig::new` call
//! site (routing the two legacy callers — `cowork`'s full-authority driver in
//! `main.rs` and `follow.rs`'s read-only builder — through it, guarded by a
//! repo-grep test) lands with `run_cockpit` in the next v0.3.4 ratchet, so the
//! first per-tab driver and the sole-site guarantee arrive together with no
//! window of unclamped drivers. Today the companion clamp is already provably
//! identical to `follow`'s observer floor (see the tests).
//!
//! # Pane kinds and their posture
//!
//! Only *chat* panes have a driver. The ambient shell, the Jupyter status pane,
//! and the fleet tab have **no driver at all** — they are not representable as a
//! [`PaneKind`] here, so this module cannot mint authority for them.
//!
//! | [`PaneKind`] | Posture |
//! |---|---|
//! | [`Companion`](PaneKind::Companion) | `fs_read=All`, everything world-touching denied, `max_calls=0`. The zero-tool default; safe for unprompted commentary *by construction* (it can speak but cannot act). |
//! | [`Reader`](PaneKind::Reader) | Read + a bounded call budget; `fs_write`/`exec`/`net` denied. The middle posture. |
//! | [`Workbench`](PaneKind::Workbench) | Read + a call budget; **`exec`/`net`/`fs_write` still denied** — held at the floor until the upstream permission gate + grant modal land (v0.3.6). Lattice-deny, not policy-deny. |
//!
//! # The `NEWT_DISABLE_OCAP` refusal (ADR-bound)
//!
//! The phase-0 verification of newt `agentic/tools.rs`
//! (`docs/decisions/cockpit_tmux_multiplexer.md`) found that
//! `NEWT_DISABLE_OCAP` makes `run_command` bypass the caveat-confined shell —
//! `exec = Scope::none()` is never consulted. A cockpit driver's observe-only
//! and default-deny guarantees therefore **do not hold** in that environment.
//! So [`driver_config`] refuses to build any config while that variable is set,
//! failing loud instead of minting a driver whose lattice is a lie.
//!
//! MCP mounts are likewise **outside** this lattice at the pinned rev (tools
//! carry no caveat leash), which is why companion/reader/workbench are `NoMcp`
//! until the per-tab MCP proxy + grant modal (v0.3.5/v0.3.6) become the
//! authority ceremony for tools.

use newt_core::agentic::TurnDriverConfig;
use newt_core::{BackendKind, Caveats, CountBound, Scope};

/// The environment variable that disables newt's object-capability enforcement.
/// Newt accepts the exact value `1` and freezes it at startup; the cockpit
/// refuses to construct drivers when that frozen authority is active.
pub const DISABLE_OCAP_ENV: &str = "NEWT_DISABLE_OCAP";

/// A bounded, non-zero tool-call budget for the tool-capable postures. Small on
/// purpose: a driven turn that needs more is a signal for the grant modal, not
/// a bigger default.
const BOUNDED_CALLS: u64 = 16;

/// The authority posture of a chat pane. Only chat panes have a driver; panes
/// with no driver (ambient shell, jupyter-status, fleet) are deliberately not
/// representable here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    /// The zero-tool default. Reads to orient itself; cannot act; eligible for
    /// proactive commentary because "speaks unprompted" and "acts unprompted"
    /// are separated by construction.
    Companion,
    /// Read plus a bounded call budget; no world-touching authority. The middle
    /// posture between a pure commentator and a workbench.
    Reader,
    /// Tool-capable in intent, but held at the floor (`exec`/`net`/`fs_write`
    /// denied) until the permission gate + grant modal land. Created only via
    /// an explicit grant in v0.3.6; here it clamps like a reader.
    Workbench,
}

/// Why a driver config could not be minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityError {
    /// `NEWT_DISABLE_OCAP=1` was frozen at launch: the caveat lattice would not
    /// actually bind a driven turn, so we refuse to build one.
    OcapDisabled,
}

impl std::fmt::Display for AuthorityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthorityError::OcapDisabled => write!(
                f,
                "refusing to build a cockpit driver while {DISABLE_OCAP_ENV} is set — \
                 the shell tool bypasses the caveat lattice, so a pane's observe-only / \
                 default-deny posture would not actually bind. Unset {DISABLE_OCAP_ENV}."
            ),
        }
    }
}

impl std::error::Error for AuthorityError {}

/// The caveat clamp for a pane kind — the **only** place a pane's authority is
/// decided. Built on [`Caveats::top`] and narrowed, so any capability axis
/// added upstream defaults to permissive and must be *deliberately* clamped
/// here to grant less; a new axis never silently grants a pane authority.
#[must_use]
pub fn caveats_for(kind: PaneKind) -> Caveats {
    match kind {
        // Identical to `follow`'s observer floor: read to orient, act never.
        PaneKind::Companion => Caveats {
            fs_read: Scope::All,
            fs_write: Scope::none(),
            exec: Scope::none(),
            net: Scope::none(),
            max_calls: CountBound::AtMost(0),
            ..Caveats::top()
        },
        // Read plus a bounded budget; still no world-touching authority.
        PaneKind::Reader => Caveats {
            fs_read: Scope::All,
            fs_write: Scope::none(),
            exec: Scope::none(),
            net: Scope::none(),
            max_calls: CountBound::AtMost(BOUNDED_CALLS),
            ..Caveats::top()
        },
        // Workbench: intent is tool-capable, but exec/net/fs_write stay at the
        // floor until the grant modal + permission gate (v0.3.6). Same clamp as
        // Reader for now — lattice-deny, so the authority never exists early.
        PaneKind::Workbench => Caveats {
            fs_read: Scope::All,
            fs_write: Scope::none(),
            exec: Scope::none(),
            net: Scope::none(),
            max_calls: CountBound::AtMost(BOUNDED_CALLS),
            ..Caveats::top()
        },
    }
}

/// Build the [`TurnDriverConfig`] for a pane — the **sole** construction site
/// in the crate. Stamps the pane's [`caveats_for`] clamp onto a fresh config.
///
/// # Errors
/// [`AuthorityError::OcapDisabled`] when [`DISABLE_OCAP_ENV`] was frozen active
/// at launch — see the module docs. Callers must surface this (fail-loud), never
/// fall back to an unclamped driver.
pub fn driver_config(
    kind: PaneKind,
    url: impl Into<String>,
    model: impl Into<String>,
    backend_kind: BackendKind,
    workspace: impl Into<String>,
) -> Result<TurnDriverConfig, AuthorityError> {
    driver_config_inner(ocap_disabled(), kind, url, model, backend_kind, workspace)
}

/// The pure core of [`driver_config`] with the OCAP-disabled state injected, so
/// the refusal logic is unit-tested without mutating the process-global
/// environment (which would race across parallel tests).
fn driver_config_inner(
    ocap_disabled: bool,
    kind: PaneKind,
    url: impl Into<String>,
    model: impl Into<String>,
    backend_kind: BackendKind,
    workspace: impl Into<String>,
) -> Result<TurnDriverConfig, AuthorityError> {
    if ocap_disabled {
        return Err(AuthorityError::OcapDisabled);
    }
    let mut config = TurnDriverConfig::new(url, model, backend_kind, workspace);
    config.caveats = caveats_for(kind);
    Ok(config)
}

/// Whether newt's frozen launch authority has OCAP enforcement disabled.
///
/// Newt resolves the widening switch once at startup and accepts only the
/// exact value `1`; consulting its frozen value keeps this pane guard aligned
/// with the shell dispatch it is meant to describe.
#[must_use]
pub fn ocap_disabled() -> bool {
    newt_core::agentic::ocap_disabled()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_is_a_zero_tool_read_only_observer() {
        let c = caveats_for(PaneKind::Companion);
        assert_eq!(c.fs_read, Scope::All, "reads to orient itself");
        assert_eq!(c.fs_write, Scope::none());
        assert_eq!(c.exec, Scope::none());
        assert_eq!(c.net, Scope::none());
        assert_eq!(
            c.max_calls,
            CountBound::AtMost(0),
            "a commentator cannot call tools"
        );
    }

    #[test]
    fn companion_matches_the_follow_observer_floor() {
        // The cockpit companion posture is exactly `gila follow`'s read-only
        // clamp — the same "speaks but cannot act" guarantee.
        assert_eq!(
            caveats_for(PaneKind::Companion),
            crate::follow::read_only_caveats()
        );
    }

    #[test]
    fn reader_and_workbench_deny_all_world_effects() {
        for kind in [PaneKind::Reader, PaneKind::Workbench] {
            let c = caveats_for(kind);
            assert_eq!(c.fs_write, Scope::none(), "{kind:?}: no writes");
            assert_eq!(
                c.exec,
                Scope::none(),
                "{kind:?}: no exec until the gate lands"
            );
            assert_eq!(c.net, Scope::none(), "{kind:?}: no network");
            assert_eq!(
                c.max_calls,
                CountBound::AtMost(BOUNDED_CALLS),
                "{kind:?}: a bounded, non-zero budget"
            );
        }
    }

    #[test]
    fn driver_config_stamps_the_pane_caveats() {
        // ocap enabled (false = not disabled): a config is minted, clamped.
        for kind in [PaneKind::Companion, PaneKind::Reader, PaneKind::Workbench] {
            let cfg = driver_config_inner(
                false,
                kind,
                "http://localhost:1234",
                "test-model",
                BackendKind::Ollama,
                "/ws",
            )
            .expect("ocap enabled → config built");
            assert_eq!(cfg.caveats, caveats_for(kind), "{kind:?}");
        }
    }

    #[test]
    fn driver_config_refuses_when_ocap_is_disabled() {
        // The ADR-bound refusal: no driver while NEWT_DISABLE_OCAP is set, since
        // the shell tool would bypass the lattice (verified without touching the
        // process-global env — the injected `true` stands in for it).
        let err = driver_config_inner(
            true,
            PaneKind::Companion,
            "http://localhost:1234",
            "m",
            BackendKind::Ollama,
            "/ws",
        )
        .unwrap_err();
        assert_eq!(err, AuthorityError::OcapDisabled);
        assert!(err.to_string().contains(DISABLE_OCAP_ENV));
    }
}
