//! Experiment-side command execution contract, independent of transport.
//!
//! Trusted setup prepares one target and binds its identity, execution user,
//! shell, initial working directory, environment, and resource/descendant policy
//! to a session. Setup must fail if an adapter cannot implement that policy.
//! Target creation, reset, and destruction belong to external supervision.
//! The broker owns authentication and sequence validation; adapters own execution.

use std::{fmt, future::Future, num::NonZeroU32};

/// A session bound to one prepared target and its trusted execution policy.
///
/// # Adapter contract
///
/// - Launch a fresh shell for each request under the configured identity, initial
///   working directory, and environment. Filesystem changes persist; shell-local
///   state does not. Never interpret command text in a parent shell.
/// - Supply EOF on stdin and capture stdout/stderr separately as bounded bytes.
///   The command must not inherit control channels or parent credentials.
/// - Apply the configured execution deadline and output limits. Preserve partial
///   output on failure. Never retry execution automatically.
/// - Echo the request sequence in every report. The broker rejects duplicate or
///   out-of-order requests before calling this method; this trait alone does not
///   provide deduplication or an exactly-once execution guarantee.
/// - Report only what the adapter can establish. Shell exit does not establish
///   descendant termination. Guest claims alone cannot confirm execution stopped.
///   Readiness must account for the configured descendant policy.
/// - Lost replies, malformed protocol records, and uncertain deadlines make the
///   session unusable. Once unusable, it stays unusable: subsequent calls return
///   `NotStarted(SessionUnusable)`, without dispatching any command.
/// - Return typed errors rather than panicking. Contain dependency unwinds where
///   supported and report their execution uncertainty; process aborts cannot be
///   caught. Discard the session after a dependency panic.
///
/// `&mut self` serializes calls through a session. Implementations must also avoid
/// sharing a target with another active execution session.
///
/// # Cancellation
///
/// Dropping the returned future does not guarantee that execution stopped and
/// produces no report. The owner must discard the session and notify supervision;
/// it must not reuse the session or retry the command. A deadline that cannot
/// confirm termination requires supervision to end the trial.
///
/// # Example
///
/// A minimal adapter can explicitly reject execution when its target is gone:
///
/// ```
/// use thorough_but_unreliable::target::*;
///
/// struct UnavailableTarget;
///
/// impl TargetSession for UnavailableTarget {
///     async fn execute(&mut self, request: CommandRequest) -> ExecutionReport {
///         ExecutionReport {
///             sequence: request.sequence,
///             outcome: ExecutionOutcome::NotStarted(StartFailure::SessionUnusable),
///         }
///     }
/// }
///
/// fn accepts_send_future(_: impl std::future::Future<Output = ExecutionReport> + Send) {}
/// let mut session = UnavailableTarget;
/// accepts_send_future(session.execute(CommandRequest {
///     sequence: 1,
///     command: "id".into(),
/// }));
/// ```
pub trait TargetSession {
    fn execute(&mut self, request: CommandRequest) -> impl Future<Output = ExecutionReport> + Send;
}

/// No target selector, identity override, runtime flags, or policy overrides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRequest {
    /// Broker-assigned identifier, scoped to this run. Sequence rules are the
    /// broker protocol's responsibility; arithmetic must never wrap.
    pub sequence: u64,
    /// Shell text interpreted only inside the target. An adapter must reject
    /// unrepresentable input (e.g. NUL in argv) before dispatch, without rewriting it.
    pub command: String,
}

/// Partial output remains available even when completion is unknown.
/// These are internal types, not a serialized HTTP or guest-channel schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionReport {
    pub sequence: u64,
    pub outcome: ExecutionOutcome,
}

impl ExecutionReport {
    pub fn session_state(&self) -> SessionState {
        self.outcome.session_state()
    }
}

/// Output exists only for outcomes where a command may have started.
/// Session usability is derived; uncertain outcomes cannot declare themselves ready.
///
/// An uncertain completion cannot carry a readiness override:
/// ```compile_fail
/// use thorough_but_unreliable::target::*;
/// let outcome = ExecutionOutcome::Unknown {
///     output: CommandOutput::default(),
///     error: ExecutionError {
///         kind: ExecutionErrorKind::Transport,
///         diagnostic: None,
///     },
///     session_state: SessionState::Ready,
/// };
/// ```
/// A command that never started cannot carry captured command output:
/// ```compile_fail
/// use thorough_but_unreliable::target::*;
/// let report = ExecutionReport {
///     sequence: 1,
///     outcome: ExecutionOutcome::NotStarted(StartFailure::SessionUnusable),
///     stdout: CapturedOutput::default(),
/// };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionOutcome {
    Completed {
        output: CommandOutput,
        completion: ProcessCompletion,
        /// A known completion can still leave the session unusable, e.g. when
        /// descendant cleanup or channel health prevents another command.
        session_state: SessionState,
    },
    /// The adapter can establish execution never began. Diagnostics belong to
    /// the error, not command stdout/stderr.
    NotStarted(StartFailure),
    /// The adapter's deadline expired, independently of guest-reported status.
    DeadlineExceeded {
        output: CommandOutput,
        execution_state: ExecutionState,
    },
    /// Execution may have begun; its outcome cannot be established.
    Unknown {
        output: CommandOutput,
        error: ExecutionError,
    },
}

impl ExecutionOutcome {
    /// These types enforce report consistency, not external facts or the
    /// adapter's internal lifecycle. Adapters must still establish observations
    /// and retain unusability after a terminal failure.
    pub fn session_state(&self) -> SessionState {
        match self {
            Self::Completed { session_state, .. }
            | Self::DeadlineExceeded {
                execution_state: ExecutionState::ConfirmedStopped { session_state },
                ..
            } => *session_state,
            Self::NotStarted(failure) => failure.session_state(),
            Self::Unknown { .. }
            | Self::DeadlineExceeded {
                execution_state: ExecutionState::MayStillBeRunning,
                ..
            } => SessionState::Unusable,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapturedOutput {
    /// Raw, untrusted bytes. Encoding and safe display belong to consumers.
    pub bytes: Vec<u8>,
    /// Output was discarded to enforce the capture limit. False does not imply
    /// a complete stream when execution or transport failed.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessCompletion {
    Exited {
        /// Exit status of the target's POSIX shell/process.
        code: u8,
        source: CompletionSource,
    },
    Signaled {
        /// Positive signal number; the adapter additionally validates numbers
        /// against its target platform when decoding external data.
        signal: NonZeroU32,
        source: CompletionSource,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionSource {
    /// The parent execution mechanism observed command completion, not merely
    /// termination of a transport/helper process. Output remains untrusted.
    ParentObserved,
    /// The guest reported completion. This does not establish its truthfulness.
    GuestReported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionState {
    /// External supervision confirmed execution stopped, including descendants
    /// covered by the policy. A guest acknowledgment alone is insufficient.
    ConfirmedStopped {
        /// Confirmed termination does not necessarily leave a usable target.
        session_state: SessionState,
    },
    /// Execution, including descendants, may continue beyond the deadline.
    MayStillBeRunning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// The channel is synchronized and policy permits another command. This is
    /// not a claim that the target is uncompromised or that no descendants exist.
    Ready,
    /// Terminal for this session; recovery requires supervision and a new session.
    Unusable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartFailure {
    /// Invalid input rejected without dispatch or damage to a healthy session.
    /// An already unusable session must return `SessionUnusable` instead.
    Rejected(CommandRejection),
    SessionUnusable,
    /// A failure known to precede execution. The session must be discarded.
    /// Use `ExecutionOutcome::Unknown` whenever non-execution is uncertain.
    Failed(ExecutionError),
}

impl StartFailure {
    pub fn session_state(&self) -> SessionState {
        match self {
            Self::Rejected(_) => SessionState::Ready,
            Self::SessionUnusable | Self::Failed(_) => SessionState::Unusable,
        }
    }
}

/// Rejections cannot describe dispatch, transport, or dependency-panic failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRejection {
    pub kind: RejectionKind,
    /// Bounded explanation without credentials or parent secrets.
    pub diagnostic: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionKind {
    InvalidCommand,
    CommandTooLarge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionError {
    pub kind: ExecutionErrorKind,
    /// Optional explanation, never a machine-readable control instruction.
    /// Adapters must bound diagnostics and omit credentials and parent secrets.
    pub diagnostic: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionErrorKind {
    TargetUnavailable,
    Transport,
    MalformedResponse,
    ExecutionMechanism,
    ResourceExhausted,
    DependencyPanicked,
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "target execution error: {:?}", self.kind)?;
        if let Some(diagnostic) = &self.diagnostic {
            write!(f, ": {diagnostic}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ExecutionError {}

#[cfg(test)]
mod tests;
