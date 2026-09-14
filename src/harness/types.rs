//! Internal values, independent of any HTTP or model-provider wire format.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    System(String),
    User(String),
    Assistant(AssistantResponse),
    Tool {
        call_id: String,
        result: Result<CommandResult, DependencyError>,
    },
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandCall {
    pub id: String,
    pub command: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub status: CommandStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandStatus {
    Exited { code: i32 },
    Signaled { signal: i32 },
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DependencyError {
    Failed(String),
    /// The request may have taken effect before the dependency failed.
    CompletionUnknown(String),
    /// A dependency unwound after invocation; its effects are also uncertain.
    Panicked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    Model,
    Command { call_id: String },
}

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
    DependencyFailure {
        operation: Operation,
        error: DependencyError,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    pub history: Vec<Message>,
    pub outcome: RunOutcome,
}
