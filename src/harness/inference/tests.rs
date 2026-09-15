use std::{io, time::Duration};

use proptest::{prelude::*, test_runner::TestCaseError};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
    time::{sleep, timeout},
};

use super::{InferenceClient, InferenceConfig, InferenceError, http::append_chunk, wire};
use crate::harness::types::*;
use crate::target::*;

fn id(value: &str) -> Result<ToolCallId, EmptyToolCallId> {
    ToolCallId::try_from(value.to_owned())
}

fn lost_reply() -> CommandClientError {
    CommandClientError {
        kind: CommandClientErrorKind::Transport,
        diagnostic: Some("lost command reply".into()),
    }
}

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn field<'a>(value: &'a Value, pointer: &str) -> Result<&'a Value, TestCaseError> {
    value
        .pointer(pointer)
        .ok_or_else(|| TestCaseError::fail(format!("missing {pointer}")))
}

fn response(message: Value, reason: &str) -> Value {
    json!({"choices":[{"index":0,"finish_reason":reason,"message":message}],"usage":{"total_tokens":42}})
}

fn call(id: &str, name: &str, arguments: String) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}})
}

fn command_response() -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&response(
        json!({"role":"assistant","content":null,"tool_calls":[
            call("call-1", "execute_target_command", r#"{"command":"id"}"#.into())
        ]}),
        "tool_calls",
    ))
}

proptest! {
    #[test]
    fn request_preserves_history_settings_and_exact_tool_arguments(
        system in text(), task in text(), command in text(), stdout in text(), stderr in text(), code in any::<u8>(), tokens in 1u32..u32::MAX,
    ) {
        let history = vec![Message::System(system.clone()), Message::User(task.clone()),
            Message::Assistant(AssistantResponse { text: None, tool_calls: vec![ToolCall { id: "c".into(), tool: Tool::ExecuteTargetCommand { command: command.clone() } }] }),
            Message::Tool { call_id: id("c")?, result: Ok(text_result(&stdout, &stderr, code)) }];
        let bytes = wire::encode_request("test-model", tokens, &history)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        prop_assert_eq!(field(&value, "/model")?, &json!("test-model"));
        prop_assert_eq!(field(&value, "/max_tokens")?, &json!(tokens));
        prop_assert_eq!(field(&value, "/stream")?, &json!(false));
        prop_assert_eq!(field(&value, "/n")?, &json!(1));
        prop_assert_eq!(field(&value, "/messages/0")?, &json!({"role":"system","content":system}));
        prop_assert_eq!(field(&value, "/messages/1")?, &json!({"role":"user","content":task}));
        let args = field(&value, "/messages/2/tool_calls/0/function/arguments")?.as_str().ok_or_else(|| TestCaseError::fail("arguments must be a JSON string"))?;
        prop_assert_eq!(serde_json::from_str::<Value>(args)?, json!({"command":command}));
        prop_assert_eq!(field(&value, "/messages/3/tool_call_id")?, &json!("c"));
        let result = field(&value, "/messages/3/content")?.as_str().ok_or_else(|| TestCaseError::fail("result must be a JSON string"))?;
        prop_assert_eq!(serde_json::from_str::<Value>(result)?, json!({"sequence":1,"session_state":"ready","outcome":{"kind":"completed","output":{"stdout":{"encoding":"utf8","data":stdout,"truncated":false},"stderr":{"encoding":"utf8","data":stderr,"truncated":false}},"completion":{"kind":"exited","code":code,"source":"parent_observed"}}}));
        prop_assert_eq!(field(&value, "/tools/0/function/name")?, &json!("execute_target_command"));
        prop_assert_eq!(field(&value, "/tools/1/function/name")?, &json!("submit"));
        prop_assert_eq!(field(&value, "/tools/0/function/parameters/required")?, &json!(["command"]));
        prop_assert_eq!(field(&value, "/tools/1/function/parameters/required")?, &json!(["answer"]));
    }

    #[test]
    fn response_preserves_text_call_ids_order_and_argument_values(
        content in prop::option::of(text()), calls in prop::collection::vec((text(), text(), any::<bool>()), 0..12),
    ) {
        let mut expected = AssistantResponse { text: content.clone(), tool_calls: vec![] };
        let mut wire_calls = Vec::new();
        for (index, (suffix, value, submit)) in calls.into_iter().enumerate() {
            let id = format!("{index}:{suffix}");
            let (name, args, tool) = if submit {
                ("submit", json!({"answer":value}), Tool::Submit { answer: value })
            } else {
                ("execute_target_command", json!({"command":value}), Tool::ExecuteTargetCommand { command: value })
            };
            wire_calls.push(call(&id, name, serde_json::to_string(&args)?));
            expected.tool_calls.push(ToolCall { id, tool });
        }
        let value = response(json!({"role":"assistant","content":content,"tool_calls":wire_calls}), "stop");
        let decoded = wire::decode_response(&serde_json::to_vec(&value)?)?;
        prop_assert_eq!(&decoded, &expected);
        let encoded: Value = serde_json::from_slice(&wire::encode_request("model", 10, &[Message::Assistant(decoded)])?)?;
        prop_assert_eq!(field(&encoded, "/messages/0/content")?, &json!(expected.text));
        if wire_calls.is_empty() {
            prop_assert!(encoded.pointer("/messages/0/tool_calls").is_none());
        } else {
            prop_assert_eq!(field(&encoded, "/messages/0/tool_calls")?, &json!(wire_calls));
        }
    }

    #[test]
    fn malformed_argument_objects_never_produce_a_tool(value in text(), submit in any::<bool>(), kind in 0u8..5) {
        let name = if submit { "submit" } else { "execute_target_command" };
        let key = if submit { "answer" } else { "command" };
        let args = match kind {
            0 => json!([value]),
            1 => json!({}),
            2 => json!({key:42}),
            3 => json!({key:value,"extra":true}),
            _ => Value::Null,
        };
        let value = response(json!({"role":"assistant","tool_calls":[call("c", name, serde_json::to_string(&args)?)]}), "tool_calls");
        prop_assert!(wire::decode_response(&serde_json::to_vec(&value)?).is_err());
    }

    #[test]
    fn unknown_tools_are_rejected(name in text()) {
        let name = format!("unsupported:{name}");
        let value = response(json!({"role":"assistant","tool_calls":[call("c", &name, "{}".into())]}), "tool_calls");
        let result = wire::decode_response(&serde_json::to_vec(&value)?);
        prop_assert!(matches!(result, Err(InferenceError::UnsupportedTool(actual)) if actual == name));
    }

    #[test]
    fn body_limit_is_exact_and_failed_appends_do_not_change_the_buffer(
        before in prop::collection::vec(any::<u8>(), 0..64), chunk in prop::collection::vec(any::<u8>(), 0..64), limit in 0usize..128,
    ) {
        let mut buffer = before.clone();
        let result = append_chunk(&mut buffer, &chunk, limit);
        if before.len().saturating_add(chunk.len()) <= limit {
            prop_assert!(result.is_ok());
            prop_assert_eq!(buffer, [before, chunk].concat());
        } else {
            prop_assert!(matches!(result, Err(InferenceError::ResponseTooLarge { .. })), "expected size-limit error");
            prop_assert_eq!(buffer, before);
        }
    }
}

#[test]
fn rejects_malformed_envelopes_and_truncated_or_ambiguous_completions() -> TestResult {
    let good: Value = serde_json::from_slice(&command_response()?)?;
    let mut fixtures = vec![
        json!([]),
        json!({}),
        json!({"choices":[]}),
        json!({"choices":[{},{}]}),
    ];
    for (pointer, replacement) in [
        ("/choices/0/index", json!(1)),
        ("/choices/0/message/role", json!("user")),
        ("/choices/0/finish_reason", json!("length")),
        ("/choices/0/finish_reason", json!("content_filter")),
        ("/choices/0/finish_reason", Value::Null),
        ("/choices/0/message", json!(["assistant", null, []])),
        ("/choices/0/message/tool_calls", json!([])),
        ("/choices/0/message/tool_calls/0/id", json!("")),
        ("/choices/0/message/tool_calls/0/type", json!("custom")),
        (
            "/choices/0/message/tool_calls/0/function/arguments",
            json!("{"),
        ),
        (
            "/choices/0/message/tool_calls/0/function/arguments",
            json!("{\"command\":\"id\",\"command\":\"pwd\"}"),
        ),
        (
            "/choices/0/message/tool_calls/0/function/arguments",
            json!({"command":"id"}),
        ),
    ] {
        let mut fixture = good.clone();
        let field = fixture
            .pointer_mut(pointer)
            .ok_or("fixture pointer missing")?;
        *field = replacement;
        fixtures.push(fixture);
    }
    let first = good
        .pointer("/choices/0/message/tool_calls/0")
        .ok_or("fixture call missing")?;
    fixtures.push(response(
        json!({"role":"assistant","tool_calls":[first,first]}),
        "tool_calls",
    ));
    for fixture in fixtures {
        assert!(
            wire::decode_response(&serde_json::to_vec(&fixture)?).is_err(),
            "accepted {fixture}"
        );
    }
    assert!(matches!(
        wire::decode_response(&serde_json::to_vec(&response(
            json!({"role":"assistant","content":"partial"}),
            "length"
        ))?),
        Err(InferenceError::TruncatedResponse)
    ));
    Ok(())
}

#[test]
fn inference_failure_conversion_preserves_categories_payloads_and_diagnostics() -> TestResult {
    let json_error = serde_json::from_slice::<Value>(b"{")
        .err()
        .ok_or("expected JSON failure")?;
    let allocation_error = Vec::<u8>::new()
        .try_reserve(usize::MAX)
        .err()
        .ok_or("expected allocation failure")?;
    for (error, kind) in [
        (
            InferenceError::Configuration("bad model".into()),
            ModelFailureKind::Configuration,
        ),
        (
            InferenceError::InvalidHistory("empty"),
            ModelFailureKind::InvalidHistory,
        ),
        (InferenceError::Json(json_error), ModelFailureKind::Json),
        (
            InferenceError::InvalidResponse("wrong role"),
            ModelFailureKind::InvalidResponse,
        ),
        (
            InferenceError::UnsupportedTool("unexpected".into()),
            ModelFailureKind::UnsupportedTool("unexpected".into()),
        ),
        (
            InferenceError::TruncatedResponse,
            ModelFailureKind::TokenLimit,
        ),
        (
            InferenceError::HttpStatus(429),
            ModelFailureKind::HttpStatus(429),
        ),
        (
            InferenceError::RequestTooLarge { limit: 123 },
            ModelFailureKind::RequestTooLarge { limit: 123 },
        ),
        (
            InferenceError::ResponseTooLarge { limit: 456 },
            ModelFailureKind::ResponseTooLarge { limit: 456 },
        ),
        (
            InferenceError::Allocation(allocation_error),
            ModelFailureKind::Allocation,
        ),
        (
            InferenceError::DependencyPanicked,
            ModelFailureKind::Panicked,
        ),
    ] {
        let diagnostic = error.to_string();
        let converted = ModelFailure::from(error);
        assert_eq!(converted.kind, kind);
        assert_eq!(converted.diagnostic, Some(diagnostic));
        assert_eq!(
            converted.completion_unknown(),
            kind == ModelFailureKind::Panicked
        );
    }
    Ok(())
}

enum Reply {
    Http { status: u16, body: Vec<u8> },
    Redirect,
    OversizedHeader,
    Chunked,
    Disconnect,
    Stall { headers: bool },
}

struct Captured {
    request_line: String,
    headers: String,
    body: Vec<u8>,
    extra_connection: bool,
}

async fn fake_server(reply: Reply) -> io::Result<(String, JoinHandle<io::Result<Captured>>)> {
    let (endpoint, server) = fake_sequence(vec![reply]).await?;
    Ok((
        endpoint,
        tokio::spawn(async move {
            server
                .await
                .map_err(io::Error::other)??
                .into_iter()
                .next()
                .ok_or_else(|| io::Error::other("missing captured request"))
        }),
    ))
}

async fn fake_sequence(
    replies: Vec<Reply>,
) -> io::Result<(String, JoinHandle<io::Result<Vec<Captured>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1/chat/completions", listener.local_addr()?);
    let location = endpoint.clone();
    let server = tokio::spawn(async move {
        timeout(Duration::from_secs(5), async move {
            let mut captured = Vec::new();
            for reply in replies {
                let (socket, _) = listener.accept().await?;
                let mut socket = BufReader::new(socket);
                let mut request_line = String::new();
                socket.read_line(&mut request_line).await?;
                let mut headers = String::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if socket.read_line(&mut line).await? == 0 {
                        return Err(io::Error::other("unexpected EOF reading request headers"));
                    }
                    if line == "\r\n" { break; }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().map_err(io::Error::other)?;
                    }
                    headers.push_str(&line);
                    if headers.len() > 16384 || length > 65536 {
                        return Err(io::Error::other("fake server request too large"));
                    }
                }
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await?;
                match reply {
                    Reply::Http { status, body } => {
                        socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await?;
                        socket.write_all(&body).await?;
                    },
                    Reply::Redirect => socket.write_all(format!("HTTP/1.1 307 Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await?,
                    Reply::OversizedHeader => socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n").await?,
                    Reply::Chunked => socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n8\r\n12345678\r\n8\r\n12345678\r\n0\r\n\r\n").await?,
                    Reply::Disconnect => {},
                    Reply::Stall { headers } => {
                        if headers {
                            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx").await?;
                            socket.flush().await?;
                        }
                        sleep(Duration::from_millis(250)).await;
                    },
                }
                socket.shutdown().await?;
                drop(socket);
                captured.push(Captured { request_line, headers, body, extra_connection: false });
            }
            let extra_connection = timeout(Duration::from_millis(100), listener.accept()).await.is_ok();
            for request in &mut captured {
                request.extra_connection = extra_connection;
            }
            Ok(captured)
        }).await.map_err(io::Error::other)?
    });
    Ok((endpoint, server))
}

fn config(endpoint: String) -> InferenceConfig {
    InferenceConfig {
        completion_url: endpoint,
        model: "test-model".into(),
        max_tokens: 256,
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(2),
        max_request_bytes: 65536,
        max_response_bytes: 65536,
    }
}

fn history() -> Vec<Message> {
    vec![
        Message::System("system".into()),
        Message::User("task".into()),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn round_trip_sends_the_expected_request_and_decodes_a_command() -> TestResult {
    let (endpoint, server) = fake_server(Reply::Http {
        status: 200,
        body: command_response()?,
    })
    .await?;
    let client = InferenceClient::new(config(endpoint))?;
    let response = client.complete(&history()).await?;
    let captured = server.await??;
    assert_eq!(
        captured.request_line,
        "POST /v1/chat/completions HTTP/1.1\r\n"
    );
    assert!(
        captured
            .headers
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert!(!captured.extra_connection);
    assert_eq!(
        serde_json::from_slice::<Value>(&captured.body)?,
        serde_json::from_slice::<Value>(&wire::encode_request("test-model", 256, &history())?)?
    );
    assert_eq!(
        response,
        AssistantResponse {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                tool: Tool::ExecuteTargetCommand {
                    command: "id".into()
                }
            }]
        }
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn redirects_http_errors_and_disconnects_are_not_retried() -> TestResult {
    for reply in [
        Reply::Redirect,
        Reply::Http {
            status: 503,
            body: vec![],
        },
        Reply::Disconnect,
    ] {
        let (endpoint, server) = fake_server(reply).await?;
        let result = InferenceClient::new(config(endpoint))?
            .complete(&history())
            .await;
        assert!(matches!(
            result,
            Err(InferenceError::HttpStatus(307 | 503) | InferenceError::Transport(_))
        ));
        assert!(!server.await??.extra_connection);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_http_response_returns_a_decode_error() -> TestResult {
    let (endpoint, server) = fake_server(Reply::Http {
        status: 200,
        body: b"not JSON".to_vec(),
    })
    .await?;
    let result = InferenceClient::new(config(endpoint))?
        .complete(&history())
        .await;
    assert!(matches!(result, Err(InferenceError::Json(_))));
    assert!(!server.await??.extra_connection);
    Ok(())
}

#[test]
fn rejects_invalid_configuration_without_panicking() {
    for endpoint in [
        "not a URL",
        "file:///v1/chat/completions",
        "http://localhost/v1",
        "http://user:password@localhost/v1/chat/completions",
        "http://localhost/v1/chat/completions?x=1",
        "http://localhost/v1/chat/completions#x",
    ] {
        assert!(matches!(
            InferenceClient::new(config(endpoint.into())),
            Err(InferenceError::Configuration(_))
        ));
    }
    let mut settings = config("http://localhost/v1/chat/completions".into());
    settings.request_timeout = Duration::MAX;
    assert!(matches!(
        InferenceClient::new(settings),
        Err(InferenceError::Configuration(_))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn response_limit_handles_content_length_and_chunked_bodies() -> TestResult {
    for reply in [Reply::OversizedHeader, Reply::Chunked] {
        let (endpoint, server) = fake_server(reply).await?;
        let mut settings = config(endpoint);
        settings.max_response_bytes = 10;
        let result = InferenceClient::new(settings)?.complete(&history()).await;
        assert!(matches!(
            result,
            Err(InferenceError::ResponseTooLarge { limit: 10 })
        ));
        assert!(!server.await??.extra_connection);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn deadline_covers_waiting_for_headers_and_reading_the_body() -> TestResult {
    for headers in [false, true] {
        let (endpoint, server) = fake_server(Reply::Stall { headers }).await?;
        let mut settings = config(endpoint);
        settings.request_timeout = Duration::from_millis(100);
        let result = InferenceClient::new(settings)?.complete(&history()).await;
        assert!(matches!(result, Err(InferenceError::Transport(error)) if error.is_timeout()));
        assert!(!server.await??.extra_connection);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_request_is_rejected_before_connecting() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut settings = config(format!(
        "http://{}/v1/chat/completions",
        listener.local_addr()?
    ));
    settings.max_request_bytes = 1;
    let result = InferenceClient::new(settings)?.complete(&history()).await;
    assert!(matches!(
        result,
        Err(InferenceError::RequestTooLarge { limit: 1 })
    ));
    assert!(
        timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[derive(Default)]
struct MemoryExecutor {
    calls: Vec<CommandCall>,
    failure: Option<CommandClientError>,
}

fn text_result(stdout: &str, stderr: &str, code: u8) -> ExecutionReport {
    ExecutionReport {
        sequence: CommandSequence::new(1),
        outcome: ExecutionOutcome::Completed {
            output: CommandOutput {
                stdout: CapturedOutput {
                    bytes: stdout.as_bytes().to_vec(),
                    truncated: false,
                },
                stderr: CapturedOutput {
                    bytes: stderr.as_bytes().to_vec(),
                    truncated: false,
                },
            },
            completion: ProcessCompletion::Exited {
                code,
                source: CompletionSource::ParentObserved,
            },
            session_state: SessionState::Ready,
        },
    }
}

fn fixed_result() -> ExecutionReport {
    text_result("uid=900(harness)\n", "diagnostic\n", 7)
}

impl crate::harness::async_driver::CommandExecutor for MemoryExecutor {
    async fn execute(&mut self, call: &CommandCall) -> Result<ExecutionReport, CommandClientError> {
        tokio::task::yield_now().await;
        self.calls.push(call.clone());
        match &self.failure {
            Some(error) => Err(error.clone()),
            None => {
                let sequence =
                    u64::try_from(self.calls.len()).map_err(|error| CommandClientError {
                        kind: CommandClientErrorKind::Configuration,
                        diagnostic: Some(error.to_string()),
                    })?;
                let mut report = fixed_result();
                report.sequence = CommandSequence::new(sequence);
                Ok(report)
            }
        }
    }
}

fn batch_response(submit: bool) -> Result<Vec<u8>, serde_json::Error> {
    let second = if submit {
        call("call-2", "submit", r#"{"answer":"done"}"#.into())
    } else {
        call(
            "call-2",
            "execute_target_command",
            r#"{"command":"pwd"}"#.into(),
        )
    };
    serde_json::to_vec(&response(
        json!({"role":"assistant","content":null,"tool_calls":[
            call("call-1", "execute_target_command", r#"{"command":"id"}"#.into()), second
        ]}),
        "tool_calls",
    ))
}

#[tokio::test(flavor = "current_thread")]
async fn async_loop_sends_ordered_results_then_records_submission() -> TestResult {
    let commands = batch_response(false)?;
    let submit = serde_json::to_vec(&response(
        json!({"role":"assistant","content":null,"tool_calls":[
            call("final", "submit", r#"{"answer":"done"}"#.into())
        ]}),
        "tool_calls",
    ))?;
    let (endpoint, server) = fake_sequence(vec![
        Reply::Http {
            status: 200,
            body: commands.clone(),
        },
        Reply::Http {
            status: 200,
            body: submit.clone(),
        },
    ])
    .await?;
    let mut client = InferenceClient::new(config(endpoint))?;
    let mut executor = MemoryExecutor::default();
    let report = crate::harness::async_driver::run(
        "system".into(),
        "task".into(),
        2,
        &mut client,
        &mut executor,
    )
    .await;
    let captured = server.await??;
    assert_eq!(captured.len(), 2);
    assert!(captured.iter().all(|request| !request.extra_connection));
    assert_eq!(
        executor.calls,
        vec![
            CommandCall {
                id: id("call-1")?,
                command: "id".into()
            },
            CommandCall {
                id: id("call-2")?,
                command: "pwd".into()
            },
        ]
    );
    let mut expected = history();
    expected.push(Message::Assistant(wire::decode_response(&commands)?));
    for (index, call) in executor.calls.iter().enumerate() {
        let mut result = fixed_result();
        result.sequence = CommandSequence::new(
            u64::try_from(index)?
                .checked_add(1)
                .ok_or("sequence overflow")?,
        );
        expected.push(Message::Tool {
            call_id: call.id.clone(),
            result: Ok(result),
        });
    }
    for (request, messages) in captured.iter().zip([history(), expected.clone()]) {
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body)?,
            serde_json::from_slice::<Value>(&wire::encode_request("test-model", 256, &messages)?)?
        );
    }
    expected.push(Message::Assistant(wire::decode_response(&submit)?));
    assert_eq!(
        report,
        RunReport {
            history: expected,
            outcome: RunOutcome::Submitted {
                answer: "done".into()
            }
        }
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_loop_preserves_history_and_classifies_inference_failures_without_retry() -> TestResult
{
    for (reply, expected_kind) in [
        (Reply::Disconnect, ModelFailureKind::Transport),
        (
            Reply::Http {
                status: 503,
                body: vec![],
            },
            ModelFailureKind::HttpStatus(503),
        ),
        (
            Reply::Http {
                status: 200,
                body: b"invalid JSON".to_vec(),
            },
            ModelFailureKind::Json,
        ),
    ] {
        let commands = command_response()?;
        let (endpoint, server) = fake_sequence(vec![
            Reply::Http {
                status: 200,
                body: commands.clone(),
            },
            reply,
        ])
        .await?;
        let mut client = InferenceClient::new(config(endpoint))?;
        let mut executor = MemoryExecutor::default();
        let report = crate::harness::async_driver::run(
            "system".into(),
            "task".into(),
            10,
            &mut client,
            &mut executor,
        )
        .await;
        let captured = server.await??;
        assert_eq!(captured.len(), 2);
        assert!(captured.iter().all(|request| !request.extra_connection));
        assert_eq!(executor.calls.len(), 1);
        let mut expected = history();
        expected.push(Message::Assistant(wire::decode_response(&commands)?));
        expected.push(Message::Tool {
            call_id: id("call-1")?,
            result: Ok(fixed_result()),
        });
        assert_eq!(report.history, expected);
        match report.outcome {
            RunOutcome::ModelFailure(error) => {
                assert_eq!(error.kind, expected_kind);
                assert_eq!(
                    error.completion_unknown(),
                    expected_kind == ModelFailureKind::Transport
                );
                assert!(error.diagnostic.is_some());
            }
            other => return Err(format!("unexpected outcome: {other:?}").into()),
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_loop_stops_for_command_failure_protocol_error_and_turn_limit() -> TestResult {
    for (mixed, failure, expected_calls, outcome) in [
        (false, None, 2, RunOutcome::ModelTurnLimit),
        (
            true,
            None,
            0,
            RunOutcome::ProtocolError(ProtocolError::MixedSubmissionAndCommands),
        ),
        (
            false,
            Some(lost_reply()),
            1,
            RunOutcome::CommandClientFailure {
                call_id: id("call-1")?,
                error: lost_reply(),
            },
        ),
    ] {
        let (endpoint, server) = fake_server(Reply::Http {
            status: 200,
            body: batch_response(mixed)?,
        })
        .await?;
        let mut client = InferenceClient::new(config(endpoint))?;
        let mut executor = MemoryExecutor {
            failure: failure.clone(),
            ..Default::default()
        };
        let report = crate::harness::async_driver::run(
            "system".into(),
            "task".into(),
            1,
            &mut client,
            &mut executor,
        )
        .await;
        assert!(!server.await??.extra_connection);
        assert_eq!(executor.calls.len(), expected_calls);
        assert_eq!(report.outcome, outcome);
        if let Some(error) = failure {
            assert_eq!(
                report.history.last(),
                Some(&Message::Tool {
                    call_id: id("call-1")?,
                    result: Err(error)
                })
            );
        }
    }
    Ok(())
}
