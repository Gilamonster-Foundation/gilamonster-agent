//! `gila chain` — the LangChain exploration surface.
//!
//! # What this is
//!
//! A minimal [langchain-rust](https://github.com/Abraxas-365/langchain-rust)
//! `LLMChain` (system prompt + human template → LLM) driven against **the same
//! newt backend the rest of the airframe uses**: endpoint, model, and bearer
//! token come from `newt_core::Config::resolve()` — exactly the seam `gila
//! follow` / `gila cowork` read — so no host, port, or credential ever appears
//! in code. The chain speaks the OpenAI wire protocol, which every backend the
//! matrix fronts (llama.cpp's server, vLLM, Ollama) serves at `/v1`.
//!
//! # Why it exists
//!
//! An exploration: what does the LangChain abstraction (prompt templates,
//! chains, later agents/tools) buy the matrix beyond `TurnDriver`? To keep the
//! answer honest and the exit cheap, the whole dependency is confined to this
//! one module behind two seams:
//!
//! - [`ChainSettings`] / [`settings_from_backend`] — the *pure* mapping from
//!   newt's [`BackendConfig`](newt_core::BackendConfig) into what a LangChain
//!   LLM client needs (unit-tested, fail-loud on an unset model, mirroring
//!   `follow::config_from_backend`).
//! - [`ask`] — the one live entry point: build the chain, run one prompt.
//!
//! upstream langchain-rust last published 2024-10 (4.6.0); if it stays dormant,
//! deleting this module and the one `Cargo.toml` line removes the experiment.
//!
//! # Authority posture
//!
//! A bare LLMChain calls no tools and touches no filesystem — its only world
//! effect is the HTTPS call to the operator's own configured inference
//! endpoint, the same call every `gila` surface makes. Tool-capable LangChain
//! agents are explicitly out of scope for this slice; they would have to come
//! back through the `authority` seam like every other pane.

use langchain_rust::chain::{Chain, LLMChainBuilder};
use langchain_rust::llm::openai::{OpenAI, OpenAIConfig};
use langchain_rust::schemas::Message;
use langchain_rust::{fmt_message, fmt_template, message_formatter, prompt_args, template_fstring};

/// The standing frame `gila chain` gives the model: answer as the matrix's
/// resident, briefly. Kept short — this is a play surface, not a persona.
pub const CHAIN_SYSTEM_PROMPT: &str = "You are gila, the Gilamonster agent matrix's \
     resident assistant, answering one question over a LangChain LLMChain. \
     Answer concisely and concretely.";

/// What a LangChain OpenAI-protocol client needs from a newt backend. The pure
/// product of [`settings_from_backend`]; construction is the only coupling to
/// newt's config shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSettings {
    /// OpenAI-compatible base URL **including** the `/v1` suffix
    /// (async-openai's convention), derived via [`openai_base`].
    pub base_url: String,
    /// The model to name in the request.
    pub model: String,
    /// Bearer token, when the backend declares one. `None` for the typical
    /// local endpoint; the wire client sends an empty bearer, which llama.cpp
    /// / vLLM / Ollama accept.
    pub api_key: Option<String>,
}

/// Map a newt endpoint to the OpenAI-protocol base URL langchain-rust's client
/// expects: trailing slashes trimmed, `/v1` appended exactly once. newt configs
/// conventionally store the bare origin (`http://host:8080`) because newt's own
/// dispatch appends full paths; async-openai instead wants the `/v1` base.
#[must_use]
pub fn openai_base(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

/// Build [`ChainSettings`] from the operator's configured newt backend.
///
/// Fail-loud on an unset model, exactly like `follow::config_from_backend` and
/// `newt solve`: newt's #1128 made `model` optional ("the server dictates",
/// filled by session-start probing), and the chain has no probe step. The
/// backend's *kind* is irrelevant here — the chain always speaks the OpenAI
/// protocol at `/v1`, which all the fronted backends serve.
pub fn settings_from_backend(backend: &newt_core::BackendConfig) -> anyhow::Result<ChainSettings> {
    let model = backend
        .effective_model()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "backend `{}` has no model (set model = in the [[backends]] entry — \
                 gila chain has no probe step to adopt one)",
                backend.name
            )
        })?
        .to_string();
    Ok(ChainSettings {
        base_url: openai_base(&backend.endpoint),
        model,
        api_key: backend.resolve_api_key(),
    })
}

/// Run one question through the LLMChain (system frame + human template) and
/// return the model's reply.
///
/// This is the module's single live entry point. It is exercised end-to-end in
/// tests against a mock OpenAI-protocol server — the same wiremock pattern
/// `follow`'s channel tests use — so the chain wiring itself is covered; only
/// the operator-facing call in `main.rs` stays uncovered (by design, like every
/// other `run_*` arm).
pub async fn ask(settings: &ChainSettings, question: &str) -> anyhow::Result<String> {
    let config = OpenAIConfig::default()
        .with_api_base(settings.base_url.clone())
        .with_api_key(settings.api_key.clone().unwrap_or_default());
    let llm = OpenAI::default()
        .with_config(config)
        .with_model(settings.model.clone());

    let prompt = message_formatter![
        fmt_message!(Message::new_system_message(CHAIN_SYSTEM_PROMPT)),
        fmt_template!(langchain_rust::prompt::HumanMessagePromptTemplate::new(
            template_fstring!("{question}", "question")
        )),
    ];
    let chain = LLMChainBuilder::new()
        .prompt(prompt)
        .llm(llm)
        .build()
        .map_err(|e| anyhow::anyhow!("building the LLMChain: {e}"))?;
    chain
        .invoke(prompt_args! { "question" => question })
        .await
        .map_err(|e| anyhow::anyhow!("running the chain: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- openai_base (pure) -------------------------------------------------

    #[test]
    fn openai_base_appends_v1_once() {
        assert_eq!(
            openai_base("http://192.0.2.7:8080"),
            "http://192.0.2.7:8080/v1"
        );
        assert_eq!(
            openai_base("http://192.0.2.7:8080/"),
            "http://192.0.2.7:8080/v1"
        );
        assert_eq!(
            openai_base("http://192.0.2.7:8080/v1"),
            "http://192.0.2.7:8080/v1"
        );
        assert_eq!(
            openai_base("http://192.0.2.7:8080/v1/"),
            "http://192.0.2.7:8080/v1"
        );
    }

    // --- settings_from_backend (pure) ---------------------------------------

    #[test]
    fn settings_thread_endpoint_model_and_key_absence() {
        let backend = newt_core::BackendConfig {
            name: "local".into(),
            endpoint: "http://192.0.2.7:8080".into(),
            model: Some("test-model".into()),
            ..Default::default()
        };
        let s = settings_from_backend(&backend).expect("model set");
        assert_eq!(
            s,
            ChainSettings {
                base_url: "http://192.0.2.7:8080/v1".into(),
                model: "test-model".into(),
                api_key: None,
            }
        );
    }

    /// Same fail-loud contract as `follow::config_from_backend`: an unset model
    /// (newt #1128 "the server dictates") must error, never silently name an
    /// empty model on the wire.
    #[test]
    fn settings_reject_a_backend_with_no_model() {
        let backend = newt_core::BackendConfig {
            name: "probe-me".into(),
            endpoint: "http://192.0.2.7:8080".into(),
            model: None,
            ..Default::default()
        };
        let err = settings_from_backend(&backend).unwrap_err();
        assert!(err.to_string().contains("probe-me"));
        assert!(err.to_string().contains("no model"));
    }

    // --- ask, end-to-end against a mock OpenAI-protocol server --------------

    /// THE chain test: the LLMChain really renders the prompt (system frame +
    /// question) into an OpenAI chat request and returns the mocked reply —
    /// proving the langchain wiring without a live backend. Mirrors follow's
    /// wiremock channel test.
    #[tokio::test]
    async fn ask_sends_the_framed_prompt_and_returns_the_reply() {
        use std::sync::{Arc, Mutex};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(String::new()));
        struct Capture {
            seen: Arc<Mutex<String>>,
        }
        impl Respond for Capture {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                *self.seen.lock().unwrap() = String::from_utf8_lossy(&req.body).into_owned();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "chatcmpl-test",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "the matrix says hi" },
                        "finish_reason": "stop"
                    }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                }))
            }
        }
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(Capture { seen: seen.clone() })
            .mount(&server)
            .await;

        let settings = ChainSettings {
            base_url: openai_base(&server.uri()),
            model: "test-model".into(),
            api_key: None,
        };
        let reply = ask(&settings, "what is a gila monster?")
            .await
            .expect("mocked chain run succeeds");
        assert_eq!(reply, "the matrix says hi");

        let body = seen.lock().unwrap().clone();
        assert!(body.contains("test-model"), "names the configured model");
        assert!(
            body.contains("what is a gila monster?"),
            "the question reached the wire"
        );
        assert!(
            body.contains("Gilamonster agent matrix"),
            "the system frame reached the wire"
        );
    }
}
