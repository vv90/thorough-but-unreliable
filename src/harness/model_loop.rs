//! Pure, ownership-based transitions. A continuation is consumed by its result;
//! it cannot accept a second result or an input of the wrong kind.

use std::collections::{BTreeSet, VecDeque};

use super::types::{
    AssistantResponse, CommandCall, CommandClientError, Message, ModelFailure, ProtocolError,
    RunOutcome, RunReport, Tool, ToolCallId,
};
use crate::target::{ExecutionReport, SessionState};

#[derive(Debug)]
pub enum Step {
    Model(ModelTurn),
    Command(CommandTurn),
    Finished(RunReport),
}

#[derive(Debug)]
struct Context {
    history: Vec<Message>,
    turns_remaining: u32,
}

impl Context {
    fn finish(self, outcome: RunOutcome) -> Step {
        Step::Finished(RunReport {
            history: self.history,
            outcome,
        })
    }

    fn next_model(self) -> Step {
        if self.turns_remaining == 0 {
            self.finish(RunOutcome::ModelTurnLimit)
        } else {
            Step::Model(ModelTurn { context: self })
        }
    }

    fn next_command(self, mut pending: VecDeque<CommandCall>) -> Step {
        match pending.pop_front() {
            Some(call) => Step::Command(CommandTurn {
                context: self,
                call,
                pending,
            }),
            None => self.next_model(),
        }
    }
}

/// Zero turns finishes immediately. Each accepted model result consumes one
/// turn; all commands in that response complete before the next budget check.
pub fn start(system_prompt: String, task: String, max_model_turns: u32) -> Step {
    Context {
        history: vec![Message::System(system_prompt), Message::User(task)],
        turns_remaining: max_model_turns,
    }
    .next_model()
}

#[derive(Debug)]
pub struct ModelTurn {
    context: Context,
}

impl ModelTurn {
    pub fn history(&self) -> &[Message] {
        &self.context.history
    }

    pub fn complete(self, result: Result<AssistantResponse, ModelFailure>) -> Step {
        let mut context = self.context;
        // ModelTurn is constructed only with a positive remaining budget.
        context.turns_remaining = context.turns_remaining.saturating_sub(1);
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                return context.finish(RunOutcome::ModelFailure(error));
            }
        };

        let decision = match classify(&response) {
            Ok(decision) => decision,
            Err(error) => {
                let reason = match error {
                    ProtocolError::InvalidTool(diagnostic) => diagnostic,
                    ProtocolError::EmptyCallId => "Tool call IDs must not be empty.".into(),
                    ProtocolError::DuplicateCallId(_) => {
                        "Tool call IDs must be unique within a response.".into()
                    }
                    ProtocolError::MultipleSubmissions => "Call submit only once.".into(),
                    ProtocolError::MixedSubmissionAndCommands => {
                        "Call submit separately from execute_target_command.".into()
                    }
                };
                context.history.push(Message::ToolCallCorrection {
                    response,
                    diagnostic: format!("No tools in this response were executed. {reason} Correct the tool calls and try again. execute_target_command requires exactly {{\"command\": string}}; submit requires exactly {{\"answer\": string}}."),
                });
                return context.next_model();
            }
        };
        context.history.push(Message::Assistant(response));
        match decision {
            Decision::EarlyTermination => context.finish(RunOutcome::EarlyTermination),
            Decision::Submit(answer) => context.finish(RunOutcome::Submitted { answer }),
            Decision::Commands(commands) => context.next_command(commands),
        }
    }
}

#[derive(Debug)]
pub struct CommandTurn {
    context: Context,
    call: CommandCall,
    pending: VecDeque<CommandCall>,
}

impl CommandTurn {
    pub fn call(&self) -> &CommandCall {
        &self.call
    }

    pub fn complete(self, result: Result<ExecutionReport, CommandClientError>) -> Step {
        let mut context = self.context;
        let terminal = match &result {
            Err(error) => Some(RunOutcome::CommandClientFailure {
                call_id: self.call.id.clone(),
                error: error.clone(),
            }),
            Ok(report) if report.session_state() == SessionState::Unusable => {
                Some(RunOutcome::TargetSessionUnusable {
                    call_id: self.call.id.clone(),
                })
            }
            Ok(_) => None,
        };
        context.history.push(Message::Tool {
            call_id: self.call.id.clone(),
            result,
        });
        match terminal {
            Some(outcome) => context.finish(outcome),
            None => context.next_command(self.pending),
        }
    }
}

enum Decision {
    EarlyTermination,
    Submit(String),
    Commands(VecDeque<CommandCall>),
}

/// Validate the whole response before permitting any command effects. Call IDs
/// must be nonempty and unique within this response.
fn classify(response: &AssistantResponse) -> Result<Decision, ProtocolError> {
    let mut ids = BTreeSet::new();
    let mut answer = None;
    let mut commands = VecDeque::new();
    for call in &response.tool_calls {
        let id = ToolCallId::try_from(call.id.clone()).map_err(|_| ProtocolError::EmptyCallId)?;
        if !ids.insert(&call.id) {
            return Err(ProtocolError::DuplicateCallId(call.id.clone()));
        }
        match &call.tool {
            Tool::Invalid { diagnostic, .. } => {
                return Err(ProtocolError::InvalidTool(diagnostic.clone()));
            }
            Tool::ExecuteTargetCommand { command } => commands.push_back(CommandCall {
                id,
                command: command.clone(),
            }),
            Tool::Submit { answer: submitted } => {
                if answer.replace(submitted.clone()).is_some() {
                    return Err(ProtocolError::MultipleSubmissions);
                }
            }
        }
    }
    match answer {
        Some(_) if !commands.is_empty() => Err(ProtocolError::MixedSubmissionAndCommands),
        Some(answer) => Ok(Decision::Submit(answer)),
        None if commands.is_empty() => Ok(Decision::EarlyTermination),
        None => Ok(Decision::Commands(commands)),
    }
}
