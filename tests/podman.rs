//! Real Unix-socket HTTP, the production adapter, and scripted runtime peers.
//! All futures are joined; no Podman daemon, target command or host shell runs.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    io,
    num::{NonZeroU32, NonZeroUsize},
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use thorough_but_unreliable::target::{
    podman::{Config, Limits, PodmanTargetSession, Settings, SupervisionEvent},
    *,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::oneshot,
    time::{sleep, timeout},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
struct Directory(PathBuf);
impl Directory {
    fn new() -> TestResult<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        for _ in 0..32 {
            let id = NEXT
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| "temporary names exhausted")?;
            let path = std::env::temp_dir().join(format!("pd-{}-{id}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not create test directory".into())
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("s"));
        let _ = std::fs::remove_dir(&self.0);
    }
}
enum Reply {
    Close(Vec<u8>),
    // Confirm the client closes its connection even if the peer doesn't.
    Park {
        bytes: Vec<u8>,
        started: oneshot::Sender<()>,
        closed: oneshot::Sender<()>,
    },
    Delay {
        bytes: Vec<u8>,
        wait: Duration,
    },
}
struct Exchange {
    method: &'static str,
    path: String,
    body: Option<Value>,
    reply: Reply,
}
fn create(command: &str, reply: Reply) -> Exchange {
    Exchange {
        method: "POST",
        path: format!("/containers/{}/exec", "a".repeat(64)),
        body: Some(
            json!({"Cmd":["/bin/sh","-c",command],"User":"1000:1000","WorkingDir":"/work","Env":["PATH=/bin"],
            "AttachStdin":false,"AttachStdout":true,"AttachStderr":true,"Tty":false,"Privileged":false}),
        ),
        reply,
    }
}
fn start(reply: Reply) -> Exchange {
    Exchange {
        method: "POST",
        path: format!("/exec/{}/start", "b".repeat(64)),
        body: Some(json!({"Detach":false,"Tty":false})),
        reply,
    }
}
fn inspect(reply: Reply) -> Exchange {
    Exchange {
        method: "GET",
        path: format!("/exec/{}/json", "b".repeat(64)),
        body: None,
        reply,
    }
}
fn json_response(status: u16, value: Value) -> TestResult<Vec<u8>> {
    let body = serde_json::to_vec(&value)?;
    let mut bytes = format!(
        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend(body);
    Ok(bytes)
}
fn created() -> TestResult<Vec<u8>> {
    json_response(201, json!({"Id":"b".repeat(64)}))
}
fn observed(running: bool, code: u8) -> TestResult<Vec<u8>> {
    json_response(
        200,
        json!({"ID":"b".repeat(64),"ContainerID":"a".repeat(64),"Running":running,"CanRemove":!running,"ExitCode":code}),
    )
}
fn frame(channel: u8, data: &[u8]) -> TestResult<Vec<u8>> {
    let mut bytes = vec![channel, 0, 0, 0];
    bytes.extend(u32::try_from(data.len())?.to_be_bytes());
    bytes.extend(data);
    Ok(bytes)
}
fn upgraded(frames: &[u8]) -> Vec<u8> {
    let mut bytes =
        b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n".to_vec();
    bytes.extend(frames);
    bytes
}
fn request(sequence: u64, command: &str) -> CommandRequest {
    CommandRequest {
        sequence: CommandSequence::new(sequence),
        command: command.into(),
    }
}

async fn check_request(socket: &mut UnixStream, exchange: &Exchange) -> TestResult {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() >= 16384 {
            return Err("request headers exceeded bound".into());
        }
        header.push(socket.read_u8().await?);
    }
    let text = std::str::from_utf8(&header)?;
    assert_eq!(
        text.lines().next(),
        Some(format!("{} {} HTTP/1.1", exchange.method, exchange.path).as_str())
    );
    let mut length = 0usize;
    let mut connection = None;
    let mut host = None;
    let mut upgrade = None;
    for line in text.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse()?,
                "connection" => connection = Some(value.trim()),
                "host" => host = Some(value.trim()),
                "upgrade" => upgrade = Some(value.trim()),
                _ => {}
            }
        }
    }
    assert_eq!(host, Some("localhost"));
    if exchange.path.ends_with("/start") {
        assert_eq!(connection, Some("Upgrade"));
        assert_eq!(upgrade, Some("tcp"));
    } else {
        assert_eq!(connection, Some("close"));
        assert_eq!(upgrade, None);
    }
    if length > 4096 {
        return Err("request body exceeded bound".into());
    }
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await?;
    match &exchange.body {
        Some(expected) => assert_eq!(&serde_json::from_slice::<Value>(&body)?, expected),
        None => assert!(body.is_empty()),
    }
    Ok(())
}
async fn serve(
    listener: UnixListener,
    script: Vec<Exchange>,
    mut done: oneshot::Receiver<()>,
) -> TestResult {
    for exchange in script {
        let (mut socket, _) = tokio::select! {
            biased;
            accepted = listener.accept() => accepted?,
            _ = &mut done => return Err("adapter finished before expected runtime request".into()),
        };
        check_request(&mut socket, &exchange).await?;
        match exchange.reply {
            Reply::Close(bytes) => {
                socket.write_all(&bytes).await?;
            }
            Reply::Park {
                bytes,
                started,
                closed,
            } => {
                socket.write_all(&bytes).await?;
                let _ = started.send(());
                let mut byte = [0];
                match socket.read(&mut byte).await {
                    Ok(0) => {}
                    Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
                    result => return Err(format!("connection not closed: {result:?}").into()),
                }
                let _ = closed.send(());
            }
            Reply::Delay { bytes, wait } => {
                sleep(wait).await;
                if let Err(error) = socket.write_all(&bytes).await
                    && !matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    )
                {
                    return Err(error.into());
                }
            }
        }
    }
    tokio::select! {
        biased;
        _ = listener.accept() => return Err("unexpected extra runtime request or retry".into()),
        _ = &mut done => {},
    }
    assert!(
        timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err(),
        "late runtime traffic"
    );
    Ok(())
}
async fn case<F, Fut>(script: Vec<Exchange>, deadline_ms: u32, exercise: F) -> TestResult
where
    F: FnOnce(PodmanTargetSession, oneshot::Receiver<SupervisionEvent>) -> Fut,
    Fut: Future<Output = TestResult>,
{
    timeout(Duration::from_secs(10), async {
        let directory = Directory::new()?;
        let path = directory.0.join("s");
        let listener = UnixListener::bind(&path)?;
        let config = Config::new(Settings {
            socket_path: path.to_str().ok_or("socket path not UTF-8")?.into(),
            container_id: "a".repeat(64),
            uid: 1000,
            gid: 1000,
            shell: "/bin/sh".into(),
            workdir: "/work".into(),
            environment: BTreeMap::from([("PATH".into(), "/bin".into())]),
            limits: Limits::new(
                NonZeroUsize::new(512).ok_or("zero")?,
                NonZeroUsize::new(1024).ok_or("zero")?,
                NonZeroUsize::new(4).ok_or("zero")?,
                NonZeroUsize::new(4).ok_or("zero")?,
                NonZeroU32::new(deadline_ms).ok_or("zero")?,
            )?,
        })?;
        let (supervisor, notices) = oneshot::channel();
        let adapter = PodmanTargetSession::new(config, supervisor);
        let (done, finished) = oneshot::channel();
        let run = async {
            exercise(adapter, notices).await?;
            let _ = done.send(());
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        };
        tokio::try_join!(serve(listener, script, finished), run)?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}
async fn unusable(
    adapter: &mut PodmanTargetSession,
    notices: oneshot::Receiver<SupervisionEvent>,
) -> TestResult {
    assert_eq!(
        notices.await?,
        SupervisionEvent::SessionUnusable {
            sequence: CommandSequence::new(1)
        }
    );
    assert_eq!(
        adapter.execute(request(2, "must not run")).await.outcome,
        ExecutionOutcome::NotStarted(StartFailure::SessionUnusable)
    );
    Ok(())
}

#[tokio::test]
async fn commands_round_trip_with_prefetched_frames_truncation_and_pending_inspection() -> TestResult
{
    let command = " printf 'λ'\n";
    let mut frames = frame(1, &[0, 255, 1, 2, 3, 4])?;
    frames.extend(frame(2, b"err")?);
    let (started, _) = oneshot::channel();
    let (closed, _) = oneshot::channel();
    case(
        vec![
            create(
                command,
                Reply::Park {
                    bytes: created()?,
                    started,
                    closed,
                },
            ),
            start(Reply::Close(upgraded(&frames))),
            inspect(Reply::Close(observed(true, 0)?)),
            inspect(Reply::Close(observed(false, 137)?)),
            create("next", Reply::Close(created()?)),
            start(Reply::Close(upgraded(&[]))),
            inspect(Reply::Close(observed(false, 0)?)),
        ],
        2000,
        |mut adapter, mut notices| async move {
            let report = adapter.execute(request(1, command)).await;
            assert_eq!(
                report,
                ExecutionReport {
                    sequence: CommandSequence::new(1),
                    outcome: ExecutionOutcome::Completed {
                        output: CommandOutput {
                            stdout: CapturedOutput {
                                bytes: vec![0, 255, 1, 2],
                                truncated: true
                            },
                            stderr: CapturedOutput {
                                bytes: b"err".to_vec(),
                                truncated: false
                            }
                        },
                        completion: ProcessCompletion::RuntimeStatus {
                            code: 137,
                            source: CompletionSource::ParentObserved
                        },
                        session_state: SessionState::Ready,
                    }
                }
            );
            assert_eq!(
                adapter.execute(request(2, "next")).await.session_state(),
                SessionState::Ready
            );
            assert_eq!(notices.try_recv(), Err(oneshot::error::TryRecvError::Empty));
            drop(adapter);
            assert_eq!(notices.await?, SupervisionEvent::SessionDropped);
            Ok(())
        },
    )
    .await
}

#[tokio::test]
async fn local_rejections_and_missing_supervision_do_not_contact_runtime() -> TestResult {
    case(vec![], 1000, |mut adapter, mut notices| async move {
        for command in ["\0".into(), "x".repeat(513)] {
            assert!(matches!(
                adapter.execute(request(1, &command)).await.outcome,
                ExecutionOutcome::NotStarted(StartFailure::Rejected(_))
            ));
        }
        assert_eq!(notices.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        drop(notices);
        assert_eq!(
            adapter.execute(request(1, "id")).await.outcome,
            ExecutionOutcome::NotStarted(StartFailure::SessionUnusable)
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn invalid_and_oversized_json_responses_end_session_without_start_or_retry() -> TestResult {
    let valid = String::from_utf8(created()?)?;
    let chunked = format!(
        "HTTP/1.1 201 Test\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n401\r\n{}\r\n0\r\n\r\n",
        "x".repeat(1025)
    );
    for bytes in [
        valid
            .replacen("application/json", "text/plain", 1)
            .into_bytes(),
        valid
            .replacen(
                "Content-Type:",
                "Content-Type: application/json\r\nContent-Type:",
                1,
            )
            .into_bytes(),
        valid
            .replacen(
                "Content-Type:",
                "Content-Encoding: gzip\r\nContent-Type:",
                1,
            )
            .into_bytes(),
        b"HTTP/1.1 201 Test\r\nContent-Type: application/json\r\nContent-Length: 1025\r\n\r\n"
            .to_vec(),
        chunked.into_bytes(),
        json_response(201, json!({"Id":"wrong"}))?,
        b"HTTP/1.1 307 Redirect\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n"
            .to_vec(),
        b"HTTP/1.1 201 Test\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nx".to_vec(),
    ] {
        case(
            vec![create("id", Reply::Close(bytes))],
            1000,
            |mut adapter, notices| async move {
                assert!(matches!(
                    adapter.execute(request(1, "id")).await.outcome,
                    ExecutionOutcome::NotStarted(StartFailure::Failed(_))
                ));
                unusable(&mut adapter, notices).await
            },
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn invalid_upgrade_or_stream_never_claims_completion() -> TestResult {
    for bytes in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
        b"HTTP/1.1 101 Switching\r\nUpgrade: tcp\r\n\r\n".to_vec(),
        b"HTTP/1.1 101 Switching\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n".to_vec(),
        b"HTTP/1.1 101 Switching\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nUpgrade: tcp\r\n\r\n"
            .to_vec(),
        b"HTTP/1.1 101 Switching\r\nConnection: Upgrade, close\r\nUpgrade: tcp\r\n\r\n".to_vec(),
        upgraded(&[1, 0, 0]),
        upgraded(&[3, 0, 0, 0, 0, 0, 0, 0]),
    ] {
        case(
            vec![
                create("id", Reply::Close(created()?)),
                start(Reply::Close(bytes)),
            ],
            1000,
            |mut adapter, notices| async move {
                assert!(matches!(
                    adapter.execute(request(1, "id")).await.outcome,
                    ExecutionOutcome::Unknown { .. }
                ));
                unusable(&mut adapter, notices).await
            },
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_at_every_io_stage_closes_connections_and_notifies_supervision() -> TestResult
{
    for stage in 0..4 {
        let (started, ready) = oneshot::channel();
        let (closed, closed_rx) = oneshot::channel();
        let bytes = if stage == 2 {
            upgraded(&frame(1, b"part")?)
        } else {
            vec![]
        };
        let parked = Reply::Park {
            bytes,
            started,
            closed,
        };
        let script = match stage {
            0 => vec![create("id", parked)],
            1 | 2 => vec![create("id", Reply::Close(created()?)), start(parked)],
            _ => vec![
                create("id", Reply::Close(created()?)),
                start(Reply::Close(upgraded(&[]))),
                inspect(parked),
            ],
        };
        case(script, 2000, |mut adapter, notices| async move {
            {
                let execution = adapter.execute(request(1, "id")); tokio::pin!(execution);
                tokio::select! {
                    report = &mut execution => return Err(format!("finished before cancellation: {report:?}").into()),
                    result = ready => result?,
                }
            }
            closed_rx.await?;
            assert_eq!(notices.await?, SupervisionEvent::ExecutionCancelled { sequence: CommandSequence::new(1) });
            assert_eq!(adapter.execute(request(2, "no retry")).await.outcome, ExecutionOutcome::NotStarted(StartFailure::SessionUnusable));
            Ok(())
        }).await?;
    }
    Ok(())
}

#[tokio::test]
async fn deadlines_cover_headers_bodies_stream_and_inspection_and_preserve_partial_output()
-> TestResult {
    for stage in 0..5 {
        let (started, _) = oneshot::channel();
        let (closed, closed_rx) = oneshot::channel();
        let bytes = match stage {
            1 => b"HTTP/1.1 201 Test\r\nContent-Type: application/json\r\nContent-Length: 50\r\n\r\n{".to_vec(),
            3 => upgraded(&frame(1, b"part")?),
            _ => vec![],
        };
        let parked = Reply::Park {
            bytes,
            started,
            closed,
        };
        let script = match stage {
            0 | 1 => vec![create("id", parked)],
            2 | 3 => vec![create("id", Reply::Close(created()?)), start(parked)],
            _ => vec![
                create("id", Reply::Close(created()?)),
                start(Reply::Close(upgraded(&frame(1, b"part")?))),
                inspect(parked),
            ],
        };
        case(script, 150, |mut adapter, notices| async move {
            let report = adapter.execute(request(1, "id")).await;
            if stage < 2 {
                assert!(matches!(
                    report.outcome,
                    ExecutionOutcome::NotStarted(StartFailure::Failed(_))
                ));
            } else {
                match report.outcome {
                    ExecutionOutcome::DeadlineExceeded {
                        output,
                        execution_state: ExecutionState::MayStillBeRunning,
                    } => {
                        assert_eq!(
                            output.stdout.bytes,
                            if stage >= 3 { b"part".to_vec() } else { vec![] }
                        );
                    }
                    _ => return Err(format!("expected deadline: {report:?}").into()),
                }
            }
            closed_rx.await?;
            unusable(&mut adapter, notices).await
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn one_deadline_is_shared_across_all_requests() -> TestResult {
    case(
        vec![
            create(
                "id",
                Reply::Delay {
                    bytes: created()?,
                    wait: Duration::from_millis(150),
                },
            ),
            start(Reply::Delay {
                bytes: upgraded(&frame(1, b"part")?),
                wait: Duration::from_millis(150),
            }),
            inspect(Reply::Delay {
                bytes: observed(false, 0)?,
                wait: Duration::from_millis(450),
            }),
        ],
        600,
        |mut adapter, notices| async move {
            assert!(matches!(
                adapter.execute(request(1, "id")).await.outcome,
                ExecutionOutcome::DeadlineExceeded {
                    execution_state: ExecutionState::MayStillBeRunning,
                    ..
                }
            ));
            unusable(&mut adapter, notices).await
        },
    )
    .await
}

#[tokio::test]
async fn chunked_json_and_inspection_failure_preserve_command_output() -> TestResult {
    let body = serde_json::to_vec(&json!({"Id":"b".repeat(64)}))?;
    let mut chunks =
        b"HTTP/1.1 201 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec();
    for byte in body {
        chunks.extend(b"1\r\n");
        chunks.push(byte);
        chunks.extend(b"\r\n");
    }
    chunks.extend(b"0\r\n\r\n");
    let wrong = json_response(
        200,
        json!({"ID":"c".repeat(64),"ContainerID":"a".repeat(64),"Running":false,"CanRemove":true,"ExitCode":0}),
    )?;
    case(
        vec![
            create("id", Reply::Close(chunks)),
            start(Reply::Close(upgraded(&frame(2, &[0, 255])?))),
            inspect(Reply::Close(wrong)),
        ],
        1000,
        |mut adapter, notices| async move {
            match adapter.execute(request(1, "id")).await.outcome {
                ExecutionOutcome::Unknown { output, error } => {
                    assert_eq!(output.stderr.bytes, vec![0, 255]);
                    assert_eq!(error.kind, ExecutionErrorKind::MalformedResponse);
                }
                outcome => return Err(format!("expected correlation failure: {outcome:?}").into()),
            }
            unusable(&mut adapter, notices).await
        },
    )
    .await
}

#[tokio::test]
async fn broker_and_harness_client_use_the_real_adapter() -> TestResult {
    use thorough_but_unreliable::{
        broker,
        harness::{
            async_driver::CommandExecutor,
            command::{CommandClient, CommandConfig},
            types::{CommandCall, ToolCallId},
        },
    };
    use tokio::net::TcpListener;
    case(
        vec![
            create("id", Reply::Close(created()?)),
            start(Reply::Close(upgraded(&frame(1, b"ok")?))),
            inspect(Reply::Close(observed(false, 0)?)),
        ],
        1000,
        |adapter, notices| async move {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let config = broker::Config::new(
                NonZeroUsize::new(1024).ok_or("zero")?,
                NonZeroUsize::new(4096).ok_or("zero")?,
                NonZeroUsize::new(2).ok_or("zero")?,
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(1),
            )?;
            let (stop, stopped) = oneshot::channel();
            let server = async {
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                    broker::serve(listener, adapter, config, async {
                        let _ = stopped.await;
                    })
                    .await?,
                )
            };
            let client = async {
                let mut client = CommandClient::new(CommandConfig {
                    command_url: format!("http://{address}/v1/command"),
                    connect_timeout: Duration::from_secs(1),
                    request_timeout: Duration::from_secs(3),
                    max_request_bytes: 1024,
                    max_response_bytes: 4096,
                })?;
                let report = client
                    .execute(&CommandCall {
                        id: ToolCallId::try_from("tool".to_owned())?,
                        command: "id".into(),
                    })
                    .await?;
                assert_eq!(report.sequence, CommandSequence::new(1));
                match report.outcome {
                    ExecutionOutcome::Completed {
                        output,
                        completion:
                            ProcessCompletion::RuntimeStatus {
                                code: 0,
                                source: CompletionSource::ParentObserved,
                            },
                        session_state: SessionState::Ready,
                    } => assert_eq!(output.stdout.bytes, b"ok"),
                    outcome => return Err(format!("unexpected report: {outcome:?}").into()),
                }
                let _ = stop.send(());
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            };
            let (reason, ()) = tokio::try_join!(server, client)?;
            assert_eq!(reason, broker::StopReason::Shutdown);
            assert_eq!(notices.await?, SupervisionEvent::SessionDropped);
            Ok(())
        },
    )
    .await
}
