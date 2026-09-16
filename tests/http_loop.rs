//! Exercise both real HTTP clients through the public async loop. Fixture JSON
//! and expected internal reports are independent of the production codecs.

use std::{collections::VecDeque, io, num::NonZeroU32, time::Duration};

use serde_json::{Value, json};
use thorough_but_unreliable::{
    harness::{
        async_driver,
        command::{CommandClient, CommandConfig},
        inference::{InferenceClient, InferenceConfig},
        types::*,
    },
    target::*,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;
const FIRST: &str = "printf 'first\\n'\n# λ";
const SECOND: &str = "printf 'second\\n'";
const LIMIT: usize = 65536;

enum Reply {
    Json(Value),
    Disconnect,
    Unavailable,
}

enum Exchange {
    Inference { messages: Vec<Value>, reply: Reply },
    Command { request: Value, reply: Reply },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endpoint {
    Inference,
    Command,
}

impl Exchange {
    fn endpoint(&self) -> Endpoint {
        match self {
            Self::Inference { .. } => Endpoint::Inference,
            Self::Command { .. } => Endpoint::Command,
        }
    }
    fn check(self, body: &Value) -> TestResult<Reply> {
        match self {
            Self::Inference { messages, reply } => {
                assert_eq!(body.get("messages"), Some(&json!(messages)));
                assert_eq!(body.get("model"), Some(&json!("fixture-model")));
                assert_eq!(body.get("stream"), Some(&json!(false)));
                assert_eq!(body.get("n"), Some(&json!(1)));
                assert_eq!(body.get("max_tokens"), Some(&json!(256)));
                let names: Vec<_> = body
                    .get("tools")
                    .and_then(Value::as_array)
                    .ok_or("missing tools")?
                    .iter()
                    .map(|tool| tool.pointer("/function/name"))
                    .collect();
                assert_eq!(
                    names,
                    vec![
                        Some(&json!("execute_target_command")),
                        Some(&json!("submit"))
                    ]
                );
                Ok(reply)
            }
            Self::Command { request, reply } => {
                assert_eq!(body, &request);
                Ok(reply)
            }
        }
    }
}

// One coordinator observes both listeners, so the script constrains global
// ordering. No task is detached: try_join drops all work if either side fails.
async fn serve(
    inference: TcpListener,
    command: TcpListener,
    mut script: VecDeque<Exchange>,
    mut done: oneshot::Receiver<()>,
) -> TestResult {
    loop {
        let (endpoint, socket) = tokio::select! {
            socket = inference.accept() => (Endpoint::Inference, socket?.0),
            socket = command.accept() => (Endpoint::Command, socket?.0),
            result = &mut done => {
                result?;
                if !script.is_empty() { return Err(format!("loop ended with {} expected requests missing", script.len()).into()); }
                // Keep both listeners open after completion to catch queued or
                // delayed extra requests, including a retry after submission.
                let extra = timeout(Duration::from_millis(100), async {
                    tokio::select! {
                        result = inference.accept() => result.map(|_| Endpoint::Inference),
                        result = command.accept() => result.map(|_| Endpoint::Command),
                    }
                }).await;
                return match extra {
                    Err(_) => Ok(()),
                    Ok(Ok(endpoint)) => Err(format!("unexpected {endpoint:?} connection after completion").into()),
                    Ok(Err(error)) => Err(error.into()),
                };
            }
        };
        let expected = script
            .pop_front()
            .ok_or_else(|| format!("unexpected {endpoint:?} request after script ended"))?;
        if endpoint != expected.endpoint() {
            return Err(
                format!("expected {:?}, received {endpoint:?}", expected.endpoint()).into(),
            );
        }
        let mut socket = BufReader::new(socket);
        let body = read_request(&mut socket, endpoint).await?;
        match expected.check(&body)? {
            Reply::Disconnect => {}
            Reply::Json(value) => {
                let body = serde_json::to_vec(&value)?;
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await?;
                socket.write_all(&body).await?;
                socket.shutdown().await?;
            }
            Reply::Unavailable => {
                socket.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
                socket.shutdown().await?;
            }
        }
    }
}

async fn read_request(socket: &mut BufReader<TcpStream>, endpoint: Endpoint) -> TestResult<Value> {
    let path = match endpoint {
        Endpoint::Inference => "/v1/chat/completions",
        Endpoint::Command => "/v1/command",
    };
    let mut line = String::new();
    // A read cap keeps even a broken client from making the fixture allocate
    // indefinitely; the enclosing scenario timeout bounds all waiting.
    (&mut *socket).take(16384).read_line(&mut line).await?;
    assert_eq!(line, format!("POST {path} HTTP/1.1\r\n"));
    let mut remaining = 16384u64;
    let mut length = None;
    let mut content_type = None;
    let mut accept = None;
    loop {
        let mut line = String::new();
        let read = (&mut *socket).take(remaining).read_line(&mut line).await?;
        if read == 0 || !line.ends_with("\r\n") {
            return Err("incomplete or oversized request headers".into());
        }
        remaining = remaining
            .checked_sub(u64::try_from(read)?)
            .ok_or("header limit exceeded")?;
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').ok_or("invalid header")?;
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.trim().parse::<usize>()?);
        }
        if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.trim().to_owned());
        }
        if name.eq_ignore_ascii_case("accept") {
            accept = Some(value.trim().to_owned());
        }
    }
    assert_eq!(content_type.as_deref(), Some("application/json"));
    assert_eq!(accept.as_deref(), Some("application/json"));
    let length = length.ok_or("missing Content-Length")?;
    if length > LIMIT {
        return Err("request body too large".into());
    }
    let mut body = Vec::new();
    body.try_reserve_exact(length)?;
    body.resize(length, 0);
    socket.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

async fn scenario(script: Vec<Exchange>) -> TestResult<RunReport> {
    timeout(Duration::from_secs(10), async {
        let inference = TcpListener::bind("127.0.0.1:0").await?;
        let command = TcpListener::bind("127.0.0.1:0").await?;
        let mut model = InferenceClient::new(InferenceConfig {
            completion_url: format!("http://{}/v1/chat/completions", inference.local_addr()?),
            model: "fixture-model".into(),
            max_tokens: 256,
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(2),
            max_request_bytes: LIMIT,
            max_response_bytes: LIMIT,
        })?;
        let mut executor = CommandClient::new(CommandConfig {
            command_url: format!("http://{}/v1/command", command.local_addr()?),
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(2),
            max_request_bytes: LIMIT,
            max_response_bytes: LIMIT,
        })?;
        let (done, received) = oneshot::channel();
        let run = async {
            let report =
                async_driver::run("system".into(), "task".into(), 3, &mut model, &mut executor)
                    .await;
            done.send(())
                .map_err(|_| io::Error::other("fake servers stopped early"))?;
            Ok::<_, TestError>(report)
        };
        let (report, ()) =
            tokio::try_join!(run, serve(inference, command, script.into(), received))?;
        Ok::<_, TestError>(report)
    })
    .await?
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})
}
fn batch() -> Value {
    json!({"role":"assistant","content":"Checking both commands.","tool_calls":[
        call("call-z", "execute_target_command", json!({"command":FIRST})),
        call("call-a", "execute_target_command", json!({"command":SECOND})),
    ]})
}
fn submit() -> Value {
    json!({"role":"assistant","content":null,"tool_calls":[call("final", "submit", json!({"answer":"done"}))]})
}
fn model_reply(message: Value) -> Reply {
    Reply::Json(json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":message}]}))
}
fn initial_messages() -> Vec<Value> {
    vec![
        json!({"role":"system","content":"system"}),
        json!({"role":"user","content":"task"}),
    ]
}
fn next_messages(results: Vec<Value>) -> Vec<Value> {
    let mut messages = initial_messages();
    messages.push(batch());
    for (id, result) in ["call-z", "call-a"].into_iter().zip(results) {
        messages.push(json!({"role":"tool","tool_call_id":id,"content":result.to_string()}));
    }
    messages
}
fn initial_exchange() -> Exchange {
    Exchange::Inference {
        messages: initial_messages(),
        reply: model_reply(batch()),
    }
}
fn command_exchange(sequence: u64, command: &str, reply: Reply) -> Exchange {
    Exchange::Command {
        request: json!({"sequence":sequence,"command":command}),
        reply,
    }
}
fn history(reports: Vec<ExecutionReport>) -> TestResult<Vec<Message>> {
    let mut history = vec![
        Message::System("system".into()),
        Message::User("task".into()),
        Message::Assistant(AssistantResponse {
            text: Some("Checking both commands.".into()),
            tool_calls: vec![
                ToolCall {
                    id: "call-z".into(),
                    tool: Tool::ExecuteTargetCommand {
                        command: FIRST.into(),
                    },
                },
                ToolCall {
                    id: "call-a".into(),
                    tool: Tool::ExecuteTargetCommand {
                        command: SECOND.into(),
                    },
                },
            ],
        }),
    ];
    for (id, report) in ["call-z", "call-a"].into_iter().zip(reports) {
        history.push(Message::Tool {
            call_id: ToolCallId::try_from(id.to_owned())?,
            result: Ok(report),
        });
    }
    Ok(history)
}
fn assert_submitted(report: RunReport, reports: Vec<ExecutionReport>) -> TestResult {
    let mut expected = history(reports)?;
    expected.push(Message::Assistant(AssistantResponse {
        text: None,
        tool_calls: vec![ToolCall {
            id: "final".into(),
            tool: Tool::Submit {
                answer: "done".into(),
            },
        }],
    }));
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

// Each fixture states the broker JSON, internal report, and model projection
// separately. Never generate these expectations using the code under test.
struct Fixture {
    wire: Value,
    report: ExecutionReport,
    view: Value,
}
fn first() -> Fixture {
    Fixture {
        wire: json!({"sequence":1,"outcome":{"kind":"completed","output":{"stdout":{"hex":"00ff0a","truncated":true},"stderr":{"hex":"7761726e","truncated":false}},"completion":{"kind":"exited","code":7,"source":"guest_reported"},"session_state":"ready"}}),
        report: ExecutionReport {
            sequence: CommandSequence::new(1),
            outcome: ExecutionOutcome::Completed {
                output: CommandOutput {
                    stdout: CapturedOutput {
                        bytes: vec![0, 255, 10],
                        truncated: true,
                    },
                    stderr: CapturedOutput {
                        bytes: b"warn".to_vec(),
                        truncated: false,
                    },
                },
                completion: ProcessCompletion::Exited {
                    code: 7,
                    source: CompletionSource::GuestReported,
                },
                session_state: SessionState::Ready,
            },
        },
        view: json!({"sequence":1,"session_state":"ready","outcome":{"kind":"completed","output":{"stdout":{"encoding":"hex","data":"00ff0a","truncated":true},"stderr":{"encoding":"utf8","data":"warn","truncated":false}},"completion":{"kind":"exited","code":7,"source":"guest_reported"}}}),
    }
}
fn second() -> TestResult<Fixture> {
    Ok(Fixture {
        wire: json!({"sequence":2,"outcome":{"kind":"completed","output":{"stdout":{"hex":"6f6b0a","truncated":false},"stderr":{"hex":"","truncated":true}},"completion":{"kind":"signaled","signal":9,"source":"parent_observed"},"session_state":"ready"}}),
        report: ExecutionReport {
            sequence: CommandSequence::new(2),
            outcome: ExecutionOutcome::Completed {
                output: CommandOutput {
                    stdout: CapturedOutput {
                        bytes: b"ok\n".to_vec(),
                        truncated: false,
                    },
                    stderr: CapturedOutput {
                        bytes: vec![],
                        truncated: true,
                    },
                },
                completion: ProcessCompletion::Signaled {
                    signal: NonZeroU32::new(9).ok_or("zero signal")?,
                    source: CompletionSource::ParentObserved,
                },
                session_state: SessionState::Ready,
            },
        },
        view: json!({"sequence":2,"session_state":"ready","outcome":{"kind":"completed","output":{"stdout":{"encoding":"utf8","data":"ok\n","truncated":false},"stderr":{"encoding":"utf8","data":"","truncated":true}},"completion":{"kind":"signaled","signal":9,"source":"parent_observed"}}}),
    })
}

#[tokio::test]
async fn both_clients_complete_a_batch_preserve_results_and_submit() -> TestResult {
    let first = first();
    let second = second()?;
    let report = scenario(vec![
        initial_exchange(),
        command_exchange(1, FIRST, Reply::Json(first.wire)),
        command_exchange(2, SECOND, Reply::Json(second.wire)),
        Exchange::Inference {
            messages: next_messages(vec![first.view, second.view]),
            reply: model_reply(submit()),
        },
    ])
    .await?;
    assert_submitted(report, vec![first.report, second.report])
}

#[tokio::test]
async fn broker_disconnect_stops_the_batch_and_all_further_inference() -> TestResult {
    let report = scenario(vec![
        initial_exchange(),
        command_exchange(1, FIRST, Reply::Disconnect),
    ])
    .await?;
    let RunOutcome::CommandClientFailure { call_id, error } = &report.outcome else {
        return Err(format!("unexpected outcome: {:?}", report.outcome).into());
    };
    assert_eq!(call_id.as_str(), "call-z");
    assert_eq!(error.kind, CommandClientErrorKind::Transport);
    assert!(error.completion_unknown());
    let mut expected = history(vec![])?;
    expected.push(Message::Tool {
        call_id: call_id.clone(),
        result: Err(error.clone()),
    });
    assert_eq!(report.history, expected);
    Ok(())
}

#[tokio::test]
async fn unusable_report_preserves_partial_output_and_stops_the_batch() -> TestResult {
    let wire = json!({"sequence":1,"outcome":{"kind":"unknown","output":{"stdout":{"hex":"ff00","truncated":true},"stderr":{"hex":"","truncated":false}},"error":{"kind":"transport","diagnostic":"guest channel lost"}}});
    let report = scenario(vec![
        initial_exchange(),
        command_exchange(1, FIRST, Reply::Json(wire)),
    ])
    .await?;
    assert_eq!(
        report.outcome,
        RunOutcome::TargetSessionUnusable {
            call_id: ToolCallId::try_from("call-z".to_owned())?
        }
    );
    assert_eq!(
        report.history,
        history(vec![ExecutionReport {
            sequence: CommandSequence::new(1),
            outcome: ExecutionOutcome::Unknown {
                output: CommandOutput {
                    stdout: CapturedOutput {
                        bytes: vec![255, 0],
                        truncated: true
                    },
                    stderr: CapturedOutput::default()
                },
                error: ExecutionError {
                    kind: ExecutionErrorKind::Transport,
                    diagnostic: Some("guest channel lost".into())
                },
            }
        }])?
    );
    Ok(())
}

#[tokio::test]
async fn ready_rejection_is_recorded_and_the_batch_continues() -> TestResult {
    let wire = json!({"sequence":1,"outcome":{"kind":"not_started","failure":{"kind":"rejected","rejection":{"kind":"invalid_command","diagnostic":"unsupported input"}}}});
    let view = json!({"sequence":1,"session_state":"ready","outcome":{"kind":"not_started","error":{"kind":"invalid_command","diagnostic":"unsupported input"}}});
    let second = second()?;
    let report = scenario(vec![
        initial_exchange(),
        command_exchange(1, FIRST, Reply::Json(wire)),
        command_exchange(2, SECOND, Reply::Json(second.wire)),
        Exchange::Inference {
            messages: next_messages(vec![view, second.view]),
            reply: model_reply(submit()),
        },
    ])
    .await?;
    assert_submitted(
        report,
        vec![
            ExecutionReport {
                sequence: CommandSequence::new(1),
                outcome: ExecutionOutcome::NotStarted(StartFailure::Rejected(CommandRejection {
                    kind: RejectionKind::InvalidCommand,
                    diagnostic: Some("unsupported input".into()),
                })),
            },
            second.report,
        ],
    )
}

#[tokio::test]
async fn inference_failure_after_commands_preserves_all_completed_results() -> TestResult {
    let first = first();
    let second = second()?;
    let report = scenario(vec![
        initial_exchange(),
        command_exchange(1, FIRST, Reply::Json(first.wire)),
        command_exchange(2, SECOND, Reply::Json(second.wire)),
        Exchange::Inference {
            messages: next_messages(vec![first.view, second.view]),
            reply: Reply::Unavailable,
        },
    ])
    .await?;
    assert_eq!(report.history, history(vec![first.report, second.report])?);
    let RunOutcome::ModelFailure(error) = report.outcome else {
        return Err("expected model failure".into());
    };
    assert_eq!(error.kind, ModelFailureKind::HttpStatus(503));
    Ok(())
}
