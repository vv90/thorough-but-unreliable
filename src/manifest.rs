use serde::{Deserialize, Deserializer, de};
use std::{
    collections::TryReserveError,
    fmt,
    fs::File,
    io::Read,
    num::{NonZeroU32, NonZeroUsize},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    time::Duration,
};

mod wire;

/// Includes JSON syntax, escaping, and inline prompt/task text.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// Validated run settings. Fields are private and exposed only for reading.
///
/// ```compile_fail
/// use thorough_but_unreliable::manifest::Manifest;
/// let invalid = Manifest { run_id: String::new() };
/// ```
/// Direct Serde deserialization also validates the schema and semantics:
///
/// ```
/// use thorough_but_unreliable::manifest::Manifest;
/// assert!(serde_json::from_str::<Manifest>(r#"{"version":2,"run_id":""}"#).is_err());
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct Manifest {
    run_id: String,
    system_prompt: String,
    task: String,
    max_model_turns: NonZeroU32,
    inference: InferenceSettings,
    command: CommandSettings,
}

#[derive(Debug, PartialEq, Eq)]
pub struct InferenceSettings {
    completion_url: reqwest::Url,
    model: String,
    max_tokens: NonZeroU32,
    transport: TransportLimits,
}

impl InferenceSettings {
    pub fn completion_url(&self) -> &reqwest::Url {
        &self.completion_url
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn max_tokens(&self) -> NonZeroU32 {
        self.max_tokens
    }
    pub fn transport(&self) -> &TransportLimits {
        &self.transport
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CommandSettings {
    command_url: reqwest::Url,
    transport: TransportLimits,
}

impl CommandSettings {
    pub fn command_url(&self) -> &reqwest::Url {
        &self.command_url
    }
    pub fn transport(&self) -> &TransportLimits {
        &self.transport
    }
}

/// Positive durations of at most u32::MAX milliseconds, with connection timeout
/// no longer than total request timeout. Byte limits fit a Rust byte buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct TransportLimits {
    connect_timeout: Duration,
    request_timeout: Duration,
    max_request_bytes: NonZeroUsize,
    max_response_bytes: NonZeroUsize,
}

impl TransportLimits {
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }
    pub fn max_request_bytes(&self) -> NonZeroUsize {
        self.max_request_bytes
    }
    pub fn max_response_bytes(&self) -> NonZeroUsize {
        self.max_response_bytes
    }
}

impl<'de> Deserialize<'de> for Manifest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: wire::RawManifest = wire::deserialize_object(deserializer)?;
        guard(|| Self::try_from(raw)).map_err(de::Error::custom)
    }
}

impl Manifest {
    pub const fn version(&self) -> u64 {
        1
    }
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
    pub fn task(&self) -> &str {
        &self.task
    }
    pub fn max_model_turns(&self) -> NonZeroU32 {
        self.max_model_turns
    }
    pub fn inference(&self) -> &InferenceSettings {
        &self.inference
    }
    pub fn command(&self) -> &CommandSettings {
        &self.command
    }

    /// Pure bounded parsing and semantic validation with typed errors.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge);
        }
        guard(|| {
            let mut deserializer = serde_json::Deserializer::from_slice(bytes);
            let raw: wire::RawManifest =
                wire::deserialize_object(&mut deserializer).map_err(ManifestError::Parse)?;
            deserializer.end().map_err(ManifestError::Parse)?;
            Self::try_from(raw)
        })
    }

    pub fn from_path(path: &Path) -> Result<Self, ManifestError> {
        Self::from_reader(File::open(path).map_err(ManifestError::Read)?)
    }

    /// Thin bounded IO layer, containing custom-reader unwinds.
    pub fn from_reader(mut reader: impl Read) -> Result<Self, ManifestError> {
        guard(move || {
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(ManifestError::Read(error)),
                };
                let chunk = buffer.get(..count).ok_or_else(|| {
                    ManifestError::Read(std::io::Error::other(
                        "reader returned an invalid byte count",
                    ))
                })?;
                if bytes
                    .len()
                    .checked_add(count)
                    .is_none_or(|size| size > MAX_MANIFEST_BYTES)
                {
                    return Err(ManifestError::TooLarge);
                }
                bytes
                    .try_reserve(count)
                    .map_err(ManifestError::Allocation)?;
                bytes.extend_from_slice(chunk);
            }
            Self::from_slice(&bytes)
        })
    }
}

fn guard<T>(operation: impl FnOnce() -> Result<T, ManifestError>) -> Result<T, ManifestError> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            Err(ManifestError::DependencyPanicked)
        }
    }
}

#[derive(Debug)]
pub enum ManifestError {
    Read(std::io::Error),
    Parse(serde_json::Error),
    UnsupportedVersion(u64),
    EmptyRunId,
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    TooLarge,
    Allocation(TryReserveError),
    DependencyPanicked,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(f, "could not read manifest: {error}"),
            Self::Parse(error) => write!(f, "invalid manifest JSON: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported manifest version {version}; expected 1")
            }
            Self::EmptyRunId => f.write_str("run_id must not be empty"),
            Self::InvalidField { field, reason } => write!(f, "invalid {field}: {reason}"),
            Self::TooLarge => write!(f, "manifest exceeds {MAX_MANIFEST_BYTES} bytes"),
            Self::Allocation(error) => write!(f, "could not allocate manifest buffer: {error}"),
            Self::DependencyPanicked => f.write_str("manifest dependency panicked"),
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Allocation(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
