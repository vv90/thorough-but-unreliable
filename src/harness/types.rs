//! Internal values, independent of HTTP and model-provider wire formats.

use crate::target::{CommandSequence, ExecutionReport};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    System(String),
    User(String),
    Assistant(AssistantResponse),
    Tool {
        call_id: ToolCallId,
        result: Result<ExecutionReport, CommandClientError>,
    },
}

/// Raw model input: malformed batches remain representable for diagnosis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub tool: Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tool {
    ExecuteTargetCommand { command: String },
    Submit { answer: String },
}

/// Validated model identifier. It is not a broker command sequence number.
///
/// ```compile_fail
/// use thorough_but_unreliable::harness::types::ToolCallId;
/// let id = ToolCallId(String::new());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ToolCallId(String);

impl ToolCallId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmptyToolCallId;
impl fmt::Display for EmptyToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("tool call ID must not be empty")
    }
}
impl std::error::Error for EmptyToolCallId {}
impl TryFrom<String> for ToolCallId {
    type Error = EmptyToolCallId;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            Err(EmptyToolCallId)
        } else {
            Ok(Self(value))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandCall {
    pub id: ToolCallId,
    pub command: String,
}

/// Stable categories survive conversion from provider/library errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelFailure {
    pub kind: ModelFailureKind,
    pub diagnostic: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelFailureKind {
    Configuration,
    InvalidHistory,
    Json,
    InvalidResponse,
    UnsupportedTool(String),
    TokenLimit,
    ClientBuild,
    Transport,
    HttpStatus(u16),
    RequestTooLarge { limit: usize },
    ResponseTooLarge { limit: usize },
    Allocation,
    Panicked,
}
impl ModelFailure {
    pub fn panicked() -> Self {
        Self {
            kind: ModelFailureKind::Panicked,
            diagnostic: None,
        }
    }
    /// Transport failure or panic may follow dispatch. Other errors do not
    /// imply that the server performed no computation.
    pub fn completion_unknown(&self) -> bool {
        matches!(
            self.kind,
            ModelFailureKind::Transport | ModelFailureKind::Panicked
        )
    }
}

impl fmt::Display for ModelFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "model failure: {:?}", self.kind)?;
        if let Some(diagnostic) = &self.diagnostic {
            write!(f, ": {diagnostic}")?;
        }
        Ok(())
    }
}
impl std::error::Error for ModelFailure {}

/// No valid target report was obtained. Claimed target output/status belong
/// exclusively to ExecutionReport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandClientError {
    pub kind: CommandClientErrorKind,
    pub diagnostic: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandClientErrorKind {
    Configuration,
    RequestTooLarge {
        limit: usize,
    },
    Transport,
    HttpStatus(u16),
    InvalidResponse,
    ResponseTooLarge {
        limit: usize,
    },
    SequenceMismatch {
        expected: CommandSequence,
        received: CommandSequence,
    },
    Panicked,
}
impl CommandClientError {
    pub fn panicked() -> Self {
        Self {
            kind: CommandClientErrorKind::Panicked,
            diagnostic: None,
        }
    }
    /// Only local pre-dispatch rejection proves execution did not start. Even an
    /// HTTP error may follow execution without a valid report.
    pub fn completion_unknown(&self) -> bool {
        !matches!(
            self.kind,
            CommandClientErrorKind::Configuration | CommandClientErrorKind::RequestTooLarge { .. }
        )
    }
}

impl fmt::Display for CommandClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "command-client failure: {:?}", self.kind)?;
        if let Some(diagnostic) = &self.diagnostic {
            write!(f, ": {diagnostic}")?;
        }
        Ok(())
    }
}
impl std::error::Error for CommandClientError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    EmptyCallId,
    DuplicateCallId(String),
    MultipleSubmissions,
    MixedSubmissionAndCommands,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Submitted {
        answer: String,
    },
    EarlyTermination,
    ModelTurnLimit,
    ProtocolError(ProtocolError),
    ModelFailure(ModelFailure),
    CommandClientFailure {
        call_id: ToolCallId,
        error: CommandClientError,
    },
    /// The complete target report, including partial output, is in history.
    TargetSessionUnusable {
        call_id: ToolCallId,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    pub history: Vec<Message>,
    pub outcome: RunOutcome,
}
