use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

struct WriteThenFinish {
    calls: AtomicUsize,
}

impl Respond for WriteThenFinish {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-gila-write",
                "object": "chat.completion",
                "created": 0,
                "model": "probe-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "write-proof",
                            "type": "function",
                            "function": {
                                "name": "write_file",
                                "arguments": "{\"path\":\"gila-tool-proof.txt\",\"content\":\"written by gila\\n\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-gila-finish",
            "object": "chat.completion",
            "created": 0,
            "model": "probe-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "done" },
                "finish_reason": "stop"
            }]
        }))
    }
}

fn fixture(server: &MockServer) -> (TempDir, Vec<String>) {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::create_dir(temp.path().join("workspace")).expect("workspace");
    fs::write(
        temp.path().join("task.md"),
        "Reply when the task is complete.",
    )
    .expect("instruction");
    fs::write(temp.path().join("capabilities.toml"), "").expect("empty manifest");
    fs::write(
        temp.path().join("bench.toml"),
        format!(
            "default_backend = \"bench\"\n\
             [[backends]]\n\
             name = \"bench\"\n\
             endpoint = {:?}\n\
             model = \"probe-model\"\n\
             kind = \"openai\"\n\
             api = \"chat_completions\"\n",
            server.uri()
        ),
    )
    .expect("profile");
    let args = vec![
        "solve".into(),
        "--cwd".into(),
        temp.path().join("workspace").display().to_string(),
        "--instruction-file".into(),
        temp.path().join("task.md").display().to_string(),
        "--config".into(),
        temp.path().join("bench.toml").display().to_string(),
        "--model".into(),
        "probe-model".into(),
        "--events".into(),
        temp.path().join("events.jsonl").display().to_string(),
        "--max-rounds".into(),
        "1".into(),
        "--context-window".into(),
        "65536".into(),
        "--model-digest".into(),
        "a".repeat(64),
        "--unsafe-host-exec".into(),
    ];
    (temp, args)
}

fn command(temp: &TempDir) -> Command {
    let mut command = Command::cargo_bin("gila-headless").expect("headless binary");
    command.env_clear().env("HOME", temp.path()).env(
        "GILA_CAPABILITIES_MANIFEST",
        temp.path().join("capabilities.toml"),
    );
    command
}

#[tokio::test(flavor = "multi_thread")]
async fn native_solve_emits_one_gila_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-gila-test",
            "object": "chat.completion",
            "created": 0,
            "model": "probe-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "done" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11 }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (temp, args) = fixture(&server);

    command(&temp).args(&args).assert().success();
    let records: Vec<Value> = fs::read_to_string(temp.path().join("events.jsonl"))
        .expect("events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONL"))
        .collect();
    let contracts: Vec<&Value> = records
        .iter()
        .filter(|record| record.get("contract_version").is_some())
        .collect();
    assert_eq!(contracts.len(), 1);
    let contract = contracts[0];
    assert_eq!(contract["agent"], "gilamonster-agent");
    assert_eq!(contract["requested_model"], "probe-model");
    assert_eq!(contract["effective_model"], "probe-model");
    assert_eq!(contract["model_digest"], "a".repeat(64));
    assert_eq!(contract["model_digest_source"], "operator_supplied");
    assert_eq!(contract["profile_sha256"].as_str().map(str::len), Some(64));
    assert_eq!(
        contract["capabilities_manifest_sha256"],
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(contract["effective_config"]["ocap"], "off");
    assert_eq!(contract["effective_config"]["self_verify"], "off");
    assert_eq!(
        contract["effective_config"]["gila_capabilities"],
        serde_json::json!([])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn native_solve_executes_a_workspace_write() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(WriteThenFinish {
            calls: AtomicUsize::new(0),
        })
        .expect(2)
        .mount(&server)
        .await;
    let (temp, mut args) = fixture(&server);
    let max_rounds = args
        .iter()
        .position(|arg| arg == "--max-rounds")
        .expect("max-rounds argument");
    args[max_rounds + 1] = "2".into();

    command(&temp).args(&args).assert().success();
    assert_eq!(
        fs::read_to_string(temp.path().join("workspace/gila-tool-proof.txt"))
            .expect("workspace write"),
        "written by gila\n"
    );
    let flight_recorder = temp.path().join(".newt/flight-recorder/unconfined.jsonl");
    assert!(fs::read_to_string(flight_recorder)
        .expect("ambient flight recorder")
        .contains("gila-tool-proof.txt"));
    let records: Vec<Value> = fs::read_to_string(temp.path().join("events.jsonl"))
        .expect("events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONL"))
        .collect();
    let result = records
        .iter()
        .find(|record| record["kind"] == "solve_result")
        .expect("solve result");
    assert_eq!(result["write_calls"], 1);
    assert!(result["trajectory"]
        .as_array()
        .is_some_and(|events| events.iter().any(|event| event["tool"] == "write_file")));
}

#[tokio::test(flavor = "multi_thread")]
async fn model_mismatch_fails_before_inference_or_trace() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let (temp, mut args) = fixture(&server);
    let position = args
        .iter()
        .position(|arg| arg == "probe-model")
        .expect("model argument");
    args[position] = "wrong-model".into();

    command(&temp)
        .args(&args)
        .assert()
        .failure()
        .stderr(predicates::str::contains("does not match profile model"));
    assert!(!temp.path().join("events.jsonl").exists());
}
