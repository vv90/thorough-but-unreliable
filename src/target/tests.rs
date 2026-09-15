use proptest::prelude::*;

use super::*;

fn output() -> impl Strategy<Value = CommandOutput> {
    (
        prop::collection::vec(any::<u8>(), 0..64),
        prop::collection::vec(any::<u8>(), 0..64),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(stdout, stderr, stdout_truncated, stderr_truncated)| CommandOutput {
                stdout: CapturedOutput {
                    bytes: stdout,
                    truncated: stdout_truncated,
                },
                stderr: CapturedOutput {
                    bytes: stderr,
                    truncated: stderr_truncated,
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

proptest! {
    #[test]
    fn uncertain_execution_is_unusable_regardless_of_output_or_error(
        sequence in any::<u64>(), output in output(), error in error(),
    ) {
        for outcome in [
            ExecutionOutcome::Unknown { output: output.clone(), error },
            ExecutionOutcome::DeadlineExceeded {
                output,
                execution_state: ExecutionState::MayStillBeRunning,
            },
        ] {
            let report = ExecutionReport { sequence, outcome };
            prop_assert_eq!(report.session_state(), SessionState::Unusable);
        }
    }

    #[test]
    fn session_failures_cannot_be_mistaken_for_reusable_input_rejections(
        error in error(), diagnostic in prop::option::of(any::<String>()),
    ) {
        for failure in [StartFailure::Failed(error), StartFailure::SessionUnusable] {
            prop_assert_eq!(ExecutionOutcome::NotStarted(failure).session_state(), SessionState::Unusable);
        }
        for kind in [RejectionKind::InvalidCommand, RejectionKind::CommandTooLarge] {
            let rejected = StartFailure::Rejected(CommandRejection { kind, diagnostic: diagnostic.clone() });
            prop_assert_eq!(ExecutionOutcome::NotStarted(rejected).session_state(), SessionState::Ready);
        }
    }

    #[test]
    fn known_completion_never_overrides_the_adapters_readiness_decision(
        output in output(), code in any::<u8>(), signal in any::<NonZeroU32>(),
    ) {
        for session_state in [SessionState::Ready, SessionState::Unusable] {
            for source in [CompletionSource::ParentObserved, CompletionSource::GuestReported] {
                for completion in [ProcessCompletion::Exited { code, source }, ProcessCompletion::Signaled { signal, source }] {
                    let outcome = ExecutionOutcome::Completed { output: output.clone(), completion, session_state };
                    prop_assert_eq!(outcome.session_state(), session_state);
                }
            }
            let outcome = ExecutionOutcome::DeadlineExceeded {
                output: output.clone(),
                execution_state: ExecutionState::ConfirmedStopped { session_state },
            };
            prop_assert_eq!(outcome.session_state(), session_state);
        }
    }
}
