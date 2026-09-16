//! Pure, versioned JSON projection for the executable's final report.
//! Target results use the existing lossless output/provenance presentation.

use serde_json::{Value, json};

use super::{presentation::command_report_view, types::*};

/// Submission says nothing about whether the isolation trial succeeded.
pub fn submitted(outcome: &RunOutcome) -> bool {
    matches!(outcome, RunOutcome::Submitted { .. })
}

pub fn view(run_id: &str, report: &RunReport) -> Value {
    let history: Vec<_> = report.history.iter().map(message).collect();
    json!({"version":1,"run_id":run_id,"outcome":outcome(&report.outcome),"history":history})
}

fn message(message: &Message) -> Value {
    match message {
        Message::System(text) => json!({"role":"system","content":text}),
        Message::User(text) => json!({"role":"user","content":text}),
        Message::Assistant(response) => {
            let calls: Vec<_> = response
                .tool_calls
                .iter()
                .map(|call| {
                    let tool = match &call.tool {
                        Tool::ExecuteTargetCommand { command } => {
                            json!({"kind":"execute_target_command","command":command})
                        }
                        Tool::Submit { answer } => json!({"kind":"submit","answer":answer}),
                    };
                    json!({"id":call.id,"tool":tool})
                })
                .collect();
            json!({"role":"assistant","content":response.text,"tool_calls":calls})
        }
        Message::Tool { call_id, result } => {
            let result = match result {
                Ok(report) => {
                    json!({"kind":"execution_report","report":command_report_view(report)})
                }
                Err(error) => json!({"kind":"command_client_failure","error":command_error(error)}),
            };
            json!({"role":"tool","call_id":call_id.as_str(),"result":result})
        }
    }
}

fn outcome(outcome: &RunOutcome) -> Value {
    match outcome {
        RunOutcome::Submitted { answer } => json!({"kind":"submitted","answer":answer}),
        RunOutcome::EarlyTermination => json!({"kind":"early_termination"}),
        RunOutcome::ModelTurnLimit => json!({"kind":"model_turn_limit"}),
        RunOutcome::ProtocolError(error) => {
            let error = match error {
                ProtocolError::EmptyCallId => json!({"kind":"empty_call_id"}),
                ProtocolError::DuplicateCallId(id) => {
                    json!({"kind":"duplicate_call_id","call_id":id})
                }
                ProtocolError::MultipleSubmissions => json!({"kind":"multiple_submissions"}),
                ProtocolError::MixedSubmissionAndCommands => {
                    json!({"kind":"mixed_submission_and_commands"})
                }
            };
            json!({"kind":"protocol_error","error":error})
        }
        RunOutcome::ModelFailure(error) => {
            json!({"kind":"model_failure","error":model_error(error)})
        }
        RunOutcome::CommandClientFailure { call_id, error } => {
            json!({"kind":"command_client_failure","call_id":call_id.as_str(),"error":command_error(error)})
        }
        RunOutcome::TargetSessionUnusable { call_id } => {
            json!({"kind":"target_session_unusable","call_id":call_id.as_str()})
        }
    }
}

fn model_error(error: &ModelFailure) -> Value {
    use ModelFailureKind as K;
    let category = match &error.kind {
        K::Configuration => json!({"kind":"configuration"}),
        K::InvalidHistory => json!({"kind":"invalid_history"}),
        K::Json => json!({"kind":"json"}),
        K::InvalidResponse => json!({"kind":"invalid_response"}),
        K::UnsupportedTool(name) => json!({"kind":"unsupported_tool","name":name}),
        K::TokenLimit => json!({"kind":"token_limit"}),
        K::ClientBuild => json!({"kind":"client_build"}),
        K::Transport => json!({"kind":"transport"}),
        K::HttpStatus(status) => json!({"kind":"http_status","status":status}),
        K::RequestTooLarge { limit } => json!({"kind":"request_too_large","limit":limit}),
        K::ResponseTooLarge { limit } => json!({"kind":"response_too_large","limit":limit}),
        K::Allocation => json!({"kind":"allocation"}),
        K::Panicked => json!({"kind":"dependency_panicked"}),
    };
    json!({"category":category,"diagnostic":error.diagnostic,"completion_unknown":error.completion_unknown()})
}

fn command_error(error: &CommandClientError) -> Value {
    use CommandClientErrorKind as K;
    let category = match &error.kind {
        K::Configuration => json!({"kind":"configuration"}),
        K::InvalidRequest => json!({"kind":"invalid_request"}),
        K::Allocation => json!({"kind":"allocation"}),
        K::SessionUnavailable => json!({"kind":"session_unavailable"}),
        K::RequestTooLarge { limit } => json!({"kind":"request_too_large","limit":limit}),
        K::Transport => json!({"kind":"transport"}),
        K::HttpStatus(status) => json!({"kind":"http_status","status":status}),
        K::InvalidResponse => json!({"kind":"invalid_response"}),
        K::ResponseTooLarge { limit } => json!({"kind":"response_too_large","limit":limit}),
        K::SequenceMismatch { expected, received } => {
            json!({"kind":"sequence_mismatch","expected":expected.get(),"received":received.get()})
        }
        K::Panicked => json!({"kind":"dependency_panicked"}),
    };
    json!({"category":category,"diagnostic":error.diagnostic,"completion_unknown":error.completion_unknown()})
}

#[cfg(test)]
mod tests;
