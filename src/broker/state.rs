//! Pure validation at the adapter/report boundary; never advance on a report
//! which cannot be correlated or serialized under the trusted response bound.
use crate::{
    command_protocol::{self, ProtocolError},
    target::{CommandSequence, ExecutionReport},
};

pub(super) fn prepare(
    report: &ExecutionReport,
    expected: CommandSequence,
    limit: usize,
) -> Result<Vec<u8>, ProtocolError> {
    if report.sequence != expected {
        return Err(ProtocolError::SequenceMismatch {
            expected,
            received: report.sequence,
        });
    }
    command_protocol::encode_report(report, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn report_admission_requires_correlation_and_exact_encoded_bound(
            sequence in any::<u64>(), expected in any::<u64>(),
            bytes in prop::collection::vec(any::<u8>(), 0..512), limit in 0usize..2048,
        ) {
            let report = ExecutionReport { sequence: CommandSequence::new(sequence), outcome: ExecutionOutcome::Unknown {
                output: CommandOutput { stdout: CapturedOutput { bytes, truncated: true }, stderr: CapturedOutput::default() },
                error: ExecutionError { kind: ExecutionErrorKind::Transport, diagnostic: None },
            }};
            let encoded = command_protocol::encode_report(&report, usize::MAX)?;
            let result = prepare(&report, CommandSequence::new(expected), limit);
            prop_assert_eq!(result.is_ok(), sequence == expected && encoded.len() <= limit);
            let matching = prepare(&report, report.sequence, encoded.len());
            prop_assert_eq!(matching.ok(), Some(encoded));
        }
    }
}
