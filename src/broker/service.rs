//! Standalone process boundary. Target preparation and cleanup belong to its
//! external supervisor; no terminal process exit proves target termination.
use crate::target::podman::{self, PodmanTargetSession};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::SocketAddr,
    num::{NonZeroU32, NonZeroUsize},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    time::Duration,
};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::oneshot,
};

const CONFIG_BYTES: usize = 1024 * 1024;
type Result<T> = std::result::Result<T, String>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u32,
    listen: SocketAddr,
    socket_path: String,
    container_id: String,
    uid: u32,
    gid: u32,
    shell: String,
    workdir: String,
    environment: Vec<(String, String)>,
    command_bytes: NonZeroUsize,
    runtime_json_bytes: NonZeroUsize,
    stdout_bytes: NonZeroUsize,
    stderr_bytes: NonZeroUsize,
    command_timeout_ms: NonZeroU32,
    request_bytes: NonZeroUsize,
    response_bytes: NonZeroUsize,
    connections: NonZeroUsize,
    read_timeout_ms: NonZeroU32,
    adapter_timeout_ms: NonZeroU32,
    write_timeout_ms: NonZeroU32,
}

/// Only constructed after both individual and cross-layer bounds validate.
pub struct Config {
    listen: SocketAddr,
    adapter: podman::Config,
    broker: super::Config,
}

impl Config {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        match catch_unwind(AssertUnwindSafe(|| parse(bytes))) {
            Ok(result) => result,
            Err(payload) => {
                std::mem::forget(payload);
                Err("configuration dependency panicked".into())
            }
        }
    }
}

fn parse(bytes: &[u8]) -> Result<Config> {
    if bytes.len() > CONFIG_BYTES {
        return Err("broker configuration exceeds 1 MiB".into());
    }
    // Serde structs also accept positional arrays; configuration must be an object.
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err("broker configuration must be a JSON object".into());
    }
    let raw: RawConfig = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if raw.version != 1
        || raw.listen.port() == 0
        || raw.listen.ip().is_unspecified()
        || raw.listen.ip().is_multicast()
    {
        return Err(
            "version must be 1 and listener must specify a unicast address and port".into(),
        );
    }
    if raw.command_timeout_ms >= raw.adapter_timeout_ms {
        return Err("command deadline must be shorter than the broker watchdog".into());
    }
    let mut environment = BTreeMap::new();
    for (key, value) in raw.environment {
        if environment.insert(key, value).is_some() {
            return Err("duplicate environment variable".into());
        }
    }
    let adapter = podman::Config::new(podman::Settings {
        socket_path: raw.socket_path,
        container_id: raw.container_id,
        uid: raw.uid,
        gid: raw.gid,
        shell: raw.shell,
        workdir: raw.workdir,
        environment,
        limits: podman::Limits::new(
            raw.command_bytes,
            raw.runtime_json_bytes,
            raw.stdout_bytes,
            raw.stderr_bytes,
            raw.command_timeout_ms,
        )
        .map_err(|e| e.to_string())?,
    })
    .map_err(|e| e.to_string())?;
    let millis = |n: NonZeroU32| Duration::from_millis(u64::from(n.get()));
    let broker = super::Config::new(
        raw.request_bytes,
        raw.response_bytes,
        raw.connections,
        millis(raw.read_timeout_ms),
        millis(raw.adapter_timeout_ms),
        millis(raw.write_timeout_ms),
    )
    .map_err(|e| e.to_string())?;
    Ok(Config {
        listen: raw.listen,
        adapter,
        broker,
    })
}

pub fn run(path: &Path) -> Result<()> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(CONFIG_BYTES + 1)
        .map_err(|e| e.to_string())?;
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take((CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let config = Config::from_slice(&bytes)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(serve(config))
}

async fn serve(config: Config) -> Result<()> {
    // Install handlers before accepting commands. External systemd supervision
    // remains responsible for cleanup after SIGKILL, abort, or setup failure.
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|e| e.to_string())?;
    let (notifier, notice) = oneshot::channel();
    let adapter = PodmanTargetSession::new(config.adapter, notifier);
    writeln!(
        std::io::stdout().lock(),
        "experiment broker: ready listen={}",
        config.listen
    )
    .map_err(|e| e.to_string())?;
    let server = super::serve(listener, adapter, config.broker, async {
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
        }
    });
    // Keep the supervision receiver alive and await the broker even when an
    // unusable notice arrives: its final response still needs to be delivered.
    let (result, event) = tokio::join!(server, notice);
    let event = event.map_err(|_| "adapter supervision channel closed unexpectedly")?;
    writeln!(
        std::io::stderr().lock(),
        "experiment broker: stopped event={event:?}; external cleanup required"
    )
    .map_err(|e| e.to_string())?;
    match result.map_err(|e| e.to_string())? {
        super::StopReason::Shutdown => Ok(()),
        reason => Err(format!("session ended: {reason:?}")),
    }
}

#[cfg(test)]
mod tests;
