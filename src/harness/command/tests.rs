use super::*;
use crate::harness::types::ToolCallId;
use crate::target::*;
use proptest::prelude::*;
use std::io;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
    time::{sleep, timeout},
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn config(command_url: String) -> CommandConfig {
    CommandConfig {
        command_url,
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(2),
        max_request_bytes: 65536,
        max_response_bytes: 65536,
    }
}

fn call() -> Result<CommandCall, crate::harness::types::EmptyToolCallId> {
    Ok(CommandCall {
        id: ToolCallId::try_from("model-id".to_owned())?,
        command: "printf 'a\\n'\n# λ\0".into(),
    })
}

fn completed(sequence: u64, session_state: SessionState) -> ExecutionReport {
    ExecutionReport {
        sequence: CommandSequence::new(sequence),
        outcome: ExecutionOutcome::Completed {
            output: CommandOutput {
                stdout: CapturedOutput {
                    bytes: vec![0, 255, 10],
                    truncated: true,
                },
                stderr: CapturedOutput {
                    bytes: b"diagnostic".to_vec(),
                    truncated: false,
                },
            },
            completion: ProcessCompletion::Exited {
                code: 7,
                source: CompletionSource::GuestReported,
            },
            session_state,
        },
    }
}

struct Reply {
    bytes: Vec<u8>,
    stall: bool,
}
impl Reply {
    fn http(status: u16, headers: &str, body: &[u8]) -> Self {
        let mut bytes = format!(
            "HTTP/1.1 {status} Test\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        Self {
            bytes,
            stall: false,
        }
    }
    fn report(report: &ExecutionReport) -> Result<Self, ProtocolError> {
        Ok(Self::http(
            200,
            "Content-Type: application/json\r\n",
            &protocol::encode_report(report, 65536)?,
        ))
    }
    fn stalled(headers: bool) -> Self {
        Self {
            bytes: if headers {
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx".to_vec()
            } else {
                vec![]
            },
            stall: true,
        }
    }
}

struct Captured {
    line: String,
    headers: String,
    body: Vec<u8>,
}
struct Observed {
    requests: Vec<Captured>,
    extra_connection: bool,
}

async fn fake_broker(
    replies: Vec<Reply>,
) -> io::Result<(String, JoinHandle<io::Result<Observed>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1/command", listener.local_addr()?);
    let server = tokio::spawn(async move {
        timeout(Duration::from_secs(5), async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (socket, _) = listener.accept().await?;
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await?;
                let mut headers = String::new();
                let mut length = 0usize;
                loop {
                    let mut header = String::new();
                    if socket.read_line(&mut header).await? == 0 {
                        return Err(io::Error::other("EOF in headers"));
                    }
                    if header == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().map_err(io::Error::other)?;
                    }
                    headers.push_str(&header);
                    if headers.len() > 16384 || length > 65536 {
                        return Err(io::Error::other("request too large"));
                    }
                }
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await?;
                requests.push(Captured {
                    line,
                    headers,
                    body,
                });
                socket.write_all(&reply.bytes).await?;
                socket.flush().await?;
                if reply.stall {
                    sleep(Duration::from_millis(250)).await;
                }
                // Dropping the socket also simulates abrupt EOF for partial replies.
            }
            let extra_connection = timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_ok();
            Ok(Observed {
                requests,
                extra_connection,
            })
        })
        .await
        .map_err(io::Error::other)?
    });
    Ok((endpoint, server))
}

proptest! {
    #[test]
    fn response_accumulation_preserves_bytes_and_never_mutates_on_rejection(
        before in prop::collection::vec(any::<u8>(), 0..128),
        chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..16),
        limit in 0usize..256,
    ) {
        let mut buffer = before;
        for chunk in chunks {
            let old = buffer.clone();
            let result = append_chunk(&mut buffer, &chunk, limit);
            if old.len().saturating_add(chunk.len()) <= limit {
                prop_assert!(result.is_ok());
                prop_assert_eq!(&buffer, &[old, chunk].concat());
                prop_assert!(buffer.len() <= limit);
            } else {
                prop_assert_eq!(result.err().map(|error| error.kind), Some(Kind::ResponseTooLarge { limit }));
                prop_assert_eq!(&buffer, &old);
            }
        }
    }
}

#[tokio::test]
async fn ordered_round_trips_preserve_commands_reports_and_exact_limits() -> TestResult {
    let reports = vec![
        completed(1, SessionState::Ready),
        ExecutionReport {
            sequence: CommandSequence::new(2),
            outcome: ExecutionOutcome::NotStarted(StartFailure::Rejected(CommandRejection {
                kind: RejectionKind::InvalidCommand,
                diagnostic: None,
            })),
        },
        ExecutionReport {
            sequence: CommandSequence::new(3),
            outcome: ExecutionOutcome::DeadlineExceeded {
                output: CommandOutput::default(),
                execution_state: ExecutionState::ConfirmedStopped {
                    session_state: SessionState::Ready,
                },
            },
        },
        completed(4, SessionState::Ready),
    ];
    let (endpoint, server) = fake_broker(
        reports
            .iter()
            .map(Reply::report)
            .collect::<Result<_, _>>()?,
    )
    .await?;
    let call = call()?;
    let mut settings = config(endpoint);
    settings.max_request_bytes =
        protocol::encode_command(CommandSequence::new(1), &call.command, 65536)?.len();
    settings.max_response_bytes =
        protocol::encode_report(&completed(1, SessionState::Ready), 65536)?.len();
    let mut client = CommandClient::new(settings)?;
    // An unpolled future performs no work and consumes no sequence.
    drop(client.execute(&call));
    for report in &reports {
        assert_eq!(&client.execute(&call).await?, report);
    }
    let observed = server.await??;
    assert!(!observed.extra_connection);
    assert_eq!(observed.requests.len(), reports.len());
    for (request, report) in observed.requests.iter().zip(&reports) {
        assert_eq!(request.line, "POST /v1/command HTTP/1.1\r\n");
        let headers = request.headers.to_ascii_lowercase();
        assert!(headers.contains("content-type: application/json\r\n"));
        assert!(headers.contains("accept: application/json\r\n"));
        assert_eq!(
            protocol::decode_request(&request.body, 65536)?,
            CommandRequest {
                sequence: report.sequence,
                command: call.command.clone()
            }
        );
    }
    Ok(())
}

#[tokio::test]
async fn unusable_reports_are_returned_intact_and_prevent_further_dispatch() -> TestResult {
    for report in [
        completed(1, SessionState::Unusable),
        ExecutionReport {
            sequence: CommandSequence::new(1),
            outcome: ExecutionOutcome::NotStarted(StartFailure::SessionUnusable),
        },
        ExecutionReport {
            sequence: CommandSequence::new(1),
            outcome: ExecutionOutcome::DeadlineExceeded {
                output: CommandOutput::default(),
                execution_state: ExecutionState::MayStillBeRunning,
            },
        },
        ExecutionReport {
            sequence: CommandSequence::new(1),
            outcome: ExecutionOutcome::Unknown {
                output: CommandOutput::default(),
                error: ExecutionError {
                    kind: ExecutionErrorKind::Transport,
                    diagnostic: None,
                },
            },
        },
    ] {
        let (endpoint, server) = fake_broker(vec![Reply::report(&report)?]).await?;
        let mut client = CommandClient::new(config(endpoint))?;
        assert_eq!(client.execute(&call()?).await?, report);
        assert_eq!(
            client.execute(&call()?).await.err().map(|error| error.kind),
            Some(Kind::SessionUnavailable)
        );
        assert!(!server.await??.extra_connection);
    }
    Ok(())
}

#[tokio::test]
async fn invalid_replies_and_http_failures_are_terminal_and_never_retried() -> TestResult {
    let body = protocol::encode_report(&completed(1, SessionState::Ready), 65536)?;
    let mut cases = vec![
        (
            Reply::http(201, "Content-Type: application/json\r\n", &body),
            Kind::HttpStatus(201),
        ),
        (Reply::http(503, "", b""), Kind::HttpStatus(503)),
        (
            Reply {
                bytes: vec![],
                stall: false,
            },
            Kind::Transport,
        ),
        (
            Reply::http(200, "Content-Type: application/json\r\n", b"bad JSON"),
            Kind::InvalidResponse,
        ),
        (
            Reply::report(&completed(2, SessionState::Ready))?,
            Kind::SequenceMismatch {
                expected: CommandSequence::new(1),
                received: CommandSequence::new(2),
            },
        ),
    ];
    for headers in [
        "",
        "Content-Type: text/plain\r\n",
        "Content-Type: application/json\r\nContent-Type: application/json\r\n",
        "Content-Type: application/json; charset=utf-8\r\n",
    ] {
        cases.push((Reply::http(200, headers, &body), Kind::InvalidResponse));
    }
    // Following this redirect would reach the separately monitored listener.
    let redirect = TcpListener::bind("127.0.0.1:0").await?;
    cases.push((
        Reply::http(
            307,
            &format!("Location: http://{}/v1/command\r\n", redirect.local_addr()?),
            b"",
        ),
        Kind::HttpStatus(307),
    ));
    for (reply, expected) in cases {
        let (endpoint, server) = fake_broker(vec![reply]).await?;
        let mut client = CommandClient::new(config(endpoint))?;
        let error = client
            .execute(&call()?)
            .await
            .err()
            .ok_or("expected failure")?;
        assert_eq!(error.kind, expected);
        assert!(error.completion_unknown());
        assert_eq!(
            client.execute(&call()?).await.err().map(|error| error.kind),
            Some(Kind::SessionUnavailable)
        );
        assert!(!server.await??.extra_connection);
    }
    assert!(
        timeout(Duration::from_millis(100), redirect.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn response_limit_covers_announced_chunked_and_eof_bodies() -> TestResult {
    for bytes in [
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n8\r\n12345678\r\n8\r\n12345678\r\n0\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n12345678901".to_vec(),
    ] {
        let (endpoint, server) = fake_broker(vec![Reply { bytes, stall: false }]).await?;
        let mut settings = config(endpoint);
        settings.max_response_bytes = 10;
        let mut client = CommandClient::new(settings)?;
        assert_eq!(client.execute(&call()?).await.err().map(|error| error.kind), Some(Kind::ResponseTooLarge { limit: 10 }));
        assert_eq!(client.execute(&call()?).await.err().map(|error| error.kind), Some(Kind::SessionUnavailable));
        assert!(!server.await??.extra_connection);
    }
    Ok(())
}

#[tokio::test]
async fn timeout_and_cancellation_during_headers_or_body_prevent_reuse() -> TestResult {
    for cancel in [false, true] {
        for headers in [false, true] {
            let (endpoint, server) = fake_broker(vec![Reply::stalled(headers)]).await?;
            let mut settings = config(endpoint);
            if !cancel {
                settings.request_timeout = Duration::from_millis(100);
            }
            let mut client = CommandClient::new(settings)?;
            let call = call()?;
            if cancel {
                assert!(
                    timeout(Duration::from_millis(100), client.execute(&call))
                        .await
                        .is_err()
                );
            } else {
                let error = client
                    .execute(&call)
                    .await
                    .err()
                    .ok_or("expected timeout")?;
                assert_eq!(error.kind, Kind::Transport);
                assert!(error.completion_unknown());
            }
            assert_eq!(
                client.execute(&call).await.err().map(|error| error.kind),
                Some(Kind::SessionUnavailable)
            );
            let observed = server.await??;
            assert_eq!(observed.requests.len(), 1);
            assert!(!observed.extra_connection);
        }
    }
    Ok(())
}

#[tokio::test]
async fn oversized_request_is_rejected_before_connection_and_ends_session() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut settings = config(format!("http://{}/v1/command", listener.local_addr()?));
    let call = call()?;
    let limit = protocol::encode_command(CommandSequence::new(1), &call.command, 65536)?
        .len()
        .checked_sub(1)
        .ok_or("empty body")?;
    settings.max_request_bytes = limit;
    let mut client = CommandClient::new(settings)?;
    let error = client
        .execute(&call)
        .await
        .err()
        .ok_or("expected rejection")?;
    assert_eq!(error.kind, Kind::RequestTooLarge { limit });
    assert!(!error.completion_unknown());
    assert_eq!(
        client.execute(&call).await.err().map(|error| error.kind),
        Some(Kind::SessionUnavailable)
    );
    assert!(
        timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[test]
fn invalid_configuration_is_rejected() {
    for url in [
        "invalid",
        "file:///v1/command",
        "http://localhost/wrong",
        "http://user:secret@localhost/v1/command",
        "http://localhost/v1/command?x=1",
        "http://localhost/v1/command#fragment",
    ] {
        assert!(matches!(
            CommandClient::new(config(url.into())),
            Err(CommandClientError {
                kind: Kind::Configuration,
                ..
            })
        ));
    }
    for index in 0..6 {
        let mut settings = config("http://localhost/v1/command".into());
        match index {
            0 => settings.max_request_bytes = 0,
            1 => settings.max_response_bytes = 0,
            2 => settings.connect_timeout = Duration::ZERO,
            3 => settings.request_timeout = Duration::ZERO,
            4 => settings.connect_timeout = Duration::MAX,
            _ => settings.request_timeout = Duration::MAX,
        }
        assert!(matches!(
            CommandClient::new(settings),
            Err(CommandClientError {
                kind: Kind::Configuration,
                ..
            })
        ));
    }
}

#[test]
fn content_type_is_one_unparameterized_json_media_type() -> TestResult {
    let mut headers = header::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "Application/JSON".parse()?);
    assert!(validate_content_type(&headers).is_ok());
    Ok(())
}
