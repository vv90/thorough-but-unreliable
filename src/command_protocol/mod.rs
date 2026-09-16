//! Pure version-1 harness/broker protocol. HTTP IO and authentication are separate.

use std::{
    fmt,
    io::Write,
    panic::{AssertUnwindSafe, catch_unwind},
};

use crate::target::{CommandRequest, CommandSequence, ExecutionReport, SessionState};

mod wire;

pub const METHOD: &str = "POST";
pub const PATH: &str = "/v1/command";
pub const CONTENT_TYPE: &str = "application/json";

#[derive(Debug)]
pub enum ProtocolError {
    BodyTooLarge {
        limit: usize,
    },
    Json(serde_json::Error),
    Allocation(std::collections::TryReserveError),
    SequenceMismatch {
        expected: CommandSequence,
        received: CommandSequence,
    },
    DependencyPanicked,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyTooLarge { limit } => {
                write!(f, "command protocol body exceeds {limit} bytes")
            }
            Self::Json(error) => write!(f, "invalid command protocol JSON: {error}"),
            Self::Allocation(error) => write!(f, "command protocol allocation failed: {error}"),
            Self::SequenceMismatch { expected, received } => write!(
                f,
                "expected command sequence {}, received {}",
                expected.get(),
                received.get()
            ),
            Self::DependencyPanicked => f.write_str("command protocol dependency panicked"),
        }
    }
}
impl std::error::Error for ProtocolError {}

/// Limit is the full JSON body size, including escaped text and hex expansion.
pub fn encode_request(request: &CommandRequest, limit: usize) -> Result<Vec<u8>, ProtocolError> {
    encode_command(request.sequence, &request.command, limit)
}

/// Borrow the command directly so clients need not clone it before bounded encoding.
pub fn encode_command(
    sequence: CommandSequence,
    command: &str,
    limit: usize,
) -> Result<Vec<u8>, ProtocolError> {
    guard(|| wire::encode_request(sequence, command, limit))
}

/// Check body size before parsing. HTTP readers must also bound accumulation.
pub fn decode_request(bytes: &[u8], limit: usize) -> Result<CommandRequest, ProtocolError> {
    guard(|| {
        check_size(bytes.len(), limit)?;
        wire::decode_request(bytes)
    })
}

pub fn encode_report(report: &ExecutionReport, limit: usize) -> Result<Vec<u8>, ProtocolError> {
    guard(|| wire::encode_report(report, limit))
}

/// A successfully decoded report always belongs to the outstanding request.
pub fn decode_report(
    bytes: &[u8],
    limit: usize,
    expected: CommandSequence,
) -> Result<ExecutionReport, ProtocolError> {
    guard(|| {
        check_size(bytes.len(), limit)?;
        let report = wire::decode_report(bytes)?;
        if report.sequence != expected {
            return Err(ProtocolError::SequenceMismatch {
                expected,
                received: report.sequence,
            });
        }
        Ok(report)
    })
}

fn check_size(size: usize, limit: usize) -> Result<(), ProtocolError> {
    if size > limit {
        Err(ProtocolError::BodyTooLarge { limit })
    } else {
        Ok(())
    }
}

fn guard<T>(call: impl FnOnce() -> Result<T, ProtocolError>) -> Result<T, ProtocolError> {
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(result) => result,
        Err(payload) => {
            // Foreign payload destructors may unwind too.
            std::mem::forget(payload);
            Err(ProtocolError::DependencyPanicked)
        }
    }
}

/// A private writer bounds serialization itself, not just the completed body.
fn encode(value: &impl serde::Serialize, limit: usize) -> Result<Vec<u8>, ProtocolError> {
    struct BoundedWriter {
        bytes: Vec<u8>,
        limit: usize,
        failure: Option<ProtocolError>,
    }
    impl Write for BoundedWriter {
        fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
            let result = self
                .bytes
                .len()
                .checked_add(chunk.len())
                .ok_or(ProtocolError::BodyTooLarge { limit: self.limit })
                .and_then(|size| {
                    check_size(size, self.limit)?;
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
                            .map_err(ProtocolError::Allocation)?;
                    }
                    Ok(())
                });
            if let Err(error) = result {
                self.failure = Some(error);
                return Err(std::io::Error::other("bounded protocol writer failed"));
            }
            self.bytes.extend_from_slice(chunk);
            Ok(chunk.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        limit,
        failure: None,
    };
    let result = serde_json::to_writer(&mut writer, value);
    if let Some(error) = writer.failure {
        return Err(error);
    }
    result.map_err(ProtocolError::Json)?;
    Ok(writer.bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceState {
    Ready(CommandSequence),
    Awaiting(CommandSequence),
    Unusable,
    Exhausted,
}

/// Pure bookkeeping shared by future client/broker adapters. Begin reserves a
/// sequence before dispatch; finish consumes its report. It does not execute,
/// authenticate, stop a target, or guarantee exactly-once remote effects.
#[derive(Debug)]
pub struct SequenceTracker {
    state: SequenceState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceError {
    Mismatch {
        expected: CommandSequence,
        received: CommandSequence,
    },
    Outstanding,
    NoOutstanding,
    Unusable,
    Exhausted,
}
impl fmt::Display for SequenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "command sequence error: {self:?}")
    }
}
impl std::error::Error for SequenceError {}

impl Default for SequenceTracker {
    fn default() -> Self {
        Self::new()
    }
}
impl SequenceTracker {
    pub const fn new() -> Self {
        Self {
            state: SequenceState::Ready(CommandSequence::new(1)),
        }
    }
    pub fn next_sequence(&self) -> Option<CommandSequence> {
        match self.state {
            SequenceState::Ready(sequence) => Some(sequence),
            _ => None,
        }
    }
    /// Unexpected requests are rejected without changing the current state.
    pub fn begin(&mut self, received: CommandSequence) -> Result<(), SequenceError> {
        match self.state {
            SequenceState::Ready(expected) if expected == received => {
                self.state = SequenceState::Awaiting(expected);
                Ok(())
            }
            SequenceState::Ready(expected) => Err(SequenceError::Mismatch { expected, received }),
            SequenceState::Awaiting(_) => Err(SequenceError::Outstanding),
            SequenceState::Unusable => Err(SequenceError::Unusable),
            SequenceState::Exhausted => Err(SequenceError::Exhausted),
        }
    }
    /// A mismatched report poisons the session; never dispatch after uncertainty.
    pub fn finish(&mut self, report: &ExecutionReport) -> Result<(), SequenceError> {
        let expected = match self.state {
            SequenceState::Awaiting(sequence) => sequence,
            SequenceState::Ready(_) => return Err(SequenceError::NoOutstanding),
            SequenceState::Unusable => return Err(SequenceError::Unusable),
            SequenceState::Exhausted => return Err(SequenceError::Exhausted),
        };
        if expected != report.sequence {
            self.abandon();
            return Err(SequenceError::Mismatch {
                expected,
                received: report.sequence,
            });
        }
        self.state = match report.session_state() {
            SessionState::Unusable => SequenceState::Unusable,
            SessionState::Ready => match expected.checked_next() {
                Some(next) => SequenceState::Ready(next),
                None => SequenceState::Exhausted,
            },
        };
        Ok(())
    }
    /// Call after cancellation or failure to obtain a valid report. This only
    /// prevents local reuse; external supervision must handle remote execution.
    pub fn abandon(&mut self) {
        self.state = SequenceState::Unusable;
    }
}

#[cfg(test)]
mod tests;
