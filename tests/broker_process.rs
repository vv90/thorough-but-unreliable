//! Standalone executable lifecycle; no real Podman or VM required.
use std::{path::PathBuf, process::Stdio, time::Duration};
use thorough_but_unreliable::{
    harness::{
        async_driver::CommandExecutor,
        command::{CommandClient, CommandConfig},
        types::{CommandCall, ToolCallId},
    },
    target::SessionState,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    net::TcpListener,
    process::Command,
    time::timeout,
};
type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T = ()> = std::result::Result<T, Error>;

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("config.json"));
        let _ = std::fs::remove_dir(&self.0);
    }
}

async fn scenario(stop: bool) -> Result {
    let directory = Directory(
        std::env::temp_dir().join(format!("broker-process-{}-{stop}", std::process::id())),
    );
    std::fs::create_dir(&directory.0)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let mut config: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/broker-config.json"))?;
    let object = config
        .as_object_mut()
        .ok_or("configuration object missing")?;
    object.insert("listen".into(), address.to_string().into());
    object.insert(
        "socket_path".into(),
        directory
            .0
            .join("absent.sock")
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    let path = directory.0.join("config.json");
    std::fs::write(&path, serde_json::to_vec(&config)?)?;
    drop(listener);
    let mut child = Command::new(env!("CARGO_BIN_EXE_experiment-broker"))
        .arg("--config")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let result = timeout(Duration::from_secs(10), async {
        let stdout = child.stdout.take().ok_or("missing stdout")?;
        let mut line = String::new();
        BufReader::new(stdout.take(1024))
            .read_line(&mut line)
            .await?;
        if !line.contains(&format!("ready listen={address}")) {
            return Err("broker did not announce readiness".into());
        }
        if stop {
            let id = child.id().ok_or("broker already exited")?;
            let status = Command::new("kill")
                .args(["-TERM", &id.to_string()])
                .kill_on_drop(true)
                .status()
                .await?;
            if !status.success() {
                return Err("could not signal broker".into());
            }
        } else {
            let mut client = CommandClient::new(CommandConfig {
                command_url: format!("http://{address}/v1/command"),
                connect_timeout: Duration::from_secs(1),
                request_timeout: Duration::from_secs(3),
                max_request_bytes: 32768,
                max_response_bytes: 65536,
            })?;
            let report = client
                .execute(&CommandCall {
                    id: ToolCallId::try_from("command-1".to_owned())?,
                    command: "true".into(),
                })
                .await?;
            if report.session_state() != SessionState::Unusable {
                return Err("missing runtime must invalidate the session".into());
            }
        }
        let status = child.wait().await?;
        if status.success() != stop {
            return Err(format!("unexpected broker exit: {status}").into());
        }
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .ok_or("missing stderr")?
            .take(4096)
            .read_to_string(&mut stderr)
            .await?;
        if !stderr.contains("external cleanup required") {
            return Err("missing cleanup diagnostic".into());
        }
        Ok::<_, Error>(())
    })
    .await;
    if child.try_wait()?.is_none() {
        child.kill().await?;
    }
    child.wait().await?;
    result??;
    Ok(())
}

#[tokio::test]
async fn sigterm_drains_and_exits_successfully() -> Result {
    scenario(true).await
}

#[tokio::test]
async fn terminal_report_is_delivered_before_process_exits() -> Result {
    scenario(false).await
}
