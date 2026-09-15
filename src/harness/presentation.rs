//! Pure model-facing projection. The transcript retains the original report.

use serde_json::{Value, json};

use crate::target::{
    CapturedOutput, CommandOutput, CompletionSource, ExecutionError, ExecutionErrorKind,
    ExecutionOutcome, ExecutionReport, ExecutionState, ProcessCompletion, RejectionKind,
    SessionState, StartFailure,
};

/// Valid UTF-8 is displayed as text; other bytes are losslessly represented as
/// hex. Explicit encoding/truncation metadata prevents silent output corruption.
fn output_view(output: &CapturedOutput) -> Value {
    let (encoding, data) = match std::str::from_utf8(&output.bytes) {
        Ok(text) => ("utf8", text.to_owned()),
        Err(_) => (
            "hex",
            output
                .bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        ),
    };
    json!({ "encoding": encoding, "data": data, "truncated": output.truncated })
}

fn output_fields(output: &CommandOutput) -> Value {
    json!({ "stdout": output_view(&output.stdout), "stderr": output_view(&output.stderr) })
}

fn error_view(error: &ExecutionError) -> Value {
    let kind = match error.kind {
        ExecutionErrorKind::TargetUnavailable => "target_unavailable",
        ExecutionErrorKind::Transport => "transport",
        ExecutionErrorKind::MalformedResponse => "malformed_response",
        ExecutionErrorKind::ExecutionMechanism => "execution_mechanism",
        ExecutionErrorKind::ResourceExhausted => "resource_exhausted",
        ExecutionErrorKind::DependencyPanicked => "dependency_panicked",
    };
    json!({ "kind": kind, "diagnostic": error.diagnostic })
}

/// This projection is deliberately separate from the future broker wire format.
/// It accepts terminal reports for inspection, though the loop will not send a
/// further model request after a terminal outcome.
pub fn command_report_view(report: &ExecutionReport) -> Value {
    let outcome = match &report.outcome {
        ExecutionOutcome::Completed {
            output, completion, ..
        } => {
            let completion = match completion {
                ProcessCompletion::Exited { code, source } => {
                    json!({ "kind": "exited", "code": code, "source": source_name(*source) })
                }
                ProcessCompletion::Signaled { signal, source } => {
                    json!({ "kind": "signaled", "signal": signal.get(), "source": source_name(*source) })
                }
            };
            json!({ "kind": "completed", "output": output_fields(output), "completion": completion })
        }
        ExecutionOutcome::NotStarted(failure) => {
            let error = match failure {
                StartFailure::Rejected(rejection) => {
                    let kind = match rejection.kind {
                        RejectionKind::InvalidCommand => "invalid_command",
                        RejectionKind::CommandTooLarge => "command_too_large",
                    };
                    json!({ "kind": kind, "diagnostic": rejection.diagnostic })
                }
                StartFailure::SessionUnusable => json!({ "kind": "session_unusable" }),
                StartFailure::Failed(error) => error_view(error),
            };
            json!({ "kind": "not_started", "error": error })
        }
        ExecutionOutcome::DeadlineExceeded {
            output,
            execution_state,
        } => {
            let state = match execution_state {
                ExecutionState::ConfirmedStopped { .. } => "confirmed_stopped",
                ExecutionState::MayStillBeRunning => "may_still_be_running",
            };
            json!({ "kind": "deadline_exceeded", "output": output_fields(output), "execution_state": state })
        }
        ExecutionOutcome::Unknown { output, error } => {
            json!({ "kind": "unknown", "output": output_fields(output), "error": error_view(error) })
        }
    };
    let state = match report.session_state() {
        SessionState::Ready => "ready",
        SessionState::Unusable => "unusable",
    };
    json!({ "sequence": report.sequence.get(), "session_state": state, "outcome": outcome })
}

fn source_name(source: CompletionSource) -> &'static str {
    match source {
        CompletionSource::ParentObserved => "parent_observed",
        CompletionSource::GuestReported => "guest_reported",
    }
}
