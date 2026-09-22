//! Two QEMU guests, a private Ethernet stream, and scripted or live inference.
//! The experiment uses the live image; no command fixture or test agent runs there.
use super::*;
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::net::UnixStream;

const PAIRED_MANIFEST: &str = include_str!("../fixtures/paired-manifest.json");
const PAIRED_REPORT: &str = include_str!("../fixtures/paired-report.json");
const READY: &str = "experiment broker: ready listen=10.99.2.2:8080";

#[path = "local.rs"]
mod local;

enum Inference {
    Scripted,
    Local { socket: String, trial: local::Trial },
}

fn inference_script() -> TestResult<Vec<Exchange>> {
    let expected: Value = serde_json::from_str(PAIRED_REPORT)?;
    let command = expected
        .pointer("/history/2/tool_calls/0/tool/command")
        .and_then(Value::as_str)
        .ok_or("missing command")?;
    let answer = expected
        .pointer("/outcome/answer")
        .ok_or("missing answer")?;
    let call = json!({"role":"assistant","content":null,"tool_calls":[{
        "id":"command","type":"function","function":{
            "name":"execute_target_command","arguments":json!({"command":command}).to_string()
        }
    }]});
    let submit = json!({"role":"assistant","content":null,"tool_calls":[{
        "id":"submit","type":"function","function":{
            "name":"submit","arguments":json!({"answer":answer}).to_string()
        }
    }]});
    let initial = vec![
        json!({"role":"system","content":"system"}),
        json!({"role":"user","content":"task"}),
    ];
    let mut next = initial.clone();
    next.push(call.clone());
    next.push(json!({
        "role":"tool","tool_call_id":"command",
        "content":expected.pointer("/history/3/result/report").ok_or("missing report")?.to_string()
    }));
    let response = |message| {
        Reply::Json(json!({
            "choices":[{"index":0,"finish_reason":"tool_calls","message":message}]
        }))
    };
    Ok(vec![
        Exchange::Inference {
            messages: initial,
            reply: response(call),
        },
        Exchange::Inference {
            messages: next,
            reply: response(submit),
        },
    ])
}

// Keep bytes available on cancellation, bound memory, and detect readiness even
// when the marker spans reads. No logging task outlives its VM supervisor.
async fn capture(
    mut stream: impl AsyncRead + Unpin,
    bytes: &mut Vec<u8>,
    limit: usize,
    mut ready: Option<oneshot::Sender<()>>,
) -> TestResult {
    let mut chunk = [0u8; 4096];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Ok(());
        }
        let next = bytes.len().checked_add(count).ok_or("log size overflow")?;
        if next > limit {
            return Err("VM log limit exceeded".into());
        }
        bytes.try_reserve(count)?;
        bytes.extend_from_slice(chunk.get(..count).ok_or("invalid read size")?);
        if ready.is_some()
            && bytes
                .windows(READY.len())
                .any(|part| part == READY.as_bytes())
            && let Some(sender) = ready.take()
        {
            sender
                .send(())
                .map_err(|_| "harness readiness receiver closed")?;
        }
    }
}

async fn experiment(
    mut child: Child,
    directory: &Path,
    ready: oneshot::Sender<()>,
    stop: oneshot::Receiver<()>,
    monitor: &Path,
    deadline: Duration,
) -> TestResult {
    let mut console = Vec::new();
    let mut stderr = Vec::new();
    let result = timeout(deadline, async {
        let out = child.stdout.take().ok_or("missing experiment stdout")?;
        let err = child.stderr.take().ok_or("missing experiment stderr")?;
        let shutdown = async {
            stop.await?;
            // A private QEMU human monitor controls ACPI poweroff only. Neither
            // guest can access this socket; it is not a guest command channel.
            let mut socket = UnixStream::connect(monitor).await?;
            socket.write_all(b"system_powerdown\n").await?;
            Ok::<_, TestError>(())
        };
        let wait = async { Ok::<_, TestError>(child.wait().await?) };
        let (status, (), (), ()) = tokio::try_join!(
            wait,
            capture(out, &mut console, 4194304, Some(ready)),
            capture(err, &mut stderr, 65536, None),
            shutdown,
        )?;
        if !status.success() {
            return Err("experiment QEMU failed".into());
        }
        Ok::<_, TestError>(())
    })
    .await;
    let cleanup = async {
        if child.try_wait()?.is_none() {
            child.kill().await?;
        }
        child.wait().await?;
        Ok::<_, TestError>(())
    }
    .await;
    std::fs::write(directory.join("console.log"), &console)?;
    std::fs::write(directory.join("stderr.log"), &stderr)?;
    cleanup?;
    result??;
    let console = String::from_utf8_lossy(&console);
    for marker in [
        "experiment-broker.service: Deactivated successfully.",
        "experiment cleanup: container absent; recorded target cgroup absent",
        "experiment-target.service: Deactivated successfully.",
    ] {
        if !console.contains(marker) {
            return Err(format!("missing {marker}; see experiment/console.log").into());
        }
    }
    if console.contains("experiment-target.service: Failed")
        || console.contains("experiment-broker.service: Failed")
    {
        return Err("experiment service shutdown failed".into());
    }
    Ok(())
}

fn qemu(overlay: &Path, memory: &str) -> Command {
    let mut command = Command::new("qemu-system-x86_64");
    command.args([
        "-enable-kvm",
        "-machine",
        "q35",
        "-cpu",
        "host",
        "-m",
        memory,
        "-display",
        "none",
        "-serial",
        "stdio",
        "-no-reboot",
        "-nic",
        "none",
    ]);
    command
        .arg("-drive")
        .arg(format!("file={},format=qcow2,if=virtio", overlay.display()));
    command
}

async fn overlay(image_variable: &str, directory: &Path) -> TestResult<PathBuf> {
    let image = std::fs::canonicalize(
        std::env::var_os(image_variable).ok_or_else(|| format!("missing {image_variable}"))?,
    )?;
    std::fs::create_dir(directory)?;
    let overlay = directory.join("disk.qcow2");
    let mut command = Command::new("qemu-img");
    command
        .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
        .arg(image)
        .arg(&overlay);
    tool(command).await?;
    Ok(overlay)
}

#[tokio::test]
#[ignore = "requires KVM; use the sandboxed Nix paired-vm check"]
async fn paired_vm_smoke() -> TestResult {
    run(Inference::Scripted).await
}

async fn run(inference_mode: Inference) -> TestResult {
    let deadline = Duration::from_secs(match &inference_mode {
        Inference::Scripted => 360,
        Inference::Local { .. } => 1200,
    });
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")?;
    let directory = artifact_directory().await?;
    let harness_dir = directory.join("harness");
    let experiment_dir = directory.join("experiment");
    let harness_disk = overlay("HARNESS_SMOKE_IMAGE", &harness_dir).await?;
    let experiment_disk = overlay("EXPERIMENT_IMAGE", &experiment_dir).await?;
    let config = directory.join("config");
    std::fs::create_dir(&config)?;
    let manifest = match &inference_mode {
        Inference::Scripted => PAIRED_MANIFEST.to_owned(),
        Inference::Local { trial, .. } => trial.manifest().to_owned(),
    };
    std::fs::write(config.join("manifest.json"), manifest)?;
    let iso = directory.join("harness-config.iso");
    let mut mkiso = Command::new("xorrisofs");
    mkiso
        .args([
            "-quiet",
            "-volid",
            "HARNESS_CONFIG",
            "-joliet",
            "-rock",
            "-output",
        ])
        .arg(&iso)
        .arg(&config);
    tool(mkiso).await?;

    // Relative, short socket paths also fit Unix socket limits in Nix builders.
    let link = directory.join("command.sock");
    let monitor = directory.join("monitor.sock");
    let mut experiment_command = qemu(&experiment_disk, "2048");
    experiment_command
        .arg("-monitor")
        .arg(format!("unix:{},server=on,wait=off", monitor.display()));
    experiment_command.arg("-netdev").arg(format!(
        "stream,id=command,server=on,addr.type=unix,addr.path={}",
        link.display()
    ));
    experiment_command.args([
        "-device",
        "virtio-net-pci,netdev=command,mac=52:54:00:99:02:02",
    ]);

    let inference = TcpListener::bind("127.0.0.1:0").await?;
    // Unconnected sentinel: the script rejects any accidental fake-broker use.
    let unused_broker = TcpListener::bind("127.0.0.1:0").await?;
    let forward = match &inference_mode {
        Inference::Scripted => format!("nc 127.0.0.1 {}", inference.local_addr()?.port()),
        Inference::Local { socket, .. } => local::forward(socket)?,
    };
    let mut harness_command = qemu(&harness_disk, "1024");
    harness_command.args(["-monitor", "none"]);
    harness_command.arg("-drive").arg(format!(
        "file={},format=raw,media=cdrom,readonly=on",
        iso.display()
    ));
    harness_command.arg("-netdev").arg(format!(
        "stream,id=command,server=off,addr.type=unix,addr.path={}",
        link.display()
    ));
    harness_command.args([
        "-device",
        "virtio-net-pci,netdev=command,mac=52:54:00:99:02:01",
    ]);
    harness_command.arg("-netdev").arg(format!(
        "user,id=inference,net=10.99.1.0/24,host=10.99.1.254,dns=10.99.1.253,ipv6=off,restrict=on,guestfwd=tcp:10.99.1.1:11434-cmd:{forward}"
    ));
    harness_command.args([
        "-device",
        "virtio-net-pci,netdev=inference,mac=52:54:00:99:01:02",
    ]);
    let exchanges = inference_script()?;
    let (ready, readiness) = oneshot::channel();
    let (stop, stopped) = oneshot::channel();
    let child = spawn(experiment_command)?;
    let run_harness = async {
        let result = async {
            timeout(Duration::from_secs(180), readiness).await??;
            if let Inference::Local { trial, .. } = &inference_mode {
                return local::drive_live(spawn(harness_command)?, &harness_dir, trial).await;
            }
            let output = drive(
                spawn(harness_command)?,
                inference,
                unused_broker,
                &harness_dir,
                exchanges,
            )
            .await?;
            let console = String::from_utf8_lossy(&output.stdout);
            if !output.status.success()
                || !console.contains("harness connected smoke: PASS")
                || console.contains("harness connected smoke: FAIL")
            {
                return Err("harness did not verify the paired report; see harness logs".into());
            }
            Ok::<_, TestError>(())
        }
        .await;
        // Also request orderly experiment shutdown when the harness test fails.
        let _ = stop.send(());
        result
    };
    let (experiment_result, harness_result) = tokio::join!(
        experiment(child, &experiment_dir, ready, stopped, &monitor, deadline),
        run_harness,
    );
    // Nix outputs contain regular artifacts, not stale control/network sockets.
    // Both supervisors have finished and reaped their QEMU processes here.
    for path in [&link, &monitor] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    experiment_result?;
    harness_result?;
    match &inference_mode {
        Inference::Scripted => {
            println!("paired VM smoke: PASS (real target report and experiment cleanup verified)")
        }
        Inference::Local { trial, .. } => println!("{}", trial.completion_message()),
    }
    Ok(())
}
