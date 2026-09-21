//! Opt-in KVM test of the bootable image. The ordinary test validates the same
//! fixtures against the executable without needing QEMU or KVM.

#[path = "connected_vm/paired.rs"]
mod paired;
mod support;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};
use support::*;
use tokio::{
    io::AsyncReadExt,
    net::TcpListener,
    process::{Child, Command},
    sync::oneshot,
    time::timeout,
};

const MANIFEST: &str = include_str!("fixtures/connected-manifest.json");
const REPORT: &str = include_str!("fixtures/connected-report.json");

fn script() -> TestResult<Vec<Exchange>> {
    let command = json!({"role":"assistant","content":null,"tool_calls":[{"id":"command","type":"function","function":{"name":"execute_target_command","arguments":"{\"command\":\"printf 'smoke\\\\n'\"}"}}]});
    let submit = json!({"role":"assistant","content":null,"tool_calls":[{"id":"submit","type":"function","function":{"name":"submit","arguments":"{\"answer\":\"connected smoke complete\"}"}}]});
    let initial = vec![
        json!({"role":"system","content":"system"}),
        json!({"role":"user","content":"task"}),
    ];
    let expected: Value = serde_json::from_str(REPORT)?;
    let result = expected
        .pointer("/history/3/result/report")
        .ok_or("missing fixture report")?;
    let mut next = initial.clone();
    next.push(command.clone());
    next.push(json!({"role":"tool","tool_call_id":"command","content":result.to_string()}));
    let response = |message| {
        Reply::Json(json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":message}]}))
    };
    Ok(vec![
        Exchange::Inference {
            messages: initial,
            reply: response(command),
        },
        Exchange::Command {
            request: json!({"sequence":1,"command":"printf 'smoke\\n'"}),
            reply: Reply::Json(
                json!({"sequence":1,"outcome":{"kind":"completed","session_state":"ready","completion":{"kind":"exited","code":0,"source":"guest_reported"},"output":{"stdout":{"hex":"736d6f6b650a","truncated":false},"stderr":{"hex":"","truncated":false}}}}),
            ),
        },
        Exchange::Inference {
            messages: next,
            reply: response(submit),
        },
    ])
}

async fn artifact_directory() -> TestResult<PathBuf> {
    std::fs::create_dir_all(".artifacts")?;
    let mut command = Command::new("mktemp");
    command.args(["-d", ".artifacts/harness-connected.XXXXXX"]);
    let output = tool(command).await?;
    let path = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    println!("connected smoke artifacts: {}", path.display());
    Ok(path)
}

async fn tool(mut command: Command) -> TestResult<Output> {
    let output = timeout(
        Duration::from_secs(30),
        command.stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await??;
    if !output.status.success() {
        return Err(format!("tool failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(output)
}

async fn drive(
    mut child: Child,
    inference: TcpListener,
    broker: TcpListener,
    directory: &Path,
    exchanges: Vec<Exchange>,
) -> TestResult<Output> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let result = timeout(Duration::from_secs(180), async {
        let out = child.stdout.take().ok_or("missing stdout")?;
        let err = child.stderr.take().ok_or("missing stderr")?;
        let (done, received) = oneshot::channel();
        let wait = async {
            let status = child.wait().await?;
            done.send(()).map_err(|_| "fixture ended early")?;
            Ok::<_, TestError>(status)
        };
        let read_out = async {
            out.take(4194305).read_to_end(&mut stdout).await?;
            if stdout.len() > 4194304 {
                return Err("console output exceeds 4 MiB".into());
            }
            Ok::<_, TestError>(())
        };
        let read_err = async {
            err.take(65537).read_to_end(&mut stderr).await?;
            if stderr.len() > 65536 {
                return Err("stderr exceeds 64 KiB".into());
            }
            Ok::<_, TestError>(())
        };
        let (status, (), (), ()) = tokio::try_join!(
            wait,
            read_out,
            read_err,
            serve(inference, broker, exchanges.into(), received)
        )?;
        Ok::<_, TestError>(status)
    })
    .await;
    if child.try_wait()?.is_none() {
        child.kill().await?;
    }
    std::fs::write(directory.join("console.log"), &stdout)?;
    std::fs::write(directory.join("stderr.log"), &stderr)?;
    Ok(Output {
        status: result??,
        stdout,
        stderr,
    })
}

fn spawn(mut command: Command) -> std::io::Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

#[tokio::test]
async fn connected_fixtures_match_executable_report() -> TestResult {
    let directory = artifact_directory().await?;
    let inference = TcpListener::bind("127.0.0.1:0").await?;
    let broker = TcpListener::bind("127.0.0.1:0").await?;
    let mut manifest: Value = serde_json::from_str(MANIFEST)?;
    for (field, address) in [
        (
            "/inference/completion_url",
            format!("http://{}/v1/chat/completions", inference.local_addr()?),
        ),
        (
            "/command/command_url",
            format!("http://{}/v1/command", broker.local_addr()?),
        ),
    ] {
        *manifest.pointer_mut(field).ok_or("missing endpoint")? = json!(address);
    }
    let path = directory.join("manifest.json");
    std::fs::write(&path, serde_json::to_vec(&manifest)?)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_harness"));
    command.args(["run", "--manifest"]).arg(path);
    let output = drive(spawn(command)?, inference, broker, &directory, script()?).await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?,
        serde_json::from_str::<Value>(REPORT)?
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires KVM, the connected smoke image, QEMU, xorrisofs, and nc; see README"]
async fn connected_vm_smoke() -> TestResult {
    // Fail before artifact creation if the host cannot run this test.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")?;
    let image = std::fs::canonicalize(
        std::env::var_os("HARNESS_SMOKE_IMAGE")
            .ok_or("set HARNESS_SMOKE_IMAGE to the connected smoke qcow2")?,
    )?;
    let directory = artifact_directory().await?;
    let config = directory.join("config");
    std::fs::create_dir(&config)?;
    std::fs::write(config.join("manifest.json"), MANIFEST)?;
    let iso = directory.join("harness-config.iso");
    let overlay = directory.join("harness.qcow2");
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
    let mut mkdisk = Command::new("qemu-img");
    mkdisk
        .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
        .arg(image)
        .arg(&overlay);
    tool(mkdisk).await?;
    let inference = TcpListener::bind("127.0.0.1:0").await?;
    let broker = TcpListener::bind("127.0.0.1:0").await?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu.args([
        "-enable-kvm",
        "-machine",
        "q35",
        "-cpu",
        "host",
        "-m",
        "1024",
        "-display",
        "none",
        "-monitor",
        "none",
        "-serial",
        "stdio",
        "-no-reboot",
        "-nic",
        "none",
    ]);
    qemu.arg("-drive")
        .arg(format!("file={},format=qcow2,if=virtio", overlay.display()));
    qemu.arg("-drive").arg(format!(
        "file={},format=raw,media=cdrom,readonly=on",
        iso.display()
    ));
    // Independent restricted user networks. QEMU gateway/DNS addresses are moved
    // away from our static endpoints. A /24 backend accommodates libslirp's
    // service addresses; the guest command interface retains its /30 mask.
    for (id, subnet, guest, port, local, mac) in [
        (
            "inference",
            "10.99.1",
            "10.99.1.1",
            11434,
            inference.local_addr()?.port(),
            "52:54:00:99:01:02",
        ),
        (
            "command",
            "10.99.2",
            "10.99.2.2",
            8080,
            broker.local_addr()?.port(),
            "52:54:00:99:02:01",
        ),
    ] {
        qemu.arg("-netdev").arg(format!("user,id={id},net={subnet}.0/24,host={subnet}.254,dns={subnet}.253,ipv6=off,restrict=on,guestfwd=tcp:{guest}:{port}-cmd:nc 127.0.0.1 {local}"));
        qemu.arg("-device")
            .arg(format!("virtio-net-pci,netdev={id},mac={mac}"));
    }
    let output = drive(spawn(qemu)?, inference, broker, &directory, script()?).await?;
    assert!(
        output.status.success(),
        "QEMU failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let console = String::from_utf8_lossy(&output.stdout);
    assert!(
        console.contains("harness connected smoke: PASS"),
        "guest did not verify report; see console.log"
    );
    assert!(!console.contains("harness connected smoke: FAIL"));
    println!("connected VM smoke passed; guest report: /var/lib/harness/report.json");
    Ok(())
}
