use super::{Error, guard};
use std::{
    collections::BTreeMap,
    num::{NonZeroU32, NonZeroUsize},
    time::Duration,
};

/// Explicit limits for one command. The deadline covers create, start, output
/// draining, and inspection together. It must fit inside the broker watchdog.
#[derive(Debug)]
pub struct Limits {
    pub(super) command: usize,
    pub(super) json: usize,
    pub(super) stdout: usize,
    pub(super) stderr: usize,
    deadline: NonZeroU32,
}
impl Limits {
    pub fn new(
        command: NonZeroUsize,
        json: NonZeroUsize,
        stdout: NonZeroUsize,
        stderr: NonZeroUsize,
        deadline_ms: NonZeroU32,
    ) -> Result<Self, Error> {
        if [command, json, stdout, stderr]
            .iter()
            .any(|n| n.get() > isize::MAX as usize)
        {
            return Err(Error::Configuration("byte limits must fit isize::MAX"));
        }
        Ok(Self {
            command: command.get(),
            json: json.get(),
            stdout: stdout.get(),
            stderr: stderr.get(),
            deadline: deadline_ms,
        })
    }
    pub fn deadline(&self) -> Duration {
        Duration::from_millis(u64::from(self.deadline.get()))
    }
    pub fn json_bytes(&self) -> usize {
        self.json
    }
    pub fn command_bytes(&self) -> usize {
        self.command
    }
    pub fn stdout_bytes(&self) -> usize {
        self.stdout
    }
    pub fn stderr_bytes(&self) -> usize {
        self.stderr
    }
}

/// Unvalidated trusted setup input; never populated from model commands.
pub struct Settings {
    pub socket_path: String,
    pub container_id: String,
    pub uid: u32,
    pub gid: u32,
    pub shell: String,
    pub workdir: String,
    /// Explicit overrides of the prepared container's environment, not the
    /// broker's environment. Empty values are preserved; no host expansion.
    pub environment: BTreeMap<String, String>,
    pub limits: Limits,
}

/// Syntactically validated binding. Actual socket permissions, container identity,
/// image environment and resource policy must be checked by trusted setup.
/// Descendants may persist within the trial; uncertainty ends the entire session.
pub struct Config {
    socket_path: String,
    pub(super) container_id: String,
    pub(super) user: String,
    pub(super) shell: String,
    pub(super) workdir: String,
    pub(super) environment: Vec<String>,
    pub(super) create_path: String,
    pub(super) limits: Limits,
}
impl Config {
    pub fn new(settings: Settings) -> Result<Self, Error> {
        guard(|| {
            let Settings {
                socket_path,
                container_id,
                uid,
                gid,
                shell,
                workdir,
                environment,
                limits,
            } = settings;
            if !absolute(&socket_path) || socket_path.len() > 107 {
                return Err(Error::Configuration(
                    "absolute Linux Unix socket path, at most 107 bytes",
                ));
            }
            if !full_id(&container_id) {
                return Err(Error::Configuration("full lowercase container ID"));
            }
            if !absolute(&shell) {
                return Err(Error::Configuration("absolute shell path"));
            }
            if !absolute(&workdir) {
                return Err(Error::Configuration("absolute working directory"));
            }
            let mut env = Vec::new();
            env.try_reserve_exact(environment.len())
                .map_err(Error::Allocation)?;
            for (key, value) in environment {
                let mut chars = key.bytes();
                if !chars
                    .next()
                    .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                    || !chars.all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    || value.contains('\0')
                {
                    return Err(Error::Configuration("environment key or value"));
                }
                env.push(format!("{key}={value}"));
            }
            let config = Self {
                socket_path,
                create_path: format!("/containers/{container_id}/exec"),
                container_id,
                user: format!("{uid}:{gid}"),
                shell,
                workdir,
                environment: env,
                limits,
            };
            super::wire::create(&config, "")?;
            Ok(config)
        })
    }
    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }
    pub fn container_id(&self) -> &str {
        &self.container_id
    }
    pub fn limits(&self) -> &Limits {
        &self.limits
    }
}
fn absolute(path: &str) -> bool {
    path.starts_with('/') && !path.contains('\0')
}
pub(super) fn full_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
