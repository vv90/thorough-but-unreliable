//! Host-side runner: only QEMU runs here; all Podman operations stay in the VM.
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

async fn tool(mut command: Command) -> TestResult<Vec<u8>> {
    let output = timeout(
        Duration::from_secs(30),
        command.stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await??;
    if !output.status.success() {
        return Err(format!("tool failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(output.stdout)
}

#[tokio::test]
#[ignore = "requires KVM and the disposable Podman image; use the Nix podman-runtime check"]
async fn podman_vm_smoke() -> TestResult {
    vm_smoke(false).await
}

#[tokio::test]
#[ignore = "requires KVM; use the Nix experiment-service check"]
async fn experiment_vm_smoke() -> TestResult {
    vm_smoke(true).await
}

async fn vm_smoke(experiment: bool) -> TestResult {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")?;
    let image = std::fs::canonicalize(
        std::env::var_os("PODMAN_SMOKE_IMAGE").ok_or("missing PODMAN_SMOKE_IMAGE")?,
    )?;
    std::fs::create_dir_all(".artifacts")?;
    let mut mktemp = Command::new("mktemp");
    mktemp.args(["-d", ".artifacts/podman-runtime.XXXXXX"]);
    let directory = PathBuf::from(String::from_utf8(tool(mktemp).await?)?.trim());
    println!("Podman VM artifacts: {}", directory.display());
    let overlay = directory.join("experiment.qcow2");
    let mut mkdisk = Command::new("qemu-img");
    mkdisk
        .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
        .arg(image)
        .arg(&overlay);
    tool(mkdisk).await?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu.args([
        "-enable-kvm",
        "-machine",
        "q35",
        "-cpu",
        "host",
        "-m",
        "2048",
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
    if experiment {
        // A standalone isolated NIC: no user-mode NAT, host forwarding, or
        // shared directories. The guest test exercises its configured address.
        qemu.args([
            "-netdev",
            "hubport,id=command,hubid=0",
            "-device",
            "virtio-net-pci,netdev=command,mac=52:54:00:99:02:02",
        ]);
    }
    let mut child = qemu
        .arg("-drive")
        .arg(format!("file={},format=qcow2,if=virtio", overlay.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut console = Vec::new();
    let mut stderr = Vec::new();
    let result = timeout(Duration::from_secs(300), async {
        let out = child.stdout.take().ok_or("missing VM stdout")?;
        let err = child.stderr.take().ok_or("missing VM stderr")?;
        let read_console = async {
            out.take(4194305).read_to_end(&mut console).await?;
            if console.len() > 4194304 {
                return Err("console exceeds 4 MiB".into());
            }
            Ok::<_, TestError>(())
        };
        let read_stderr = async {
            err.take(65537).read_to_end(&mut stderr).await?;
            if stderr.len() > 65536 {
                return Err("stderr exceeds 64 KiB".into());
            }
            Ok::<_, TestError>(())
        };
        let wait = async { Ok::<_, TestError>(child.wait().await?) };
        let (status, (), ()) = tokio::try_join!(wait, read_console, read_stderr)?;
        Ok::<_, TestError>(status)
    })
    .await;
    // Preserve diagnostics even if killing/reaping the child fails.
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
    if !result??.success() {
        return Err("QEMU failed; see retained logs".into());
    }
    let console = String::from_utf8_lossy(&console);
    let marker = if experiment {
        "experiment service smoke"
    } else {
        "podman runtime smoke"
    };
    if !console.contains(&format!("{marker}: PASS")) || console.contains(&format!("{marker}: FAIL"))
    {
        return Err("guest did not verify execution and cleanup; see console.log".into());
    }
    Ok(())
}
