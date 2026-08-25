//! Gila's emitter-side declaration of the benchmark contract.

use newt_core::{BehaviorSignal, ErrorClass, ParseSignal};

use crate::build_info;

pub const CONTRACT_VERSION: &str = "1";
pub const AGENT: &str = "gilamonster-agent";
pub const AIRFRAME_REVISION: &str = "a46d001430f4c3b9713fa5254beb9838a0777968";

pub struct ContractInputs<'a> {
    pub requested_model: &'a str,
    pub effective_model: &'a str,
    pub model_digest: Option<&'a str>,
    pub model_digest_source: Option<&'a str>,
    pub profile_sha256: &'a str,
    pub capabilities_manifest_sha256: &'a str,
    pub backend_name: &'a str,
    pub backend_kind: &'a str,
    pub outcome: &'static str,
    pub context_window: Option<u32>,
    pub tenacity: &'a str,
    pub cognition: &'a str,
    pub ocap: &'static str,
    pub max_rounds: u32,
    pub progress_grace_rounds: u32,
    pub wall_ms: u64,
    pub gen_tokens: Option<u64>,
}

#[must_use]
pub fn outcome_label(clean: bool, class: Option<ErrorClass>) -> &'static str {
    if clean {
        return "completed";
    }
    match class {
        Some(ErrorClass::Model) => "model_error",
        Some(ErrorClass::Transport) => "transport_error",
        Some(ErrorClass::Timeout) => "timeout",
        Some(ErrorClass::Harness) | None => "harness_error",
    }
}

#[must_use]
pub fn parse_signal_line(signal: &ParseSignal) -> serde_json::Value {
    serde_json::to_value(signal).expect("ParseSignal serializes")
}

#[must_use]
pub fn behavior_signal_line(signal: &BehaviorSignal) -> serde_json::Value {
    serde_json::to_value(signal).expect("BehaviorSignal serializes")
}

#[must_use]
pub fn contract_record(i: &ContractInputs<'_>) -> serde_json::Value {
    let mut timing = serde_json::json!({ "wall_ms": i.wall_ms });
    if let Some(gen) = i.gen_tokens {
        timing["gen_tokens"] = gen.into();
        if i.wall_ms > 0 {
            timing["tok_s"] = serde_json::json!(gen as f64 * 1000.0 / i.wall_ms as f64);
        }
    }

    let mut effective_config = serde_json::json!({
        "tenacity": i.tenacity,
        "cognition": i.cognition,
        "crew": "off",
        "ocap": i.ocap,
        "max_rounds": i.max_rounds,
        "progress_grace_rounds": i.progress_grace_rounds,
        "tool_routing": "off",
        "self_verify": "off",
        "flight_recorder": "on",
        "gila_capabilities": [],
    });
    if let Some(window) = i.context_window {
        effective_config["context_window"] = window.into();
    }

    let mut record = serde_json::json!({
        "contract_version": CONTRACT_VERSION,
        "requested_model": i.requested_model,
        "effective_model": i.effective_model,
        "outcome": i.outcome,
        "backend": { "name": i.backend_name, "kind": i.backend_kind },
        "agent": AGENT,
        "agent_version": build_info::VERSION_WITH_COMMIT,
        "airframe": {
            "name": "newt-agent",
            "revision": AIRFRAME_REVISION
        },
        "profile_sha256": i.profile_sha256,
        "capabilities_manifest_sha256": i.capabilities_manifest_sha256,
        "effective_config": effective_config,
        "timing": timing,
    });
    if let Some(digest) = i.model_digest {
        record["model_digest"] = digest.into();
    }
    if let Some(source) = i.model_digest_source {
        record["model_digest_source"] = source.into();
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> ContractInputs<'static> {
        ContractInputs {
            requested_model: "qwen3.6_35b",
            effective_model: "qwen3.6_35b",
            model_digest: Some("sha256:abc"),
            model_digest_source: Some("operator_supplied"),
            profile_sha256: "profile",
            capabilities_manifest_sha256: "capabilities",
            backend_name: "dgx1",
            backend_kind: "openai",
            outcome: "completed",
            context_window: Some(65_536),
            tenacity: "standard",
            cognition: "default",
            ocap: "off",
            max_rounds: 40,
            progress_grace_rounds: 5,
            wall_ms: 2_000,
            gen_tokens: Some(100),
        }
    }

    #[test]
    fn record_identifies_gila_and_its_airframe() {
        let record = contract_record(&inputs());
        assert_eq!(record["contract_version"], "1");
        assert_eq!(record["agent"], "gilamonster-agent");
        assert_eq!(record["airframe"]["revision"], AIRFRAME_REVISION);
        assert_eq!(record["effective_config"]["ocap"], "off");
        assert_eq!(
            record["effective_config"]["gila_capabilities"],
            serde_json::json!([])
        );
        assert_eq!(record["model_digest"], "sha256:abc");
        assert_eq!(record["model_digest_source"], "operator_supplied");
        assert_eq!(record["profile_sha256"], "profile");
    }

    #[test]
    fn failure_classes_are_structural() {
        assert_eq!(outcome_label(false, Some(ErrorClass::Model)), "model_error");
        assert_eq!(
            outcome_label(false, Some(ErrorClass::Transport)),
            "transport_error"
        );
        assert_eq!(outcome_label(false, Some(ErrorClass::Timeout)), "timeout");
        assert_eq!(outcome_label(false, None), "harness_error");
    }

    #[test]
    fn declared_airframe_matches_every_newt_dependency() {
        let manifest = include_str!("../Cargo.toml");
        let revisions: Vec<&str> = manifest
            .lines()
            .filter(|line| line.contains("Gilamonster-Foundation/newt-agent"))
            .filter(|line| line.contains("rev ="))
            .collect();
        assert!(!revisions.is_empty());
        assert!(
            revisions
                .iter()
                .all(|line| line.contains(AIRFRAME_REVISION)),
            "contract airframe revision drifted from Cargo.toml"
        );
    }
}
