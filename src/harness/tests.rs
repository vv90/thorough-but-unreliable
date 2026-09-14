use std::collections::VecDeque;

use proptest::{prelude::*, test_runner::TestCaseError};

use super::{
    driver::{CommandExecutor, ModelClient, run},
    model_loop::{Step, start},
    types::*,
};

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn command_result() -> impl Strategy<Value = CommandResult> {
    (
        text(),
        text(),
        prop_oneof![
            any::<i32>().prop_map(|code| CommandStatus::Exited { code }),
            any::<i32>().prop_map(|signal| CommandStatus::Signaled { signal }),
            Just(CommandStatus::TimedOut),
        ],
    )
        .prop_map(|(stdout, stderr, status)| CommandResult {
            stdout,
            stderr,
            status,
        })
}

fn failure() -> impl Strategy<Value = DependencyError> {
    prop_oneof![
        text().prop_map(DependencyError::Failed),
        text().prop_map(DependencyError::CompletionUnknown),
        Just(DependencyError::Panicked),
    ]
}

fn commands(values: &[String]) -> AssistantResponse {
    AssistantResponse {
        text: Some("Commands requested".into()),
        tool_calls: values
            .iter()
            .enumerate()
            .map(|(index, command)| ToolCall {
                id: format!("call-{index}"),
                tool: Tool::ExecuteTargetCommand {
                    command: command.clone(),
                },
            })
            .collect(),
    }
}

fn submission(answer: String) -> AssistantResponse {
    AssistantResponse {
        text: None,
        tool_calls: vec![ToolCall {
            id: "submit".into(),
            tool: Tool::Submit { answer },
        }],
    }
}

#[derive(Default)]
struct ScriptedModel {
    responses: VecDeque<Result<AssistantResponse, DependencyError>>,
    histories: Vec<Vec<Message>>,
}

impl ModelClient for ScriptedModel {
    fn respond(&mut self, history: &[Message]) -> Result<AssistantResponse, DependencyError> {
        self.histories.push(history.to_vec());
        self.responses
            .pop_front()
            .ok_or_else(|| DependencyError::Failed("model script exhausted".into()))?
    }
}

#[derive(Default)]
struct ScriptedExecutor {
    results: VecDeque<Result<CommandResult, DependencyError>>,
    calls: Vec<CommandCall>,
}

impl CommandExecutor for ScriptedExecutor {
    fn execute(&mut self, call: &CommandCall) -> Result<CommandResult, DependencyError> {
        self.calls.push(call.clone());
        self.results
            .pop_front()
            .ok_or_else(|| DependencyError::Failed("executor script exhausted".into()))?
    }
}

fn model_step(step: Step) -> Result<super::model_loop::ModelTurn, TestCaseError> {
    match step {
        Step::Model(turn) => Ok(turn),
        other => Err(TestCaseError::fail(format!(
            "expected model step, got {other:?}"
        ))),
    }
}

fn finished(step: Step) -> Result<RunReport, TestCaseError> {
    match step {
        Step::Finished(report) => Ok(report),
        other => Err(TestCaseError::fail(format!(
            "expected terminal step, got {other:?}"
        ))),
    }
}

proptest! {
    #[test]
    fn ordered_commands_produce_correlated_results_before_next_model_turn(
        prompt in text(), task in text(),
        batch in prop::collection::vec((text(), command_result()), 1..16),
    ) {
        let response = commands(&batch.iter().map(|(cmd, _)| cmd.clone()).collect::<Vec<_>>());
        let mut expected = vec![Message::System(prompt.clone()), Message::User(task.clone())];
        let turn = model_step(start(prompt, task, 2))?;
        prop_assert_eq!(turn.history(), &expected);
        let mut step = turn.complete(Ok(response.clone()));
        expected.push(Message::Assistant(response));

        for (index, (command, result)) in batch.into_iter().enumerate() {
            let Step::Command(turn) = step else {
                return Err(TestCaseError::fail("batch must finish before another model turn"));
            };
            let id = format!("call-{index}");
            prop_assert_eq!(turn.call(), &CommandCall { id: id.clone(), command });
            expected.push(Message::Tool { call_id: id, result: Ok(result.clone()) });
            step = turn.complete(Ok(result));
        }
        let next_turn = model_step(step)?;
        prop_assert_eq!(next_turn.history(), &expected);
    }

    #[test]
    fn submission_preserves_answer_and_response_on_last_allowed_turn(
        answer in text(), assistant_text in prop::option::of(text()),
    ) {
        let mut response = submission(answer.clone());
        response.text = assistant_text;
        let turn = model_step(start("system".into(), "task".into(), 1))?;
        let report = finished(turn.complete(Ok(response.clone())))?;
        prop_assert_eq!(report.outcome, RunOutcome::Submitted { answer });
        prop_assert_eq!(report.history.last(), Some(&Message::Assistant(response)));
    }

    #[test]
    fn text_without_tools_is_early_termination(assistant_text in prop::option::of(text())) {
        let response = AssistantResponse { text: assistant_text, tool_calls: vec![] };
        let turn = model_step(start("system".into(), "task".into(), 1))?;
        let report = finished(turn.complete(Ok(response.clone())))?;
        prop_assert_eq!(report.outcome, RunOutcome::EarlyTermination);
        prop_assert_eq!(report.history.last(), Some(&Message::Assistant(response)));
    }

    #[test]
    fn invalid_response_is_recorded_and_executes_nothing(
        values in prop::collection::vec(text(), 1..16),
        answer in text(), kind in 0u8..4, invalid_first in any::<bool>(),
    ) {
        let mut response = commands(&values);
        let (bad_call, expected_error) = match kind {
            0 => (ToolCall { id: String::new(), tool: Tool::ExecuteTargetCommand { command: answer } }, ProtocolError::EmptyCallId),
            1 => (ToolCall { id: "call-0".into(), tool: Tool::ExecuteTargetCommand { command: answer } }, ProtocolError::DuplicateCallId("call-0".into())),
            2 => (ToolCall { id: "submit".into(), tool: Tool::Submit { answer } }, ProtocolError::MixedSubmissionAndCommands),
            _ => {
                response = submission("first".into());
                (ToolCall { id: "second-submit".into(), tool: Tool::Submit { answer } }, ProtocolError::MultipleSubmissions)
            }
        };
        response.tool_calls.push(bad_call);
        if invalid_first {
            response.tool_calls.reverse();
        }
        let mut model = ScriptedModel { responses: VecDeque::from([Ok(response.clone())]), ..Default::default() };
        let mut executor = ScriptedExecutor::default();
        let report = run("system".into(), "task".into(), 10, &mut model, &mut executor);
        prop_assert_eq!(report.outcome, RunOutcome::ProtocolError(expected_error));
        prop_assert!(executor.calls.is_empty());
        prop_assert_eq!(model.histories.len(), 1);
        prop_assert_eq!(report.history.last(), Some(&Message::Assistant(response)));
    }

    #[test]
    fn turn_budget_bounds_model_calls_and_finishes_the_last_command_batch(
        budget in 0u32..16,
        batch in prop::collection::vec((text(), command_result()), 1..8),
    ) {
        let response = commands(&batch.iter().map(|(cmd, _)| cmd.clone()).collect::<Vec<_>>());
        let mut model = ScriptedModel::default();
        let mut executor = ScriptedExecutor::default();
        let mut expected_calls = Vec::new();
        for _ in 0..budget {
            model.responses.push_back(Ok(response.clone()));
            for (index, (command, result)) in batch.iter().enumerate() {
                executor.results.push_back(Ok(result.clone()));
                expected_calls.push(CommandCall { id: format!("call-{index}"), command: command.clone() });
            }
        }
        let report = run("system".into(), "task".into(), budget, &mut model, &mut executor);
        prop_assert_eq!(report.outcome, RunOutcome::ModelTurnLimit);
        prop_assert_eq!(u32::try_from(model.histories.len()).ok(), Some(budget));
        prop_assert_eq!(executor.calls, expected_calls);
        prop_assert!(executor.results.is_empty());
        prop_assert!(model.responses.is_empty());
        if budget == 0 {
            prop_assert_eq!(report.history, vec![Message::System("system".into()), Message::User("task".into())]);
        }
    }

    #[test]
    fn command_failure_records_uncertainty_and_stops_without_retry_or_later_effects(
        before in prop::collection::vec(text(), 0..8),
        after in prop::collection::vec(text(), 0..8),
        failed_command in text(), error in failure(), result in command_result(),
    ) {
        let mut values = before.clone();
        values.push(failed_command.clone());
        values.extend(after);
        let response = commands(&values);
        let mut model = ScriptedModel { responses: VecDeque::from([Ok(response.clone())]), ..Default::default() };
        let mut executor = ScriptedExecutor::default();
        let mut expected_calls = Vec::new();
        let mut expected_history = vec![Message::System("system".into()), Message::User("task".into()), Message::Assistant(response)];
        for (index, command) in before.iter().enumerate() {
            let id = format!("call-{index}");
            executor.results.push_back(Ok(result.clone()));
            expected_calls.push(CommandCall { id: id.clone(), command: command.clone() });
            expected_history.push(Message::Tool { call_id: id, result: Ok(result.clone()) });
        }
        let failed_id = format!("call-{}", before.len());
        executor.results.push_back(Err(error.clone()));
        expected_calls.push(CommandCall { id: failed_id.clone(), command: failed_command });
        expected_history.push(Message::Tool { call_id: failed_id.clone(), result: Err(error.clone()) });
        let report = run("system".into(), "task".into(), 10, &mut model, &mut executor);
        prop_assert_eq!(report.outcome, RunOutcome::DependencyFailure { operation: Operation::Command { call_id: failed_id }, error });
        prop_assert_eq!(report.history, expected_history);
        prop_assert_eq!(executor.calls, expected_calls);
        prop_assert_eq!(model.histories.len(), 1);
    }

    #[test]
    fn model_failure_preserves_completed_history_and_is_never_retried(
        command in text(), result in command_result(), error in failure(),
    ) {
        let response = commands(&[command]);
        let mut model = ScriptedModel { responses: VecDeque::from([Ok(response.clone()), Err(error.clone())]), ..Default::default() };
        let mut executor = ScriptedExecutor { results: VecDeque::from([Ok(result.clone())]), ..Default::default() };
        let report = run("system".into(), "task".into(), 10, &mut model, &mut executor);
        prop_assert_eq!(report.outcome, RunOutcome::DependencyFailure { operation: Operation::Model, error });
        prop_assert_eq!(model.histories.len(), 2);
        prop_assert_eq!(executor.calls.len(), 1);
        prop_assert_eq!(report.history, vec![Message::System("system".into()), Message::User("task".into()), Message::Assistant(response), Message::Tool { call_id: "call-0".into(), result: Ok(result) }]);
    }

    #[test]
    fn multi_turn_run_preserves_every_message_and_replays_deterministically(
        prompt in text(), task in text(), answer in text(),
        batches in prop::collection::vec(prop::collection::vec((text(), command_result()), 1..6), 0..6),
    ) {
        let mut responses = VecDeque::new();
        let mut results = VecDeque::new();
        let mut expected_history = vec![Message::System(prompt.clone()), Message::User(task.clone())];
        let mut expected_inputs = Vec::new();
        for batch in batches {
            expected_inputs.push(expected_history.clone());
            let response = commands(&batch.iter().map(|(cmd, _)| cmd.clone()).collect::<Vec<_>>());
            responses.push_back(Ok(response.clone()));
            expected_history.push(Message::Assistant(response));
            for (index, (_, result)) in batch.into_iter().enumerate() {
                results.push_back(Ok(result.clone()));
                expected_history.push(Message::Tool { call_id: format!("call-{index}"), result: Ok(result) });
            }
        }
        expected_inputs.push(expected_history.clone());
        let response = submission(answer.clone());
        responses.push_back(Ok(response.clone()));
        expected_history.push(Message::Assistant(response));
        let mut first_model = ScriptedModel { responses: responses.clone(), ..Default::default() };
        let mut first_executor = ScriptedExecutor { results: results.clone(), ..Default::default() };
        let mut second_model = ScriptedModel { responses, ..Default::default() };
        let mut second_executor = ScriptedExecutor { results, ..Default::default() };
        let first = run(prompt.clone(), task.clone(), u32::MAX, &mut first_model, &mut first_executor);
        let second = run(prompt, task, u32::MAX, &mut second_model, &mut second_executor);
        prop_assert_eq!(&first, &second);
        prop_assert_eq!(first_model.histories, expected_inputs);
        prop_assert_eq!(first_executor.calls, second_executor.calls);
        prop_assert_eq!(first.history, expected_history);
        prop_assert_eq!(first.outcome, RunOutcome::Submitted { answer });
    }
}
