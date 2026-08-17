//! Headless Gilamonster solve path for external evaluators.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{ensure, Context, Result};
use newt_core::model_card::ChatCompletionsCapability;
use newt_core::role_profile::Cognition;
use newt_core::{
    BackendKind, Config, OpenAiApi, RuntimeSettingsSnapshot, TurnDriver, TurnDriverConfig,
    TurnStatus,
};
use sha2::{Digest, Sha256};

use crate::{manifest::Manifest, solve_contract};

pub const CAPABILITIES_MANIFEST_ENV: &str = "GILA_CAPABILITIES_MANIFEST";

/// Configuration resolved once, before Gila freezes launch authority.
pub struct PreparedSolve {
    config: Config,
    profile_sha256: String,
    capabilities_manifest_sha256: String,
}

impl PreparedSolve {
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }
}

/// Resolve the explicit profile and Gila's opted-in capability set.
///
/// The shared headless driver does not expose MCP tools yet. Refuse a non-empty
/// set instead of silently benchmarking a different surface from `gila code`.
pub fn prepare(profile: &Path) -> Result<PreparedSolve> {
    ensure!(
        std::env::var("NEWT_PROVIDER").map_or(true, |value| value.is_empty()),
        "NEWT_PROVIDER must be unset for an exact-profile solve"
    );
    ensure!(
        std::env::var_os("NEWT_TEAM").is_none(),
        "NEWT_TEAM must be unset for a single-agent solve"
    );
    let profile_bytes = std::fs::read(profile)
        .with_context(|| format!("reading --config {}", profile.display()))?;
    let profile_sha256 = format!("{:x}", Sha256::digest(&profile_bytes));
    let mut config =
        Config::load(profile).with_context(|| format!("loading --config {}", profile.display()))?;
    config.apply_runtime_settings();
    let capability_path = std::env::var_os(CAPABILITIES_MANIFEST_ENV)
        .map(PathBuf::from)
        .context("GILA_CAPABILITIES_MANIFEST must name the pinned empty manifest")?;
    ensure!(
        capability_path.is_file(),
        "GILA_CAPABILITIES_MANIFEST does not name a file"
    );
    let capability_bytes = std::fs::read(&capability_path).with_context(|| {
        format!(
            "reading GILA_CAPABILITIES_MANIFEST {}",
            capability_path.display()
        )
    })?;
    let capabilities_manifest_sha256 = format!("{:x}", Sha256::digest(&capability_bytes));
    let manifest = Manifest::load(&capability_path)?;
    ensure!(
        manifest.agent_exposed().next().is_none() && config.mcp_servers.is_empty(),
        "gila solve requires an empty MCP/capability set until the headless driver exposes MCP"
    );
    Ok(PreparedSolve {
        config,
        profile_sha256,
        capabilities_manifest_sha256,
    })
}

pub struct SolveArgs {
    pub cwd: PathBuf,
    pub instruction_file: PathBuf,
    pub model: String,
    pub unsafe_host_exec: bool,
    pub events: Option<PathBuf>,
    pub max_rounds: Option<usize>,
    pub context_window: Option<u32>,
    pub model_digest: Option<String>,
}

fn clean_digest(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn apply_context_config(driver: &mut TurnDriverConfig, context: Option<&newt_core::ContextConfig>) {
    let Some(context) = context else {
        return;
    };
    driver.compaction_trigger_policy = context.compaction_trigger_policy;
    driver.input_ceiling_pct =
        newt_core::config::normalize_input_ceiling_pct(context.input_ceiling_pct);
    driver.low_budget_pct = context.low_budget_pct;
    driver.estimation = context.estimation;
    driver.summary_input_cap_floor_chars = context.summary_input_cap_floor_chars;
}

fn apply_context_window(driver: &mut TurnDriverConfig, context_window: u32) {
    let input_budget =
        newt_core::config::input_percentage_ceiling(context_window, driver.input_ceiling_pct);
    driver.safe_context = Some(input_budget);
    driver.max_ok_input = Some(input_budget);
    driver.num_ctx = Some(context_window);
}

fn runtime_posture(config: &Config, model: &str) -> RuntimeSettingsSnapshot {
    newt_core::tenacity::attribute_active_family(config.tenacity.as_ref(), model);
    RuntimeSettingsSnapshot::resolve(config, None, None)
}

fn projected_cognition(
    cognition: Option<Cognition>,
    kind: BackendKind,
    api: OpenAiApi,
    capability: ChatCompletionsCapability,
) -> Option<Cognition> {
    match (kind, api) {
        (BackendKind::Openai, OpenAiApi::Responses) => cognition,
        (BackendKind::Openai, OpenAiApi::ChatCompletions) if capability.cognition == Some(true) => {
            cognition
        }
        _ => None,
    }
}

/// Run one complete Gila turn. `Ok(true)` means the agent loop completed;
/// Terminal-Bench's verifier, not this function, decides whether the task passed.
pub async fn run(prepared: PreparedSolve, args: SolveArgs) -> Result<bool> {
    ensure!(
        args.unsafe_host_exec,
        "headless host execution requires --unsafe-host-exec"
    );
    let self_verify = std::env::var("NEWT_SELF_VERIFY").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "on" | "true"
        )
    });
    ensure!(!self_verify, "self-verify must be off for this baseline");
    let authority = newt_core::launch_authority::current();
    ensure!(
        authority.ocap_disabled() && authority.full_access(),
        "Gila's ambient launch posture was not frozen before solve"
    );
    ensure!(
        std::env::var("NEWT_NO_ROUTE").as_deref() == Ok("1"),
        "Gila's ambient no-route posture is missing"
    );
    ensure!(
        std::env::var_os(newt_core::flight_recorder::CAPTURE_PATH_ENV).is_some(),
        "Gila's ambient flight recorder is not armed"
    );

    let config = prepared.config;
    let profile_sha256 = prepared.profile_sha256;
    let capabilities_manifest_sha256 = prepared.capabilities_manifest_sha256;
    let backend = config.select_configured_backend().context(
        "no usable backend in --config (add a [[backends]] entry with endpoint and model)",
    )?;
    let url = backend.endpoint.clone();
    let configured_model = backend
        .effective_model()
        .context("selected backend has no model")?;
    ensure!(
        configured_model == args.model,
        "requested model {:?} does not match profile model {:?}",
        args.model,
        configured_model
    );
    let model = args.model;
    let backend_name = backend.name.clone();
    let kind = backend.kind.unwrap_or(BackendKind::Openai);
    let api_key = backend.resolve_api_key();
    let api = backend.api.unwrap_or_default();
    let chat_capability = backend.chat_completions_capability();
    let reasoning_replay_scope = backend.reasoning_replay_scope();
    newt_tui::apply_openai_api_env(api);

    let runtime = runtime_posture(&config, &model);
    ensure!(!runtime.crew, "gila solve does not support crew mode");
    let tenacity = runtime.tenacity.label();
    let cognition = projected_cognition(runtime.cognition, kind, api, chat_capability);
    let cognition_label = cognition.map_or("default", Cognition::label);

    let instruction_file = args.instruction_file.canonicalize().with_context(|| {
        format!(
            "resolving --instruction-file {}",
            args.instruction_file.display()
        )
    })?;
    let instruction = std::fs::read_to_string(&instruction_file)
        .with_context(|| format!("reading {}", instruction_file.display()))?;
    let workspace = args
        .cwd
        .canonicalize()
        .unwrap_or(args.cwd)
        .to_string_lossy()
        .into_owned();

    let mut driver_config = TurnDriverConfig::new(&url, &model, kind, &workspace);
    apply_context_config(&mut driver_config, config.context.as_ref());
    driver_config.api_key = api_key;
    driver_config.chat_completions_capability = chat_capability;
    driver_config.reasoning_replay_scope = reasoning_replay_scope;
    if let Some(rounds) = args.max_rounds {
        driver_config.max_tool_rounds = rounds;
    }
    if let Some(window) = args.context_window {
        apply_context_window(&mut driver_config, window);
    }
    let max_rounds = driver_config.max_tool_rounds as u32;
    let progress_grace_rounds = driver_config.workflow_grace_rounds as u32;
    let mut driver = TurnDriver::new(driver_config)
        .with_cognition(cognition)
        .with_tenacity(runtime.tenacity);

    let started = Instant::now();
    driver
        .submit(instruction.trim())
        .map_err(|error| anyhow::anyhow!("submit failed: {error:?}"))?;
    let outcome = loop {
        match driver.poll() {
            TurnStatus::Completed(outcome) => break Ok(outcome),
            TurnStatus::Failed(error) => break Err(error),
            TurnStatus::Idle | TurnStatus::Running => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    };
    let elapsed = started.elapsed();
    let wall_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let observed = outcome.as_ref().ok();
    let clean = matches!(&outcome, Ok(value) if value.error.is_none());
    let (status, error) = match &outcome {
        Ok(value) if value.error.is_none() => ("completed", None),
        Ok(value) => ("failed", value.error.clone()),
        Err(error) => ("failed", Some(error.clone())),
    };
    let (tool_calls, write_calls, end_reason, trajectory) = match observed {
        Some(value) => {
            let names: Vec<&str> = value
                .tool_events
                .iter()
                .map(|event| event.tool.as_str())
                .collect();
            let writes = names
                .iter()
                .filter(|name| matches!(**name, "write_file" | "edit_file"))
                .count();
            (
                names.len(),
                writes,
                format!("{:?}", value.end_reason),
                serde_json::to_value(&value.tool_events).unwrap_or(serde_json::Value::Null),
            )
        }
        None => (0, 0, "None".to_string(), serde_json::Value::Null),
    };
    let result = serde_json::json!({
        "kind": "solve_result",
        "task_file": instruction_file.to_string_lossy(),
        "cwd": workspace,
        "model": model,
        "backend": backend_name,
        "backend_kind": kind.label(),
        "status": status,
        "reply_chars": observed.map(|value| value.reply.len()).unwrap_or(0),
        "usage_total_tokens": observed.and_then(|value| value.usage.as_ref().map(|u| u.total())),
        "hallucinations": observed.map(|value| value.hallucinations).unwrap_or(0),
        "wall_secs": elapsed.as_secs_f64(),
        "tool_calls": tool_calls,
        "write_calls": write_calls,
        "end_reason": end_reason,
        "trajectory": trajectory,
        "error": error,
    });

    let mut records = vec![result];
    if let Some(value) = observed {
        records.extend(
            value
                .parse_signals
                .iter()
                .map(solve_contract::parse_signal_line),
        );
        records.extend(
            value
                .behavior_signals
                .iter()
                .map(solve_contract::behavior_signal_line),
        );
    }
    let outcome_label = solve_contract::outcome_label(
        clean,
        match &outcome {
            Ok(value) => value.error_class,
            Err(_) => None,
        },
    );
    let effective_model = observed
        .and_then(|value| value.served_model.clone())
        .unwrap_or_else(|| model.clone());
    let model_digest = clean_digest(args.model_digest.as_deref());
    records.push(solve_contract::contract_record(
        &solve_contract::ContractInputs {
            requested_model: &model,
            effective_model: &effective_model,
            model_digest: model_digest.as_deref(),
            model_digest_source: model_digest.as_ref().map(|_| "operator_supplied"),
            profile_sha256: &profile_sha256,
            capabilities_manifest_sha256: &capabilities_manifest_sha256,
            backend_name: &backend_name,
            backend_kind: kind.label(),
            outcome: outcome_label,
            context_window: args.context_window,
            tenacity,
            cognition: cognition_label,
            ocap: "off",
            max_rounds,
            progress_grace_rounds,
            wall_ms,
            gen_tokens: observed
                .and_then(|value| value.usage.as_ref())
                .map(|usage| u64::from(usage.output_tokens)),
        },
    ));

    if let Some(path) = &args.events {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening --events {}", path.display()))?;
        for record in &records {
            writeln!(file, "{record}").context("writing events")?;
        }
    }
    for record in records {
        println!("{record}");
    }
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_trimmed_and_blank_is_absent() {
        assert_eq!(
            clean_digest(Some("  sha256:abc  ")).as_deref(),
            Some("sha256:abc")
        );
        assert_eq!(clean_digest(Some("  ")), None);
        assert_eq!(clean_digest(None), None);
    }

    #[test]
    fn context_window_sets_the_shared_input_budget() {
        let mut config =
            TurnDriverConfig::new("http://127.0.0.1:1", "test", BackendKind::Openai, "/tmp");
        apply_context_window(&mut config, 65_536);
        assert_eq!(config.num_ctx, Some(65_536));
        assert_eq!(config.safe_context, Some(52_428));
        assert_eq!(config.max_ok_input, config.safe_context);
    }
}
