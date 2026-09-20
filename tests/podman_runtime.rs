//! Opt-in test executed only inside the disposable Podman smoke VM.
//! Target lifecycle belongs to this fixture, never to the adapter.
use std::{
    collections::BTreeMap,
    num::{NonZeroU32, NonZeroUsize},
    process::Stdio,
    time::Duration,
};
use thorough_but_unreliable::{
    broker,
    harness::{
        async_driver::CommandExecutor,
        command::{CommandClient, CommandConfig},
        types::{CommandCall, ToolCallId},
    },
    target::{
        podman::{Config, Limits, PodmanTargetSession, Settings, SupervisionEvent},
        *,
    },
};
use tokio::{net::TcpListener, process::Command, sync::oneshot, time::timeout};

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

#[tokio::test]
#[ignore = "runs in the experiment service VM through the Nix experiment-service check"]
async fn standalone_experiment() -> TestResult {
    if std::fs::read_to_string("/etc/experiment-smoke")? != "disposable-fixture\n" {
        return Err("not the disposable experiment fixture".into());
    }
    timeout(Duration::from_secs(15), async {
        loop {
            if tokio::net::TcpStream::connect("10.99.2.2:8080")
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    let id = std::fs::read_to_string("/run/experiment/container-id")?
        .trim()
        .to_owned();
    let cgroup =
        std::path::PathBuf::from(std::fs::read_to_string("/run/experiment/cgroup")?.trim());
    if !cgroup.starts_with("/sys/fs/cgroup") || !cgroup.is_dir() {
        return Err("missing target cgroup".into());
    }
    let mut client = CommandClient::new(CommandConfig {
        command_url: "http://10.99.2.2:8080/v1/command".into(),
        connect_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(45),
        max_request_bytes: 32768,
        max_response_bytes: 1048576,
    })?;
    completed(
        execute(
            &mut client,
            1,
            "id -u; id -g; pwd; test ! -e /run/podman/podman.sock",
        )
        .await?,
        0,
        b"1000\n1000\n/work\n",
        b"",
        false,
    )?;
    let report = execute(&mut client, 2, "sleep 300 & printf 'waiting\\n'; wait").await?;
    match report.outcome {
        ExecutionOutcome::DeadlineExceeded {
            output,
            execution_state: ExecutionState::MayStillBeRunning,
        } if output.stdout.bytes == b"waiting\n"
            && !output.stdout.truncated
            && output.stderr == CapturedOutput::default() => {}
        other => return Err(format!("expected uncertain deadline, got {other:?}").into()),
    }
    // Cleanup is performed by the real systemd unit, not this test.
    timeout(Duration::from_secs(20), async {
        loop {
            let status = Command::new("systemctl")
                .args([
                    "show",
                    "experiment-target.service",
                    "--property=ActiveState",
                    "--property=Result",
                ])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await?;
            if !status.status.success() {
                return Err("could not inspect target service".into());
            }
            let status = std::str::from_utf8(&status.stdout)?;
            if status.lines().any(|line| line == "ActiveState=failed") {
                return Err("target service cleanup failed".into());
            }
            if status.lines().any(|line| line == "ActiveState=inactive") {
                if !status.lines().any(|line| line == "Result=success") {
                    return Err("target service did not stop successfully".into());
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, TestError>(())
    })
    .await??;
    if cgroup.try_exists()? {
        return Err("target cgroup still exists after cleanup completed".into());
    }
    if podman_status(&["container", "exists", &id]).await?.code() != Some(1) {
        return Err("target still exists after service cleanup".into());
    }
    if tokio::net::TcpStream::connect("10.99.2.2:8080")
        .await
        .is_ok()
    {
        return Err("broker still listening after terminal session".into());
    }
    println!("experiment service: real command, deadline response, and systemd cleanup verified");
    Ok(())
}
const CAPTURE: usize = 64;

fn positive(value: usize) -> TestResult<NonZeroUsize> {
    NonZeroUsize::new(value).ok_or_else(|| "zero limit".into())
}

async fn execute(
    client: &mut CommandClient,
    sequence: u64,
    command: &str,
) -> TestResult<ExecutionReport> {
    let report = client
        .execute(&CommandCall {
            id: ToolCallId::try_from(format!("command-{sequence}"))?,
            command: command.into(),
        })
        .await?;
    if report.sequence != CommandSequence::new(sequence) {
        return Err(format!("wrong sequence: {report:?}").into());
    }
    println!("command {sequence}: {:?}", report.outcome);
    Ok(report)
}

fn completed(
    report: ExecutionReport,
    code: u8,
    stdout: &[u8],
    stderr: &[u8],
    truncated: bool,
) -> TestResult {
    let expected = ExecutionOutcome::Completed {
        output: CommandOutput {
            stdout: CapturedOutput {
                bytes: stdout.into(),
                truncated,
            },
            stderr: CapturedOutput {
                bytes: stderr.into(),
                truncated,
            },
        },
        completion: ProcessCompletion::RuntimeStatus {
            code,
            source: CompletionSource::ParentObserved,
        },
        session_state: SessionState::Ready,
    };
    if report.outcome != expected {
        return Err(format!("expected {expected:?}, got {:?}", report.outcome).into());
    }
    Ok(())
}

async fn exercise(client: &mut CommandClient) -> TestResult {
    completed(execute(client, 1,
        "id -u; id -g; pwd; printf '%s\\n' \"$ADAPTER_FIXTURE\"; test ! -e /run/podman/podman.sock"
    ).await?, 0, b"1000\n1000\n/work\nexplicit\n", b"", false)?;
    completed(
        execute(
            client,
            2,
            "if read -r line; then exit 90; fi; printf 'eof\\n'; printf '\\000\\377' >&2; exit 7",
        )
        .await?,
        7,
        b"eof\n",
        &[0, 255],
        false,
    )?;
    completed(
        execute(
            client,
            3,
            "printf saved > /work/state; export ONLY_FIRST=yes; cd /",
        )
        .await?,
        0,
        b"",
        b"",
        false,
    )?;
    completed(
        execute(
            client,
            4,
            "pwd; printf '%s\\n' \"${ONLY_FIRST-unset}\"; cat /work/state",
        )
        .await?,
        0,
        b"/work\nunset\nsaved",
        b"",
        false,
    )?;
    completed(
        execute(
            client,
            5,
            "head -c 8192 /dev/zero; head -c 8192 /dev/zero >&2",
        )
        .await?,
        0,
        &[0; CAPTURE],
        &[0; CAPTURE],
        true,
    )?;
    // A command after truncation proves the previous stream was drained and the
    // same session remains usable. The child keeps the output pipes open.
    let report = execute(client, 6, "sleep 300 & printf 'waiting\\n'; wait").await?;
    match report.outcome {
        ExecutionOutcome::DeadlineExceeded {
            output,
            execution_state: ExecutionState::MayStillBeRunning,
        } if output
            == (CommandOutput {
                stdout: CapturedOutput {
                    bytes: b"waiting\n".into(),
                    truncated: false,
                },
                stderr: CapturedOutput::default(),
            }) =>
        {
            Ok(())
        }
        outcome => {
            Err(format!("expected uncertain deadline with partial output, got {outcome:?}").into())
        }
    }
}

async fn podman_status(args: &[&str]) -> TestResult<std::process::ExitStatus> {
    let mut child = Command::new("podman")
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    match timeout(Duration::from_secs(20), child.wait()).await {
        Ok(status) => Ok(status?),
        Err(error) => {
            child.kill().await?;
            child.wait().await?;
            Err(error.into())
        }
    }
}

async fn supervise(
    id: &str,
    cgroup: &std::path::Path,
    notice: oneshot::Receiver<SupervisionEvent>,
) -> TestResult {
    // Even an unexpected notice/channel closure must lead to cleanup first.
    let event = notice.await;
    if !podman_status(&["rm", "--force", "--time", "0", id])
        .await?
        .success()
    {
        return Err("container removal failed".into());
    }
    if podman_status(&["container", "exists", id]).await?.code() != Some(1) {
        return Err("container still exists or absence check failed".into());
    }
    // Container metadata absence alone is insufficient: require the dedicated
    // cgroup to disappear, including the deliberately started descendant.
    timeout(Duration::from_secs(5), async {
        while cgroup.try_exists()? {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok::<_, TestError>(())
    })
    .await??;
    println!("podman runtime cleanup: container absent; target cgroup removed");
    let event = event?;
    if event
        != (SupervisionEvent::SessionUnusable {
            sequence: CommandSequence::new(6),
        })
    {
        return Err(format!("unexpected supervision event: {event:?}").into());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "runs inside the disposable Podman VM via the Nix podman-runtime check"]
async fn real_podman() -> TestResult {
    if std::fs::read_to_string("/etc/podman-runtime-smoke")? != "disposable-fixture\n" {
        return Err("not the disposable Podman fixture VM".into());
    }
    let id = std::fs::read_to_string("/run/podman-runtime-smoke/container-id")?
        .trim()
        .to_owned();
    let config = Config::new(Settings {
        socket_path: "/run/podman/podman.sock".into(),
        container_id: id.clone(),
        uid: 1000,
        gid: 1000,
        shell: "/bin/bash".into(),
        workdir: "/work".into(),
        environment: BTreeMap::from([("ADAPTER_FIXTURE".into(), "explicit".into())]),
        limits: Limits::new(
            positive(4096)?,
            positive(65536)?,
            positive(CAPTURE)?,
            positive(CAPTURE)?,
            NonZeroU32::new(5000).ok_or("zero deadline")?,
        )?,
    })?;
    let cgroup = std::path::PathBuf::from(
        std::fs::read_to_string("/run/podman-runtime-smoke/cgroup")?.trim(),
    );
    if !cgroup.starts_with("/sys/fs/cgroup") || !cgroup.is_dir() {
        return Err("missing target cgroup before trial".into());
    }
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut client = CommandClient::new(CommandConfig {
        command_url: format!("http://{}/v1/command", listener.local_addr()?),
        connect_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(25),
        max_request_bytes: 8192,
        max_response_bytes: 65536,
    })?;
    let broker_config = broker::Config::new(
        positive(8192)?,
        positive(65536)?,
        positive(2)?,
        Duration::from_secs(5),
        Duration::from_secs(10),
        Duration::from_secs(5),
    )?;
    let (notifier, notice) = oneshot::channel();
    let adapter = PodmanTargetSession::new(config, notifier);
    let (stop, stopped) = oneshot::channel();
    let mut stop = Some(stop);
    let server = broker::serve(listener, adapter, broker_config, async {
        let _ = stopped.await;
    });
    let commands = async {
        let result = exercise(&mut client).await;
        // A failed assertion must still drain the broker and trigger supervision.
        if result.is_err()
            && let Some(stop) = stop.take()
        {
            let _ = stop.send(());
        }
        result
    };
    // Join rather than fail-fast: allow supervision to finish on all test errors.
    let (commands, server, cleanup) = timeout(Duration::from_secs(90), async {
        tokio::join!(commands, server, supervise(&id, &cgroup, notice))
    })
    .await?;
    commands?;
    cleanup?;
    if server? != broker::StopReason::SessionUnusable {
        return Err("broker did not terminate the unusable session".into());
    }
    println!("podman runtime assertions: PASS");
    Ok(())
}
