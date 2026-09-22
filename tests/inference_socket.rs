//! Opt-in real inference probe. Commands are inspected, never executed.
use futures_util::FutureExt;
use std::{panic::AssertUnwindSafe, path::Path, time::Duration};
use thorough_but_unreliable::{
    harness::{inference::wire, types::*},
    target::*,
};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T = ()> = std::result::Result<T, Error>;
const COMMAND: &str = "printf 'hello\\n'";
const LIMIT: usize = 1048576;
const TOKENS: u32 = 4096;

fn initial_history() -> Vec<Message> {
    vec![
        Message::System("You are testing tool-call compatibility. Follow the requested two-turn protocol exactly. Use tools, not prose descriptions of calls.".into()),
        Message::User(format!("First call execute_target_command exactly once with command {COMMAND:?}. Do not submit yet. After receiving the tool result, call submit exactly once with answer equal to stdout with its trailing newline removed. This probe supplies a synthetic result; no command will actually execute.")),
    ]
}

fn continue_history(
    mut history: Vec<Message>,
    response: AssistantResponse,
) -> Result<Vec<Message>> {
    let [call] = response.tool_calls.as_slice() else {
        return Err("expected exactly one command tool call in first response".into());
    };
    if !matches!(&call.tool, Tool::ExecuteTargetCommand { command } if command == COMMAND) {
        return Err("first response must request the specified printf command".into());
    }
    let call_id = ToolCallId::try_from(call.id.clone())?;
    history.push(Message::Assistant(response));
    history.push(Message::Tool {
        call_id,
        result: Ok(ExecutionReport {
            sequence: CommandSequence::new(1),
            outcome: ExecutionOutcome::Completed {
                completion: ProcessCompletion::Exited {
                    code: 0,
                    source: CompletionSource::GuestReported,
                },
                output: CommandOutput {
                    stdout: CapturedOutput {
                        bytes: b"hello\n".to_vec(),
                        truncated: false,
                    },
                    stderr: CapturedOutput::default(),
                },
                session_state: SessionState::Ready,
            },
        }),
    });
    Ok(history)
}

fn verify_submission(response: &AssistantResponse) -> Result {
    let [call] = response.tool_calls.as_slice() else {
        return Err("expected exactly one submit tool call in second response".into());
    };
    ToolCallId::try_from(call.id.clone())?;
    if !matches!(&call.tool, Tool::Submit { answer } if answer == "hello") {
        return Err("second response must submit the synthetic stdout: hello".into());
    }
    Ok(())
}

async fn exchange(
    client: &reqwest::Client,
    model: &str,
    history: &[Message],
    directory: &Path,
    turn: u8,
) -> Result<AssistantResponse> {
    let request = wire::encode_request(model, TOKENS, history)?;
    if request.len() > LIMIT {
        return Err("request exceeds 1 MiB".into());
    }
    std::fs::write(directory.join(format!("request-{turn}.json")), &request)?;
    let mut response = client
        .post("http://10.99.1.1:11434/v1/chat/completions")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(request)
        .send()
        .await?;
    let status = response.status();
    std::fs::write(
        directory.join(format!("status-{turn}.txt")),
        status.to_string(),
    )?;
    let mut bytes = Vec::new();
    let received: Result = async {
        while let Some(chunk) = response.chunk().await? {
            let length = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or("response size overflow")?;
            if length > LIMIT {
                return Err("response exceeds 1 MiB".into());
            }
            bytes.try_reserve_exact(chunk.len())?;
            bytes.extend_from_slice(&chunk);
        }
        Ok(())
    }
    .await;
    // Preserve bounded partial/error bodies even when reading or decoding fails.
    std::fs::write(directory.join(format!("response-{turn}.json")), &bytes)?;
    received?;
    if !status.is_success() {
        return Err(format!("inference returned HTTP {status}").into());
    }
    Ok(wire::decode_response(&bytes)?)
}

async fn probe(socket: &Path, model: &str, directory: &Path) -> Result {
    if !socket.is_absolute() {
        return Err("inference socket path must be absolute".into());
    }
    let client = reqwest::Client::builder()
        .unix_socket(socket.to_path_buf())
        .no_proxy()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(300))
        .build()?;
    let history = initial_history();
    let response = exchange(&client, model, &history, directory, 1).await?;
    let history = continue_history(history, response)?;
    let response = exchange(&client, model, &history, directory, 2).await?;
    verify_submission(&response)
}

async fn recorded_probe(socket: &Path, model: &str, directory: &Path) -> Result {
    std::fs::create_dir_all(directory)?;
    let result = match AssertUnwindSafe(probe(socket, model, directory))
        .catch_unwind()
        .await
    {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            Err("probe dependency panicked".into())
        }
    };
    let diagnostic = match &result {
        Ok(()) => {
            "PASS: real tool-call exchange; command result was synthetic; no command executed\n"
                .into()
        }
        Err(error) => format!("FAIL: {error:#?}\n"),
    };
    std::fs::write(directory.join("result.txt"), &diagnostic)?;
    eprintln!("{diagnostic}Artifacts: {}", directory.display());
    result
}

#[tokio::test]
#[ignore = "real inference; run scripts/run-inference-probe.sh on the host"]
async fn real_inference() -> Result {
    let socket = std::env::var("INFERENCE_SOCKET")?;
    let model = std::env::var("INFERENCE_MODEL")?;
    recorded_probe(
        Path::new(&socket),
        &model,
        Path::new(".artifacts/inference-probe"),
    )
    .await
}

proptest::proptest! {
    #[test]
    fn synthetic_result_preserves_history_and_correlates_with_any_nonempty_id(id in ".{1,80}") {
        let prefix = initial_history();
        let assistant = AssistantResponse { text: None, tool_calls: vec![ToolCall {
            id: id.clone(), tool: Tool::ExecuteTargetCommand { command: COMMAND.into() },
        }] };
        let history = continue_history(prefix.clone(), assistant.clone())
            .map_err(|error| proptest::test_runner::TestCaseError::fail(error.to_string()))?;
        proptest::prop_assert!(history.starts_with(&prefix));
        proptest::prop_assert_eq!(history.get(prefix.len()), Some(&Message::Assistant(assistant)));
        let Some(Message::Tool { call_id, result: Ok(report) }) = history.last() else {
            return Err(proptest::test_runner::TestCaseError::fail("missing synthetic result"));
        };
        proptest::prop_assert_eq!(call_id.as_str(), id);
        proptest::prop_assert_eq!(report.sequence, CommandSequence::new(1));
        proptest::prop_assert_eq!(report.session_state(), SessionState::Ready);
    }

    #[test]
    fn batches_never_advance_unless_they_contain_exactly_one_command(count in 0usize..16) {
        let response = AssistantResponse { text: None, tool_calls: (0..count).map(|n| ToolCall {
            id: format!("call-{n}"), tool: Tool::ExecuteTargetCommand { command: COMMAND.into() },
        }).collect() };
        proptest::prop_assert_eq!(continue_history(initial_history(), response).is_ok(), count == 1);
    }
}

async fn fake_endpoint_scenario(malformed: bool) -> Result {
    use http_body_util::{BodyExt, Full, Limited};
    use hyper::{body::Bytes, server::conn::http1, service::service_fn};
    use hyper_util::rt::TokioIo;
    use serde_json::{Value, json};
    use tokio::net::UnixListener;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("inference-probe-{}-{stamp}", std::process::id()));
    std::fs::create_dir(&directory)?;
    let socket = directory.join("gateway.sock");
    let listener = UnixListener::bind(&socket)?;
    let server = async {
        for turn in 1..=if malformed { 1 } else { 2 } {
            let (connection, _) = listener.accept().await?;
            let service = service_fn(
                move |request: hyper::Request<hyper::body::Incoming>| async move {
                    if request.method() != "POST" || request.uri().path() != "/v1/chat/completions"
                    {
                        return Err::<_, Error>("unexpected request route".into());
                    }
                    let bytes = Limited::new(request.into_body(), LIMIT)
                        .collect()
                        .await?
                        .to_bytes();
                    let body: Value = serde_json::from_slice(&bytes)?;
                    if body.get("model") != Some(&json!("fixture-model"))
                        || body.get("max_tokens") != Some(&json!(TOKENS))
                        || body.get("tool_choice") != Some(&json!("auto"))
                        || body.get("stream") != Some(&json!(false))
                    {
                        return Err("unexpected inference settings".into());
                    }
                    if turn == 2 {
                        if body.pointer("/messages/3/tool_call_id") != Some(&json!("fixture-call"))
                        {
                            return Err("synthetic result lost call correlation".into());
                        }
                        let report: Value = serde_json::from_str(
                            body.pointer("/messages/3/content")
                                .and_then(Value::as_str)
                                .ok_or("missing synthetic result")?,
                        )?;
                        if report.pointer("/outcome/output/stdout/data") != Some(&json!("hello\n"))
                        {
                            return Err("missing synthetic stdout".into());
                        }
                    }
                    let (name, arguments) = if turn == 1 {
                        ("execute_target_command", json!({"command":COMMAND}))
                    } else {
                        ("submit", json!({"answer":"hello"}))
                    };
                    let body = if malformed {
                        b"{not-json".to_vec()
                    } else {
                        serde_json::to_vec(
                            &json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":{
                                "role":"assistant","content":null,"tool_calls":[{"id":"fixture-call","type":"function",
                                "function":{"name":name,"arguments":arguments.to_string()}}]
                            }}]}),
                        )?
                    };
                    Ok::<_, Error>(
                        hyper::Response::builder()
                            .header("Connection", "close")
                            .header("Content-Type", "application/json")
                            .body(Full::new(Bytes::from(body)))?,
                    )
                },
            );
            http1::Builder::new()
                .serve_connection(TokioIo::new(connection), service)
                .await?;
        }
        Ok::<_, Error>(())
    };
    let (server_result, outcome) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, recorded_probe(&socket, "fixture-model", &directory))
    })
    .await?;
    server_result?;
    if malformed {
        if outcome.is_ok() {
            return Err("malformed model response was accepted".into());
        }
        if std::fs::read(directory.join("response-1.json"))? != b"{not-json" {
            return Err("failed response was not retained".into());
        }
        if directory.join("request-2.json").exists() {
            return Err("continued after invalid response".into());
        }
    } else {
        outcome?;
        if !directory.join("response-2.json").exists() {
            return Err("missing second response artifact".into());
        }
    }
    Ok(())
}

#[tokio::test]
async fn unix_socket_probe_completes_two_turns() -> Result {
    fake_endpoint_scenario(false).await
}

#[tokio::test]
async fn invalid_model_response_is_retained_and_stops_probe() -> Result {
    fake_endpoint_scenario(true).await
}
