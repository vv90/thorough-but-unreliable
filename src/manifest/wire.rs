//! Private wire objects and pure validation; no client construction or clock IO.

use serde::{Deserialize, Deserializer, de};
use std::{
    fmt,
    num::{NonZeroU32, NonZeroUsize},
    time::Duration,
};

use super::{CommandSettings, InferenceSettings, Manifest, ManifestError, TransportLimits};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawManifest {
    version: u64,
    run_id: String,
    system_prompt: String,
    task: String,
    max_model_turns: u64,
    #[serde(deserialize_with = "deserialize_object")]
    inference: RawInference,
    #[serde(deserialize_with = "deserialize_object")]
    command: RawCommand,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInference {
    completion_url: String,
    model: String,
    max_tokens: u64,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    max_request_bytes: u64,
    max_response_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    command_url: String,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    max_request_bytes: u64,
    max_response_bytes: u64,
}

/// Serde structs otherwise also accept positional arrays. Require maps at every
/// level without losing the derive's duplicate/unknown/missing-field checks.
pub(super) fn deserialize_object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct ObjectVisitor<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> de::Visitor<'de> for ObjectVisitor<T> {
        type Value = T;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a manifest object")
        }
        fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::deserialize(de::value::MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_map(ObjectVisitor(std::marker::PhantomData))
}

impl TryFrom<RawManifest> for Manifest {
    type Error = ManifestError;
    fn try_from(raw: RawManifest) -> Result<Self, Self::Error> {
        if raw.version != 1 {
            return Err(ManifestError::UnsupportedVersion(raw.version));
        }
        if raw.run_id.is_empty() {
            return Err(ManifestError::EmptyRunId);
        }
        let inference = raw.inference;
        let command = raw.command;
        Ok(Self {
            run_id: raw.run_id,
            system_prompt: text(raw.system_prompt, "system_prompt")?,
            task: text(raw.task, "task")?,
            max_model_turns: positive_u32(raw.max_model_turns, "max_model_turns")?,
            inference: InferenceSettings {
                completion_url: endpoint(
                    &inference.completion_url,
                    "/v1/chat/completions",
                    "inference.completion_url",
                )?,
                model: text(inference.model, "inference.model")?,
                max_tokens: positive_u32(inference.max_tokens, "inference.max_tokens")?,
                transport: limits(
                    inference.connect_timeout_ms,
                    inference.request_timeout_ms,
                    inference.max_request_bytes,
                    inference.max_response_bytes,
                    "inference",
                )?,
            },
            command: CommandSettings {
                command_url: endpoint(
                    &command.command_url,
                    crate::command_protocol::PATH,
                    "command.command_url",
                )?,
                transport: limits(
                    command.connect_timeout_ms,
                    command.request_timeout_ms,
                    command.max_request_bytes,
                    command.max_response_bytes,
                    "command",
                )?,
            },
        })
    }
}

fn invalid(field: &'static str, reason: &'static str) -> ManifestError {
    ManifestError::InvalidField { field, reason }
}

fn text(value: String, field: &'static str) -> Result<String, ManifestError> {
    if value.trim().is_empty() {
        return Err(invalid(field, "must contain non-whitespace text"));
    }
    Ok(value)
}

fn positive_u32(value: u64, field: &'static str) -> Result<NonZeroU32, ManifestError> {
    u32::try_from(value)
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or_else(|| invalid(field, "must be an integer from 1 to 4294967295"))
}

fn endpoint(value: &str, path: &str, field: &'static str) -> Result<reqwest::Url, ManifestError> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| invalid(field, "must be an absolute HTTP(S) URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != path
    {
        return Err(invalid(
            field,
            "must use HTTP(S), the client's exact endpoint path, and no credentials, query, or fragment",
        ));
    }
    Ok(url)
}

fn limits(
    connect_ms: u64,
    request_ms: u64,
    request_bytes: u64,
    response_bytes: u64,
    field: &'static str,
) -> Result<TransportLimits, ManifestError> {
    let connect = positive_u32(connect_ms, field)?;
    let request = positive_u32(request_ms, field)?;
    if connect > request {
        return Err(invalid(
            field,
            "connect_timeout_ms must not exceed request_timeout_ms",
        ));
    }
    let byte_limit = |value| {
        usize::try_from(value)
            .ok()
            .filter(|value| isize::try_from(*value).is_ok())
            .and_then(NonZeroUsize::new)
            .ok_or_else(|| {
                invalid(
                    field,
                    "byte limits must be positive and fit a Rust byte buffer",
                )
            })
    };
    Ok(TransportLimits {
        connect_timeout: Duration::from_millis(u64::from(connect.get())),
        request_timeout: Duration::from_millis(u64::from(request.get())),
        max_request_bytes: byte_limit(request_bytes)?,
        max_response_bytes: byte_limit(response_bytes)?,
    })
}
