use super::{Config, Error, config::full_id, guard};
use serde::{Deserialize, Serialize};
use std::{fmt, io::Write};

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Create<'a> {
    cmd: [&'a str; 3],
    user: &'a str,
    working_dir: &'a str,
    env: &'a [String],
    attach_stdin: bool,
    attach_stdout: bool,
    attach_stderr: bool,
    tty: bool,
    privileged: bool,
}
pub(super) fn create(config: &Config, command: &str) -> Result<Vec<u8>, Error> {
    guard(|| {
        encode(
            &Create {
                cmd: [&config.shell, "-c", command],
                user: &config.user,
                working_dir: &config.workdir,
                env: &config.environment,
                attach_stdin: false,
                attach_stdout: true,
                attach_stderr: true,
                tty: false,
                privileged: false,
            },
            config.limits.json,
        )
    })
}

fn encode(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, Error> {
    struct Buffer {
        bytes: Vec<u8>,
        limit: usize,
        error: Option<Error>,
    }
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let result = self
                .bytes
                .len()
                .checked_add(bytes.len())
                .filter(|n| *n <= self.limit)
                .ok_or(Error::BodyTooLarge { limit: self.limit })
                .and_then(|size| {
                    if size > self.bytes.capacity() {
                        let capacity = self
                            .bytes
                            .capacity()
                            .max(64)
                            .saturating_mul(2)
                            .min(self.limit)
                            .max(size);
                        self.bytes
                            .try_reserve_exact(capacity.saturating_sub(self.bytes.len()))
                            .map_err(Error::Allocation)?;
                    }
                    Ok(())
                });
            if let Err(error) = result {
                self.error = Some(error);
                return Err(std::io::Error::other("bounded Podman JSON writer failed"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Buffer {
        bytes: Vec::new(),
        limit,
        error: None,
    };
    let result = serde_json::to_writer(&mut buffer, value);
    if let Some(error) = buffer.error {
        return Err(error);
    }
    result.map_err(Error::Json)?;
    Ok(buffer.bytes)
}

// Require actual objects; Serde derives alone also accept positional arrays.
// Extra runtime metadata is ignored for forward compatibility. Known fields
// remain required and reject duplicates, including case aliases.
fn object<'de, T: Deserialize<'de>>(bytes: &'de [u8], limit: usize) -> Result<T, Error> {
    if bytes.len() > limit {
        return Err(Error::BodyTooLarge { limit });
    }
    struct Visitor<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
        type Value = T;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a Podman response object")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let result =
        serde::Deserializer::deserialize_map(&mut deserializer, Visitor(std::marker::PhantomData))
            .map_err(Error::Json)?;
    deserializer.end().map_err(Error::Json)?;
    Ok(result)
}

#[derive(Debug)]
pub(super) struct ExecId(String);
impl ExecId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
pub(super) fn created(bytes: &[u8], limit: usize) -> Result<ExecId, Error> {
    #[derive(Deserialize)]
    struct Created {
        #[serde(rename = "Id", alias = "ID")]
        id: String,
    }
    guard(|| {
        let value: Created = object(bytes, limit)?;
        if !full_id(&value.id) {
            return Err(Error::InvalidResponse("invalid exec ID"));
        }
        Ok(ExecId(value.id))
    })
}

pub(super) enum Observed {
    Pending,
    Stopped(u8),
}
pub(super) fn inspect(
    bytes: &[u8],
    limit: usize,
    exec: &ExecId,
    config: &Config,
) -> Result<Observed, Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Inspect {
        #[serde(rename = "ID")]
        id: String,
        #[serde(rename = "ContainerID")]
        container_id: String,
        running: bool,
        can_remove: bool,
        exit_code: i64,
    }
    guard(|| {
        let value: Inspect = object(bytes, limit)?;
        if value.id != exec.0 || value.container_id != config.container_id {
            return Err(Error::InvalidResponse(
                "exec/container correlation mismatch",
            ));
        }
        match (value.running, value.can_remove) {
            (true, true) => Err(Error::InvalidResponse("running exec marked removable")),
            (_, false) => Ok(Observed::Pending),
            (false, true) => u8::try_from(value.exit_code)
                .map(Observed::Stopped)
                .map_err(|_| Error::InvalidResponse("completion status outside 0..255")),
        }
    })
}
