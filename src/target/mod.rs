//! Experiment-side command execution contract, independent of transport.
//!
//! Trusted setup prepares one target and binds its identity, execution user,
//! shell, initial working directory, environment, and resource/descendant policy
//! to a session. Setup must fail if an adapter cannot implement that policy.
//! Target creation, reset, and destruction belong to external supervision.
//! The broker owns authentication and sequence validation; adapters own execution.

use std::{fmt, future::Future};

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
///   `NotStarted` with `SessionUnusable`, without dispatching any command.
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
///             stdout: CapturedOutput::default(),
///             stderr: CapturedOutput::default(),
///             completion: Completion::NotStarted {
///                 error: ExecutionError {
///                     kind: ExecutionErrorKind::SessionUnusable,
///                     diagnostic: Some("target is no longer available".into()),
///                 },
///             },
///             session_state: SessionState::Unusable,
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
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
    pub completion: Completion,
    /// Adapters must report `Unusable` after uncertain completion or a protocol
    /// failure and retain that state internally. This field does not enforce it.
    pub session_state: SessionState,
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
pub enum Completion {
    Exited {
        code: i32,
        source: CompletionSource,
    },
    Signaled {
        signal: i32,
        source: CompletionSource,
    },
    /// The adapter's deadline expired, independently of guest-reported status.
    DeadlineExceeded {
        execution_state: ExecutionState,
    },
    /// The adapter can establish that this command did not begin execution.
    NotStarted {
        error: ExecutionError,
    },
    /// Execution may have begun; its outcome cannot be established.
    Unknown {
        error: ExecutionError,
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
    ConfirmedStopped,
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
pub struct ExecutionError {
    pub kind: ExecutionErrorKind,
    /// Optional explanation, never a machine-readable control instruction.
    /// Adapters must bound diagnostics and omit credentials and parent secrets.
    pub diagnostic: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionErrorKind {
    TargetUnavailable,
    InvalidCommand,
    Transport,
    MalformedResponse,
    ExecutionMechanism,
    ResourceExhausted,
    SessionUnusable,
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
