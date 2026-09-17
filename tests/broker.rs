//! Real HTTP and the production broker, with a controlled target. No command
//! execution, container runtime, host shell, or detached test tasks.
use std::{
    collections::VecDeque, future::Future, io, net::SocketAddr, num::NonZeroUsize, sync::Arc,
    time::Duration,
};
use thorough_but_unreliable::{
    broker::{self, Config, StopReason},
    command_protocol,
    harness::{
        async_driver::CommandExecutor,
        command::{CommandClient, CommandConfig},
        types::{CommandCall, ToolCallId},
    },
    target::*,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, oneshot},
    time::timeout,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Calls = Arc<Mutex<Vec<CommandRequest>>>;

enum Action {
    Outcome(ExecutionOutcome),
    WrongSequence,
    Wait {
        started: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
        finished: oneshot::Sender<()>,
    },
    Hang,
    Unwind,
}
struct Fake {
    calls: Calls,
    actions: VecDeque<Action>,
}
impl Fake {
    fn new(actions: impl IntoIterator<Item = Action>) -> (Self, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: calls.clone(),
                actions: actions.into_iter().collect(),
            },
            calls,
        )
    }
}
fn completed() -> ExecutionOutcome {
    ExecutionOutcome::Completed {
        output: CommandOutput {
            stdout: CapturedOutput {
                bytes: vec![0, 255, 10],
                truncated: true,
            },
            stderr: CapturedOutput {
                bytes: b"stderr".to_vec(),
                truncated: false,
            },
        },
        completion: ProcessCompletion::Exited {
            code: 7,
            source: CompletionSource::ParentObserved,
        },
        session_state: SessionState::Ready,
    }
}
impl TargetSession for Fake {
    async fn execute(&mut self, request: CommandRequest) -> ExecutionReport {
        let mut sequence = request.sequence;
        self.calls.lock().await.push(request);
        let outcome = match self.actions.pop_front() {
            Some(Action::Outcome(outcome)) => outcome,
            Some(Action::WrongSequence) => {
                sequence = CommandSequence::new(0);
                completed()
            }
            Some(Action::Wait {
                started,
                release,
                finished,
            }) => {
                let _ = started.send(());
                let _ = release.await;
                let _ = finished.send(());
                completed()
            }
            Some(Action::Hang) => std::future::pending().await,
            // Fault injection only: adapters must return errors in production.
            Some(Action::Unwind) => std::panic::resume_unwind(Box::new("injected adapter unwind")),
            None => completed(),
        };
        ExecutionReport { sequence, outcome }
    }
}

fn config() -> TestResult<Config> {
    Ok(Config::new(
        NonZeroUsize::new(128).ok_or("zero")?,
        NonZeroUsize::new(4096).ok_or("zero")?,
        NonZeroUsize::new(8).ok_or("zero")?,
        Duration::from_millis(500),
        Duration::from_secs(2),
        Duration::from_secs(1),
    )?)
}
async fn case<S, F, Fut, T>(session: S, config: Config, exercise: F) -> TestResult<(StopReason, T)>
where
    S: TargetSession + Send,
    F: FnOnce(SocketAddr, oneshot::Sender<()>) -> Fut,
    Fut: Future<Output = TestResult<T>>,
{
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (stop, stopped) = oneshot::channel();
        let server = async {
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                broker::serve(listener, session, config, async {
                    let _ = stopped.await;
                })
                .await?,
            )
        };
        Ok(tokio::try_join!(server, exercise(address, stop))?)
    })
    .await?
}
fn request(sequence: u64) -> String {
    let body = format!(r#"{{"sequence":{sequence},"command":"id"}}"#);
    format!(
        "POST /v1/command HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}
async fn read(stream: &mut TcpStream) -> TestResult<(u16, Vec<u8>)> {
    let mut bytes = Vec::new();
    stream.take(16384).read_to_end(&mut bytes).await?;
    let boundary = bytes
        .windows(4)
        .position(|b| b == b"\r\n\r\n")
        .ok_or("no response headers")?;
    let headers = std::str::from_utf8(bytes.get(..boundary).ok_or("header range")?)?;
    let status = headers
        .split_whitespace()
        .nth(1)
        .ok_or("no status")?
        .parse()?;
    Ok((
        status,
        bytes.get(boundary + 4..).ok_or("body range")?.to_vec(),
    ))
}
async fn raw(address: SocketAddr, request: &str) -> TestResult<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect(address).await?;
    stream.write_all(request.as_bytes()).await?;
    read(&mut stream).await
}
fn client(address: SocketAddr) -> TestResult<CommandClient> {
    Ok(CommandClient::new(CommandConfig {
        command_url: format!("http://{address}/v1/command"),
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(4),
        max_request_bytes: 128,
        max_response_bytes: 4096,
    })?)
}
fn call(command: &str) -> TestResult<CommandCall> {
    Ok(CommandCall {
        id: ToolCallId::try_from("tool".to_owned())?,
        command: command.into(),
    })
}

#[tokio::test]
async fn real_client_preserves_reports_and_reuses_only_ready_sessions() -> TestResult {
    let rejection = ExecutionOutcome::NotStarted(StartFailure::Rejected(CommandRejection {
        kind: RejectionKind::InvalidCommand,
        diagnostic: Some("NUL".into()),
    }));
    let (fake, calls) = Fake::new([Action::Outcome(rejection.clone())]);
    let (reason, ()) = case(fake, config()?, |address, stop| async move {
        let mut client = client(address)?;
        assert_eq!(client.execute(&call("\0")?).await?.outcome, rejection);
        // Consecutive connections also exercise admission after response writes.
        for sequence in 2..22 {
            let report = client.execute(&call(" printf 'λ'\n")?).await?;
            assert_eq!(
                report,
                ExecutionReport {
                    sequence: CommandSequence::new(sequence),
                    outcome: completed()
                }
            );
        }
        drop(stop);
        Ok(())
    })
    .await?;
    assert_eq!(reason, StopReason::Shutdown);
    let calls = calls.lock().await;
    assert_eq!(calls.len(), 21);
    assert_eq!(calls.first().ok_or("no first call")?.command, "\0");
    assert!(calls.iter().skip(1).all(|c| c.command == " printf 'λ'\n"));
    Ok(())
}

#[tokio::test]
async fn invalid_http_and_sequences_never_dispatch_or_advance() -> TestResult {
    let (fake, calls) = Fake::new([]);
    let (reason, ()) = case(fake, config()?, |address, stop| async move {
        let valid = request(1);
        let malformed = "POST /v1/command HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        for (request, expected) in [
            (valid.replacen("POST", "GET", 1), 405),
            (valid.replacen("/v1/command", "/v1/command?target=other", 1), 404),
            (valid.replacen("application/json", "text/plain", 1), 415),
            (valid.replacen("application/json", "application/json; charset=utf-8", 1), 415),
            (valid.replacen("Content-Type:", "Content-Type: application/json\r\nContent-Type:", 1), 415),
            (valid.replacen("Host:", "Content-Encoding: gzip\r\nHost:", 1), 415),
            (valid.replacen("Host:", "Expect: 100-continue\r\nHost:", 1), 417),
            (valid.replacen("HTTP/1.1", "HTTP/1.0", 1), 505),
            (malformed.to_owned(), 400),
            ("POST /v1/command HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 129\r\n\r\n".into(), 413),
            (format!("POST /v1/command HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n81\r\n{}\r\n0\r\n\r\n", "x".repeat(129)), 413),
            (request(0), 409), (request(2), 409),
        ] {
            assert_eq!(raw(address, &request).await?.0, expected, "{request}");
        }
        assert_eq!(raw(address, &valid).await?.0, 200);
        assert_eq!(raw(address, &valid).await?.0, 409);
        assert_eq!(raw(address, &request(2)).await?.0, 200);
        drop(stop);
        Ok(())
    }).await?;
    assert_eq!(reason, StopReason::Shutdown);
    assert_eq!(
        calls
            .lock()
            .await
            .iter()
            .map(|c| c.sequence.get())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_commands_are_rejected_without_queueing() -> TestResult {
    let (started, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let (finished, finish) = oneshot::channel();
    let (fake, calls) = Fake::new([Action::Wait {
        started,
        release: released,
        finished,
    }]);
    let (reason, ()) = case(fake, config()?, |address, stop| async move {
        let mut first = TcpStream::connect(address).await?;
        first.write_all(request(1).as_bytes()).await?;
        start.await?;
        assert_eq!(raw(address, &request(1)).await?.0, 409);
        assert_eq!(raw(address, &request(2)).await?.0, 409);
        release.send(()).map_err(|_| "target cancelled")?;
        finish.await?;
        assert_eq!(read(&mut first).await?.0, 200);
        assert_eq!(raw(address, &request(2)).await?.0, 200);
        drop(stop);
        Ok(())
    })
    .await?;
    assert_eq!(reason, StopReason::Shutdown);
    assert_eq!(calls.lock().await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn disconnected_client_does_not_cancel_target_execution() -> TestResult {
    let (started, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let (finished, finish) = oneshot::channel();
    let (fake, calls) = Fake::new([Action::Wait {
        started,
        release: released,
        finished,
    }]);
    let (reason, ()) = case(fake, config()?, |address, stop| async move {
        let mut first = TcpStream::connect(address).await?;
        first.write_all(request(1).as_bytes()).await?;
        start.await?;
        drop(first);
        // The adapter is still live. A second HTTP exchange gives the server
        // an opportunity to observe EOF while execution remains blocked.
        assert_eq!(raw(address, &request(2)).await?.0, 409);
        release
            .send(())
            .map_err(|_| "target cancelled on disconnect")?;
        finish.await?;
        drop(stop);
        Ok(())
    })
    .await?;
    assert_eq!(reason, StopReason::DeliveryFailed);
    assert_eq!(calls.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn shutdown_drains_accepted_execution_and_report() -> TestResult {
    let (started, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let (finished, finish) = oneshot::channel();
    let (fake, calls) = Fake::new([Action::Wait {
        started,
        release: released,
        finished,
    }]);
    let (reason, ()) = case(fake, config()?, |address, stop| async move {
        let mut first = TcpStream::connect(address).await?;
        first.write_all(request(1).as_bytes()).await?;
        start.await?;
        stop.send(()).map_err(|_| "broker stopped early")?;
        tokio::task::yield_now().await;
        release.send(()).map_err(|_| "shutdown cancelled target")?;
        finish.await?;
        assert_eq!(read(&mut first).await?.0, 200);
        Ok(())
    })
    .await?;
    assert_eq!(reason, StopReason::Shutdown);
    assert_eq!(calls.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn terminal_reports_stop_the_broker_after_delivery() -> TestResult {
    let unavailable = ExecutionOutcome::NotStarted(StartFailure::SessionUnusable);
    let deadline = ExecutionOutcome::DeadlineExceeded {
        output: CommandOutput::default(),
        execution_state: ExecutionState::MayStillBeRunning,
    };
    let unwound = ExecutionOutcome::Unknown {
        output: CommandOutput::default(),
        error: ExecutionError {
            kind: ExecutionErrorKind::DependencyPanicked,
            diagnostic: None,
        },
    };
    for (action, expected) in [
        (Action::Outcome(unavailable.clone()), unavailable),
        (Action::Hang, deadline),
        (Action::Unwind, unwound),
    ] {
        let (fake, calls) = Fake::new([action]);
        let (reason, ()) = case(fake, config()?, |address, stop| async move {
            let mut client = client(address)?;
            let report = client.execute(&call("id")?).await?;
            assert_eq!(report.session_state(), SessionState::Unusable);
            assert_eq!(report.outcome, expected);
            assert!(client.execute(&call("must not execute")?).await.is_err());
            drop(stop);
            Ok(())
        })
        .await?;
        assert_eq!(reason, StopReason::SessionUnusable);
        assert_eq!(calls.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn connection_capacity_is_released_after_header_timeout() -> TestResult {
    let (fake, calls) = Fake::new([]);
    let bounds = Config::new(
        NonZeroUsize::new(128).ok_or("zero")?,
        NonZeroUsize::new(4096).ok_or("zero")?,
        NonZeroUsize::MIN,
        Duration::from_millis(500),
        Duration::from_secs(2),
        Duration::from_secs(1),
    )?;
    let (reason, ()) = case(fake, bounds, |address, stop| async move {
        let mut occupying = TcpStream::connect(address).await?;
        occupying.write_all(b"POST /v1/").await?;
        let mut waiting = TcpStream::connect(address).await?;
        waiting.write_all(request(1).as_bytes()).await?;
        // The second connection remains in the TCP backlog. It cannot dispatch
        // while the only application connection slot holds incomplete headers.
        let mut byte = [0];
        assert!(
            timeout(Duration::from_millis(100), waiting.read(&mut byte))
                .await
                .is_err()
        );
        assert_eq!(read(&mut waiting).await?.0, 200);
        drop(stop);
        Ok(())
    })
    .await?;
    assert_eq!(reason, StopReason::Shutdown);
    assert_eq!(calls.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn mismatched_or_oversized_adapter_reports_are_terminal_http_failures() -> TestResult {
    let oversized = ExecutionOutcome::Unknown {
        output: CommandOutput {
            stdout: CapturedOutput {
                bytes: vec![0; 4096],
                truncated: false,
            },
            stderr: CapturedOutput::default(),
        },
        error: ExecutionError {
            kind: ExecutionErrorKind::Transport,
            diagnostic: None,
        },
    };
    for action in [Action::WrongSequence, Action::Outcome(oversized)] {
        let (fake, calls) = Fake::new([action]);
        let (reason, ()) = case(fake, config()?, |address, stop| async move {
            assert_eq!(raw(address, &request(1)).await?, (500, Vec::new()));
            drop(stop);
            Ok(())
        })
        .await?;
        assert_eq!(reason, StopReason::InvalidReport);
        assert_eq!(calls.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn incomplete_headers_and_body_have_deadlines_without_dispatch() -> TestResult {
    let (fake, calls) = Fake::new([]);
    let (reason, ()) = case(fake, config()?, |address, stop| async move {
        let mut header = TcpStream::connect(address).await?;
        header.write_all(b"POST /v1/").await?;
        let mut bytes = Vec::new();
        // Hyper may close or emit a framing error; neither invokes the target.
        let result = header.take(16384).read_to_end(&mut bytes).await;
        if let Err(error) = result { assert_eq!(error.kind(), io::ErrorKind::ConnectionReset); }
        let partial = "POST /v1/command HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 30\r\n\r\n{";
        assert_eq!(raw(address, partial).await?.0, 408);
        assert_eq!(raw(address, &request(1)).await?.0, 200);
        drop(stop);
        Ok(())
    }).await?;
    assert_eq!(reason, StopReason::Shutdown);
    assert_eq!(calls.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn chunked_request_at_exact_body_bound_round_trips() -> TestResult {
    let (fake, calls) = Fake::new([]);
    let (reason, ()) = case(fake, config()?, |address, stop| async move {
        let prefix = r#"{"sequence":1,"command":""#;
        let body = format!("{prefix}{}\"}}", "a".repeat(128 - prefix.len() - 2));
        assert_eq!(body.len(), 128);
        let wire = format!("POST /v1/command HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n80\r\n{body}\r\n0\r\n\r\n");
        let (status, bytes) = raw(address, &wire).await?;
        assert_eq!(status, 200);
        assert_eq!(command_protocol::decode_report(&bytes, 4096, CommandSequence::new(1))?.outcome, completed());
        drop(stop);
        Ok(())
    }).await?;
    assert_eq!(reason, StopReason::Shutdown);
    assert_eq!(calls.lock().await.len(), 1);
    Ok(())
}
