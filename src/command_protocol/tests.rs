use super::*;
use crate::target::*;
use proptest::prelude::*;
use serde_json::{Value, json};

const LIMIT: usize = 64 * 1024;

fn output() -> impl Strategy<Value = CommandOutput> {
    (
        prop::collection::vec(any::<u8>(), 0..128),
        prop::collection::vec(any::<u8>(), 0..128),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(stdout, stderr, out_truncated, err_truncated)| CommandOutput {
                stdout: CapturedOutput {
                    bytes: stdout,
                    truncated: out_truncated,
                },
                stderr: CapturedOutput {
                    bytes: stderr,
                    truncated: err_truncated,
                },
            },
        )
}
fn error() -> impl Strategy<Value = ExecutionError> {
    (
        prop_oneof![
            Just(ExecutionErrorKind::TargetUnavailable),
            Just(ExecutionErrorKind::Transport),
            Just(ExecutionErrorKind::MalformedResponse),
            Just(ExecutionErrorKind::ExecutionMechanism),
            Just(ExecutionErrorKind::ResourceExhausted),
            Just(ExecutionErrorKind::DependencyPanicked),
        ],
        prop::option::of(any::<String>()),
    )
        .prop_map(|(kind, diagnostic)| ExecutionError { kind, diagnostic })
}
fn outcome() -> impl Strategy<Value = ExecutionOutcome> {
    let state = || prop_oneof![Just(SessionState::Ready), Just(SessionState::Unusable)];
    let source = || {
        prop_oneof![
            Just(CompletionSource::ParentObserved),
            Just(CompletionSource::GuestReported)
        ]
    };
    prop_oneof![
        (output(), any::<u8>(), source(), state()).prop_map(
            |(output, code, source, session_state)| ExecutionOutcome::Completed {
                output,
                completion: ProcessCompletion::Exited { code, source },
                session_state,
            }
        ),
        (output(), any::<std::num::NonZeroU32>(), source(), state()).prop_map(
            |(output, signal, source, session_state)| ExecutionOutcome::Completed {
                output,
                completion: ProcessCompletion::Signaled { signal, source },
                session_state,
            }
        ),
        (output(), state()).prop_map(|(output, session_state)| {
            ExecutionOutcome::DeadlineExceeded {
                output,
                execution_state: ExecutionState::ConfirmedStopped { session_state },
            }
        }),
        output().prop_map(|output| ExecutionOutcome::DeadlineExceeded {
            output,
            execution_state: ExecutionState::MayStillBeRunning
        }),
        (output(), error()).prop_map(|(output, error)| ExecutionOutcome::Unknown { output, error }),
        error().prop_map(|error| ExecutionOutcome::NotStarted(StartFailure::Failed(error))),
        Just(ExecutionOutcome::NotStarted(StartFailure::SessionUnusable)),
        (any::<bool>(), prop::option::of(any::<String>())).prop_map(|(large, diagnostic)| {
            ExecutionOutcome::NotStarted(StartFailure::Rejected(CommandRejection {
                kind: if large {
                    RejectionKind::CommandTooLarge
                } else {
                    RejectionKind::InvalidCommand
                },
                diagnostic,
            }))
        }),
    ]
}
fn ready(sequence: CommandSequence) -> ExecutionReport {
    ExecutionReport {
        sequence,
        outcome: ExecutionOutcome::Completed {
            output: CommandOutput::default(),
            completion: ProcessCompletion::Exited {
                code: 0,
                source: CompletionSource::ParentObserved,
            },
            session_state: SessionState::Ready,
        },
    }
}

fn object_paths(value: &Value, path: &str, paths: &mut Vec<String>) {
    if let Value::Object(fields) = value {
        paths.push(path.to_owned());
        for (name, nested) in fields {
            // All keys here come from the fixed wire schema.
            object_paths(nested, &format!("{path}/{name}"), paths);
        }
    }
}

proptest! {
    #[test]
    fn requests_preserve_exact_commands_and_enforce_encoded_byte_limits(command in any::<String>(), sequence in any::<u64>()) {
        let request = CommandRequest { sequence: CommandSequence::new(sequence), command };
        let bytes = encode_request(&request, LIMIT)?;
        prop_assert_eq!(decode_request(&bytes, bytes.len())?, request.clone());
        prop_assert_eq!(encode_request(&request, bytes.len())?, bytes.clone());
        let smaller = bytes.len().checked_sub(1).ok_or_else(|| proptest::test_runner::TestCaseError::fail("unexpected empty JSON"))?;
        prop_assert!(matches!(encode_request(&request, smaller), Err(ProtocolError::BodyTooLarge { .. })), "encoding exceeded limit");
        prop_assert!(matches!(decode_request(&bytes, smaller), Err(ProtocolError::BodyTooLarge { .. })), "decoding exceeded limit");
    }

    #[test]
    fn all_report_variants_round_trip_losslessly_with_exact_limits(sequence in any::<u64>(), outcome in outcome()) {
        let report = ExecutionReport { sequence: CommandSequence::new(sequence), outcome };
        let bytes = encode_report(&report, LIMIT)?;
        prop_assert_eq!(decode_report(&bytes, bytes.len(), report.sequence)?, report.clone());
        prop_assert_eq!(encode_report(&report, bytes.len())?, bytes.clone());
        let smaller = bytes.len().checked_sub(1).ok_or_else(|| proptest::test_runner::TestCaseError::fail("unexpected empty JSON"))?;
        prop_assert!(matches!(encode_report(&report, smaller), Err(ProtocolError::BodyTooLarge { .. })), "encoding exceeded limit");
        prop_assert!(matches!(decode_report(&bytes, smaller, report.sequence), Err(ProtocolError::BodyTooLarge { .. })), "decoding exceeded limit");
        let other = CommandSequence::new(sequence ^ 1);
        let mismatch = decode_report(&bytes, LIMIT, other);
        prop_assert!(matches!(mismatch, Err(ProtocolError::SequenceMismatch { expected, received }) if expected == other && received == report.sequence), "accepted wrong sequence");
    }

    #[test]
    fn every_nested_report_object_rejects_extra_fields_arrays_and_missing_required_fields(outcome in outcome()) {
        let sequence = CommandSequence::new(1);
        let report = ExecutionReport { sequence, outcome };
        let base: Value = serde_json::from_slice(&encode_report(&report, LIMIT)?)?;
        let mut paths = Vec::new();
        object_paths(&base, "", &mut paths);
        for path in paths {
            let fields = base.pointer(&path).and_then(Value::as_object)
                .ok_or_else(|| proptest::test_runner::TestCaseError::fail("expected object"))?;
            let mut extra = fields.clone();
            extra.insert("unexpected".into(), Value::Null);
            let mut mutations = vec![Value::Object(extra), Value::Array(fields.values().cloned().collect())];
            for key in fields.keys().filter(|key| key.as_str() != "diagnostic") {
                let mut missing = fields.clone();
                missing.remove(key);
                mutations.push(Value::Object(missing));
            }
            for replacement in mutations {
                let mut value = base.clone();
                *value.pointer_mut(&path).ok_or_else(|| proptest::test_runner::TestCaseError::fail("invalid path"))? = replacement;
                let result = decode_report(&serde_json::to_vec(&value)?, LIMIT, sequence);
                prop_assert!(matches!(result, Err(ProtocolError::Json(_))), "accepted mutation: {}", value);
            }
        }
    }

    #[test]
    fn only_expected_sequences_are_admitted_and_only_ready_reports_advance(start in any::<u64>(), outcome in outcome()) {
        let expected = CommandSequence::new(start);
        let wrong = CommandSequence::new(start ^ 1);
        let mut tracker = SequenceTracker { state: SequenceState::Ready(expected) };
        prop_assert_eq!(tracker.finish(&ready(expected)), Err(SequenceError::NoOutstanding));
        prop_assert_eq!(tracker.begin(wrong), Err(SequenceError::Mismatch { expected, received: wrong }));
        prop_assert_eq!(tracker.next_sequence(), Some(expected));
        tracker.begin(expected)?;
        prop_assert_eq!(tracker.next_sequence(), None);
        prop_assert_eq!(tracker.begin(expected), Err(SequenceError::Outstanding));
        let report = ExecutionReport { sequence: expected, outcome };
        tracker.finish(&report)?;
        if report.session_state() == SessionState::Ready {
            prop_assert_eq!(tracker.next_sequence(), expected.checked_next());
            if let Some(next) = expected.checked_next() {
                prop_assert_eq!(tracker.begin(expected), Err(SequenceError::Mismatch { expected: next, received: expected }));
                tracker.begin(next)?;
            } else {
                prop_assert_eq!(tracker.begin(expected), Err(SequenceError::Exhausted));
            }
        } else {
            prop_assert_eq!(tracker.next_sequence(), None);
            prop_assert_eq!(tracker.begin(wrong), Err(SequenceError::Unusable));
            prop_assert_eq!(tracker.finish(&ready(expected)), Err(SequenceError::Unusable));
        }
    }

    #[test]
    fn mismatched_reports_and_abandonment_permanently_prevent_reuse(start in any::<u64>(), later in any::<u64>()) {
        let sequence = CommandSequence::new(start);
        let mut tracker = SequenceTracker { state: SequenceState::Ready(sequence) };
        tracker.begin(sequence)?;
        let other = CommandSequence::new(start ^ 1);
        prop_assert_eq!(tracker.finish(&ready(other)), Err(SequenceError::Mismatch { expected: sequence, received: other }));
        prop_assert_eq!(tracker.begin(CommandSequence::new(later)), Err(SequenceError::Unusable));
        prop_assert_eq!(tracker.finish(&ready(sequence)), Err(SequenceError::Unusable));
        let mut tracker = SequenceTracker::new();
        tracker.abandon();
        prop_assert_eq!(tracker.begin(CommandSequence::new(later)), Err(SequenceError::Unusable));
        prop_assert_eq!(tracker.finish(&ready(sequence)), Err(SequenceError::Unusable));
    }

    #[test]
    fn arbitrary_input_never_unwinds_or_bypasses_bounds(bytes in prop::collection::vec(any::<u8>(), 0..1024), limit in 0usize..1024) {
        let request = decode_request(&bytes, limit);
        let report = decode_report(&bytes, limit, CommandSequence::new(1));
        prop_assert!(!matches!(request, Err(ProtocolError::DependencyPanicked)));
        prop_assert!(!matches!(report, Err(ProtocolError::DependencyPanicked)));
        if bytes.len() > limit {
            prop_assert!(matches!(request, Err(ProtocolError::BodyTooLarge { .. })), "oversized request");
            prop_assert!(matches!(report, Err(ProtocolError::BodyTooLarge { .. })), "oversized report");
        }
    }
}

#[test]
fn sequence_starts_at_one_and_exhausts_without_wrapping() -> Result<(), Box<dyn std::error::Error>>
{
    assert_eq!(
        SequenceTracker::new().next_sequence(),
        Some(CommandSequence::new(1))
    );
    let last = CommandSequence::new(u64::MAX);
    let mut tracker = SequenceTracker {
        state: SequenceState::Ready(last),
    };
    tracker.begin(last)?;
    tracker.finish(&ready(last))?;
    assert_eq!(tracker.next_sequence(), None);
    assert_eq!(
        tracker.begin(CommandSequence::new(0)),
        Err(SequenceError::Exhausted)
    );
    assert_eq!(tracker.finish(&ready(last)), Err(SequenceError::Exhausted));
    Ok(())
}

#[test]
fn documented_exchange_has_the_expected_schema() -> Result<(), Box<dyn std::error::Error>> {
    let request = br#"{"sequence":1,"command":"printf 'ok\\n'"}"#;
    assert_eq!(decode_request(request, LIMIT)?.command, "printf 'ok\\n'");
    let response = br#"{"sequence":1,"outcome":{"kind":"completed","completion":{"kind":"exited","code":0,"source":"parent_observed"},"session_state":"ready","output":{"stdout":{"hex":"6f6b0a","truncated":false},"stderr":{"hex":"","truncated":false}}}}"#;
    let sequence = CommandSequence::new(1);
    let mut expected = ready(sequence);
    if let ExecutionOutcome::Completed { output, .. } = &mut expected.outcome {
        output.stdout.bytes = b"ok\n".to_vec();
    }
    assert_eq!(decode_report(response, LIMIT, sequence)?, expected);
    assert_eq!(
        serde_json::from_slice::<Value>(&encode_report(&expected, LIMIT)?)?,
        serde_json::from_slice::<Value>(response)?
    );
    Ok(())
}

#[test]
fn malformed_requests_are_rejected() {
    for input in [
        r#"[1,"id"]"#,
        r#"{"sequence":1,"command":"id","target":"host"}"#,
        r#"{"sequence":1,"sequence":1,"command":"id"}"#,
        r#"{"sequence":1,"command":"id","command":"pwd"}"#,
        r#"{"sequence":-1,"command":"id"}"#,
        r#"{"sequence":1.0,"command":"id"}"#,
        r#"{"sequence":18446744073709551616,"command":"id"}"#,
        r#"{"sequence":1,"command":null}"#,
        r#"{"sequence":1}"#,
        r#"{"sequence":1,"command":"id"} {}"#,
    ] {
        assert!(
            decode_request(input.as_bytes(), LIMIT).is_err(),
            "accepted {input}"
        );
    }
}

#[test]
fn malformed_and_contradictory_reports_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let sequence = CommandSequence::new(1);
    let base: Value = serde_json::from_slice(&encode_report(&ready(sequence), LIMIT)?)?;
    for (pointer, replacement) in [
        ("/outcome/completion/code", json!(256)),
        ("/outcome/completion/code", json!(-1)),
        ("/outcome/completion/code", json!(0.0)),
        (
            "/outcome/completion/source",
            json!({"parent_observed":null}),
        ),
        ("/outcome/session_state", json!({"ready":null})),
        ("/outcome/output", json!([])),
        ("/outcome/output/stdout", json!(["", false])),
        ("/outcome/output/stdout/hex", json!("A0")),
        ("/outcome/output/stdout/hex", json!("0")),
        ("/outcome/output/stdout/hex", json!("gg")),
        ("/outcome/output/stdout/hex", json!("é")),
        ("/outcome/output/stdout/truncated", Value::Null),
        (
            "/outcome/completion",
            json!({"kind":"signaled","signal":0,"source":"guest_reported"}),
        ),
        (
            "/outcome/completion",
            json!({"kind":"exited","code":0,"signal":1,"source":"guest_reported"}),
        ),
        (
            "/outcome",
            json!({"kind":"not_started","failure":{"kind":"session_unusable"},"output":{}}),
        ),
        (
            "/outcome",
            json!({"kind":"unknown","output":{"stdout":{"hex":"","truncated":false},"stderr":{"hex":"","truncated":false}},"error":{"kind":"transport","diagnostic":null},"session_state":"ready"}),
        ),
        (
            "/outcome",
            json!({"kind":"deadline_exceeded","output":{"stdout":{"hex":"","truncated":false},"stderr":{"hex":"","truncated":false}},"execution_state":{"kind":"may_still_be_running","session_state":"ready"}}),
        ),
        (
            "/outcome",
            json!({"kind":"not_started","failure":{"kind":"session_unusable","extra":true}}),
        ),
    ] {
        let mut value = base.clone();
        *value.pointer_mut(pointer).ok_or("invalid test pointer")? = replacement;
        let bytes = serde_json::to_vec(&value)?;
        assert!(
            decode_report(&bytes, LIMIT, sequence).is_err(),
            "accepted {value}"
        );
    }
    for input in [
        r#"[1,{"kind":"not_started","failure":{"kind":"session_unusable"}}]"#,
        r#"{"sequence":1,"sequence":1,"outcome":{"kind":"not_started","failure":{"kind":"session_unusable"}}}"#,
        r#"{"sequence":1,"outcome":{"kind":"not_started","kind":"not_started","failure":{"kind":"session_unusable"}}}"#,
        r#"{"sequence":1,"outcome":{"kind":"completed","completion":{"kind":"exited","code":0,"code":1,"source":"parent_observed"},"session_state":"ready","output":{"stdout":{"hex":"","truncated":false},"stderr":{"hex":"","truncated":false}}}}"#,
        r#"{"sequence":1,"outcome":{"kind":"not_started","failure":{"kind":"session_unusable"}}} {}"#,
    ] {
        assert!(
            decode_report(input.as_bytes(), LIMIT, sequence).is_err(),
            "accepted {input}"
        );
    }
    Ok(())
}
