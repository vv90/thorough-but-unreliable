use super::*;
use crate::target::*;
use proptest::prelude::*;

proptest! {
    #[test]
    fn report_preserves_text_call_order_and_binary_output(
        run_id in any::<String>(), text in any::<String>(), answer in any::<String>(),
        commands in prop::collection::vec(any::<String>(), 0..12),
        bytes in prop::collection::vec(any::<u8>(), 0..512),
        sequence in any::<u64>(), truncated in any::<bool>(), code in any::<u8>(), runtime in any::<bool>(),
    ) {
        let id = ToolCallId::try_from("call".to_owned())?;
        let report = RunReport {
            history: vec![Message::System(text.clone()), Message::User(text.clone()),
                Message::Assistant(AssistantResponse { text: Some(text.clone()), tool_calls: commands.iter().map(|command| ToolCall {
                    id: text.clone(), tool: Tool::ExecuteTargetCommand { command: command.clone() },
                }).collect() }),
                Message::Tool { call_id: id, result: Ok(ExecutionReport {
                    sequence: CommandSequence::new(sequence),
                    outcome: ExecutionOutcome::Completed {
                        output: CommandOutput { stdout: CapturedOutput { bytes: bytes.clone(), truncated }, stderr: CapturedOutput::default() },
                        completion: if runtime { ProcessCompletion::RuntimeStatus { code, source: CompletionSource::GuestReported } }
                            else { ProcessCompletion::Exited { code, source: CompletionSource::GuestReported } },
                        session_state: SessionState::Unusable,
                    },
                }) },
            ],
            outcome: RunOutcome::Submitted { answer: answer.clone() },
        };
        let value: Value = serde_json::from_slice(&serde_json::to_vec(&view(&run_id, &report))?)?;
        prop_assert_eq!(value.get("run_id"), Some(&json!(run_id)));
        prop_assert_eq!(value.pointer("/outcome/answer"), Some(&json!(answer)));
        for position in [0, 1, 2] {
            prop_assert_eq!(value.pointer(&format!("/history/{position}/content")), Some(&json!(text)));
        }
        let calls = value.pointer("/history/2/tool_calls").and_then(Value::as_array).ok_or_else(|| TestCaseError::fail("missing calls"))?;
        prop_assert_eq!(calls.len(), commands.len());
        for (call, command) in calls.iter().zip(commands) {
            prop_assert_eq!(call.get("id"), Some(&json!(text)));
            prop_assert_eq!(call.pointer("/tool/command"), Some(&json!(command)));
        }
        let execution = value.pointer("/history/3/result/report").ok_or_else(|| TestCaseError::fail("missing report"))?;
        prop_assert_eq!(execution.get("sequence"), Some(&json!(sequence)));
        prop_assert_eq!(execution.get("session_state"), Some(&json!("unusable")));
        prop_assert_eq!(execution.pointer("/outcome/completion"), Some(&json!({"kind":if runtime { "runtime_status" } else { "exited" },"code":code,"source":"guest_reported"})));
        let output = execution.pointer("/outcome/output/stdout").ok_or_else(|| TestCaseError::fail("missing stdout"))?;
        let expected = match std::str::from_utf8(&bytes) {
            Ok(text) => json!({"encoding":"utf8","data":text,"truncated":truncated}),
            Err(_) => json!({"encoding":"hex","data":bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),"truncated":truncated}),
        };
        prop_assert_eq!(output, &expected);
    }

    #[test]
    fn failure_payloads_and_uncertainty_survive_projection(
        diagnostic in prop::option::of(any::<String>()), status in any::<u16>(),
        limit in any::<usize>(), expected in any::<u64>(), received in any::<u64>(),
    ) {
        let model = ModelFailure { kind: ModelFailureKind::HttpStatus(status), diagnostic: diagnostic.clone() };
        prop_assert_eq!(model_error(&model), json!({"category":{"kind":"http_status","status":status},"diagnostic":diagnostic,"completion_unknown":false}));
        let command = CommandClientError { kind: CommandClientErrorKind::SequenceMismatch {
            expected: CommandSequence::new(expected), received: CommandSequence::new(received) }, diagnostic: diagnostic.clone() };
        prop_assert_eq!(command_error(&command), json!({"category":{"kind":"sequence_mismatch","expected":expected,"received":received},"diagnostic":diagnostic,"completion_unknown":true}));
        let command = CommandClientError { kind: CommandClientErrorKind::RequestTooLarge { limit }, diagnostic: diagnostic.clone() };
        prop_assert_eq!(command_error(&command), json!({"category":{"kind":"request_too_large","limit":limit},"diagnostic":diagnostic,"completion_unknown":false}));
    }
}

#[test]
fn only_submission_maps_to_success_and_all_terminal_kinds_are_distinct()
-> Result<(), EmptyToolCallId> {
    let mut kinds = std::collections::BTreeSet::new();
    for (result, kind, success) in [
        (
            RunOutcome::Submitted {
                answer: "done".into(),
            },
            "submitted",
            true,
        ),
        (RunOutcome::EarlyTermination, "early_termination", false),
        (RunOutcome::ModelTurnLimit, "model_turn_limit", false),
        (
            RunOutcome::ProtocolError(ProtocolError::DuplicateCallId("same".into())),
            "protocol_error",
            false,
        ),
        (
            RunOutcome::ModelFailure(ModelFailure::panicked()),
            "model_failure",
            false,
        ),
        (
            RunOutcome::CommandClientFailure {
                call_id: ToolCallId::try_from("call".to_owned())?,
                error: CommandClientError::panicked(),
            },
            "command_client_failure",
            false,
        ),
        (
            RunOutcome::TargetSessionUnusable {
                call_id: ToolCallId::try_from("call".to_owned())?,
            },
            "target_session_unusable",
            false,
        ),
    ] {
        assert_eq!(submitted(&result), success);
        assert_eq!(outcome(&result).get("kind"), Some(&json!(kind)));
        assert!(kinds.insert(kind));
    }
    Ok(())
}
