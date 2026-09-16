//! Private wire schema. Borrow for encoding; own and validate decoded data.

use super::ProtocolError;
use crate::target::*;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, fmt, num::NonZeroU32};

// Reject positional arrays at every object boundary, retaining Serde's checks
// for duplicate fields. Parsing via Value would silently lose duplicate keys.
#[derive(Serialize)]
#[serde(transparent)]
struct Object<T>(T);
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
            type Value = Object<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(Object)
            }
        }
        deserializer.deserialize_map(Visitor(std::marker::PhantomData))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request<'a> {
    sequence: u64,
    command: Cow<'a, str>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Report<'a> {
    sequence: u64,
    outcome: Object<Outcome<'a>>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Outcome<'a> {
    Completed {
        output: Object<Output<'a>>,
        completion: Object<Completion>,
        #[serde(with = "state")]
        session_state: SessionState,
    },
    NotStarted {
        failure: Object<Failure<'a>>,
    },
    DeadlineExceeded {
        output: Object<Output<'a>>,
        execution_state: Object<DeadlineState>,
    },
    Unknown {
        output: Object<Output<'a>>,
        error: Object<Error<'a>>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Completion {
    Exited {
        code: u8,
        #[serde(with = "source")]
        source: CompletionSource,
    },
    Signaled {
        signal: NonZeroU32,
        #[serde(with = "source")]
        source: CompletionSource,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DeadlineState {
    ConfirmedStopped {
        #[serde(with = "state")]
        session_state: SessionState,
    },
    // Empty struct variants reject extra fields; Serde unit variants do not.
    MayStillBeRunning {},
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Failure<'a> {
    Rejected { rejection: Object<Rejection<'a>> },
    SessionUnusable {},
    Failed { error: Object<Error<'a>> },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rejection<'a> {
    #[serde(with = "rejection_type")]
    kind: RejectionKind,
    diagnostic: Option<Cow<'a, str>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Error<'a> {
    #[serde(with = "error_type")]
    kind: ExecutionErrorKind,
    diagnostic: Option<Cow<'a, str>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Output<'a> {
    stdout: Object<Capture<'a>>,
    stderr: Object<Capture<'a>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capture<'a> {
    #[serde(with = "hex")]
    hex: Cow<'a, [u8]>,
    truncated: bool,
}

// Remote derives keep serialization out of the transport-independent types.
#[derive(Serialize, Deserialize)]
#[serde(remote = "SessionState", rename_all = "snake_case")]
enum State {
    Ready,
    Unusable,
}
#[derive(Serialize, Deserialize)]
#[serde(remote = "CompletionSource", rename_all = "snake_case")]
enum Source {
    ParentObserved,
    GuestReported,
}
#[derive(Serialize, Deserialize)]
#[serde(remote = "RejectionKind", rename_all = "snake_case")]
enum RejectionType {
    InvalidCommand,
    CommandTooLarge,
}
#[derive(Serialize, Deserialize)]
#[serde(remote = "ExecutionErrorKind", rename_all = "snake_case")]
enum ErrorType {
    TargetUnavailable,
    Transport,
    MalformedResponse,
    ExecutionMechanism,
    ResourceExhausted,
    DependencyPanicked,
}

// Serde unit enums also accept maps such as {"ready":null}; v1 requires strings.
macro_rules! string_enum_codec {
    ($module:ident, $schema:ident, $type:ty) => {
        mod $module {
            use super::*;
            pub fn serialize<S: serde::Serializer>(
                value: &$type,
                serializer: S,
            ) -> Result<S::Ok, S::Error> {
                $schema::serialize(value, serializer)
            }
            pub fn deserialize<'de, D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> Result<$type, D::Error> {
                let value = String::deserialize(deserializer)?;
                $schema::deserialize(serde::de::value::StringDeserializer::<D::Error>::new(value))
            }
        }
    };
}
string_enum_codec!(state, State, SessionState);
string_enum_codec!(source, Source, CompletionSource);
string_enum_codec!(rejection_type, RejectionType, RejectionKind);
string_enum_codec!(error_type, ErrorType, ExecutionErrorKind);

mod hex {
    use super::*;
    const INVALID_HEX: &str = "output must contain an even number of lowercase hexadecimal digits";
    pub fn serialize<S: serde::Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        struct Hex<'a>(&'a [u8]);
        impl fmt::Display for Hex<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
        // serde_json streams collect_str to our bounded writer. No temporary
        // hex string proportional to the entire captured output is allocated.
        serializer.collect_str(&Hex(bytes))
    }
    pub fn deserialize<'de, 'a, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Cow<'a, [u8]>, D::Error> {
        let text = Cow::<'de, str>::deserialize(deserializer)?;
        if !text.len().is_multiple_of(2) {
            return Err(serde::de::Error::custom(INVALID_HEX));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(text.len() / 2)
            .map_err(serde::de::Error::custom)?;
        for pair in text.as_bytes().chunks_exact(2) {
            let [high, low] = pair else {
                return Err(serde::de::Error::custom(INVALID_HEX));
            };
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            };
            match (digit(*high), digit(*low)) {
                (Some(high), Some(low)) => bytes.push((high << 4) | low),
                _ => return Err(serde::de::Error::custom(INVALID_HEX)),
            }
        }
        Ok(Cow::Owned(bytes))
    }
}

pub(super) fn encode_request(
    sequence: CommandSequence,
    command: &str,
    limit: usize,
) -> Result<Vec<u8>, ProtocolError> {
    super::encode(
        &Request {
            sequence: sequence.get(),
            command: Cow::Borrowed(command),
        },
        limit,
    )
}
pub(super) fn decode_request(bytes: &[u8]) -> Result<CommandRequest, ProtocolError> {
    let Object(request): Object<Request<'_>> =
        serde_json::from_slice(bytes).map_err(ProtocolError::Json)?;
    Ok(CommandRequest {
        sequence: CommandSequence::new(request.sequence),
        command: request.command.into_owned(),
    })
}
pub(super) fn encode_report(
    report: &ExecutionReport,
    limit: usize,
) -> Result<Vec<u8>, ProtocolError> {
    super::encode(
        &Report {
            sequence: report.sequence.get(),
            outcome: Object(Outcome::from(&report.outcome)),
        },
        limit,
    )
}
pub(super) fn decode_report(bytes: &[u8]) -> Result<ExecutionReport, ProtocolError> {
    let Object(report): Object<Report<'_>> =
        serde_json::from_slice(bytes).map_err(ProtocolError::Json)?;
    Ok(ExecutionReport {
        sequence: CommandSequence::new(report.sequence),
        outcome: report.outcome.0.into(),
    })
}

impl<'a> From<&'a CommandOutput> for Object<Output<'a>> {
    fn from(output: &'a CommandOutput) -> Self {
        let capture = |value: &'a CapturedOutput| {
            Object(Capture {
                hex: Cow::Borrowed(&value.bytes),
                truncated: value.truncated,
            })
        };
        Object(Output {
            stdout: capture(&output.stdout),
            stderr: capture(&output.stderr),
        })
    }
}
impl From<Object<Output<'_>>> for CommandOutput {
    fn from(output: Object<Output<'_>>) -> Self {
        let capture = |value: Object<Capture<'_>>| CapturedOutput {
            bytes: value.0.hex.into_owned(),
            truncated: value.0.truncated,
        };
        Self {
            stdout: capture(output.0.stdout),
            stderr: capture(output.0.stderr),
        }
    }
}
impl<'a> From<&'a ExecutionError> for Object<Error<'a>> {
    fn from(error: &'a ExecutionError) -> Self {
        Object(Error {
            kind: error.kind,
            diagnostic: error.diagnostic.as_deref().map(Cow::Borrowed),
        })
    }
}
impl From<Object<Error<'_>>> for ExecutionError {
    fn from(error: Object<Error<'_>>) -> Self {
        Self {
            kind: error.0.kind,
            diagnostic: error.0.diagnostic.map(Cow::into_owned),
        }
    }
}
impl From<&ProcessCompletion> for Completion {
    fn from(value: &ProcessCompletion) -> Self {
        match *value {
            ProcessCompletion::Exited { code, source } => Self::Exited { code, source },
            ProcessCompletion::Signaled { signal, source } => Self::Signaled { signal, source },
        }
    }
}
impl From<Completion> for ProcessCompletion {
    fn from(value: Completion) -> Self {
        match value {
            Completion::Exited { code, source } => Self::Exited { code, source },
            Completion::Signaled { signal, source } => Self::Signaled { signal, source },
        }
    }
}
impl<'a> From<&'a StartFailure> for Failure<'a> {
    fn from(value: &'a StartFailure) -> Self {
        match value {
            StartFailure::Rejected(rejection) => Self::Rejected {
                rejection: Object(Rejection {
                    kind: rejection.kind,
                    diagnostic: rejection.diagnostic.as_deref().map(Cow::Borrowed),
                }),
            },
            StartFailure::SessionUnusable => Self::SessionUnusable {},
            StartFailure::Failed(error) => Self::Failed {
                error: error.into(),
            },
        }
    }
}
impl From<Failure<'_>> for StartFailure {
    fn from(value: Failure<'_>) -> Self {
        match value {
            Failure::Rejected { rejection } => Self::Rejected(CommandRejection {
                kind: rejection.0.kind,
                diagnostic: rejection.0.diagnostic.map(Cow::into_owned),
            }),
            Failure::SessionUnusable {} => Self::SessionUnusable,
            Failure::Failed { error } => Self::Failed(error.into()),
        }
    }
}
impl From<ExecutionState> for DeadlineState {
    fn from(value: ExecutionState) -> Self {
        match value {
            ExecutionState::ConfirmedStopped { session_state } => {
                Self::ConfirmedStopped { session_state }
            }
            ExecutionState::MayStillBeRunning => Self::MayStillBeRunning {},
        }
    }
}
impl From<DeadlineState> for ExecutionState {
    fn from(value: DeadlineState) -> Self {
        match value {
            DeadlineState::ConfirmedStopped { session_state } => {
                Self::ConfirmedStopped { session_state }
            }
            DeadlineState::MayStillBeRunning {} => Self::MayStillBeRunning,
        }
    }
}
impl<'a> From<&'a ExecutionOutcome> for Outcome<'a> {
    fn from(value: &'a ExecutionOutcome) -> Self {
        match value {
            ExecutionOutcome::Completed {
                output,
                completion,
                session_state,
            } => Self::Completed {
                output: output.into(),
                completion: Object(completion.into()),
                session_state: *session_state,
            },
            ExecutionOutcome::NotStarted(failure) => Self::NotStarted {
                failure: Object(failure.into()),
            },
            ExecutionOutcome::DeadlineExceeded {
                output,
                execution_state,
            } => Self::DeadlineExceeded {
                output: output.into(),
                execution_state: Object((*execution_state).into()),
            },
            ExecutionOutcome::Unknown { output, error } => Self::Unknown {
                output: output.into(),
                error: error.into(),
            },
        }
    }
}
impl From<Outcome<'_>> for ExecutionOutcome {
    fn from(value: Outcome<'_>) -> Self {
        match value {
            Outcome::Completed {
                output,
                completion,
                session_state,
            } => Self::Completed {
                output: output.into(),
                completion: completion.0.into(),
                session_state,
            },
            Outcome::NotStarted { failure } => Self::NotStarted(failure.0.into()),
            Outcome::DeadlineExceeded {
                output,
                execution_state,
            } => Self::DeadlineExceeded {
                output: output.into(),
                execution_state: execution_state.0.into(),
            },
            Outcome::Unknown { output, error } => Self::Unknown {
                output: output.into(),
                error: error.into(),
            },
        }
    }
}
