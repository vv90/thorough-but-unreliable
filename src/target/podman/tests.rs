use super::*;
use crate::target::*;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError as TestError;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    num::{NonZeroU32, NonZeroUsize},
};

type TestResult<T = ()> = Result<T, proptest::test_runner::TestCaseError>;
fn settings() -> TestResult<Settings> {
    Ok(Settings {
        socket_path: "/run/experiment/podman.sock".into(),
        container_id: "a".repeat(64),
        uid: 1234,
        gid: 2345,
        shell: "/bin/sh".into(),
        workdir: "/work".into(),
        environment: BTreeMap::from([("PATH".into(), "/bin".into()), ("EMPTY".into(), "".into())]),
        limits: Limits::new(
            NonZeroUsize::new(1024).ok_or_else(|| TestError::fail("zero"))?,
            NonZeroUsize::new(4096).ok_or_else(|| TestError::fail("zero"))?,
            NonZeroUsize::new(64).ok_or_else(|| TestError::fail("zero"))?,
            NonZeroUsize::new(64).ok_or_else(|| TestError::fail("zero"))?,
            NonZeroU32::new(1000).ok_or_else(|| TestError::fail("zero"))?,
        )?,
    })
}
fn session() -> TestResult<Session> {
    Ok(Session::new(Config::new(settings()?)?))
}
fn request(sequence: u64, command: String) -> CommandRequest {
    CommandRequest {
        sequence: CommandSequence::new(sequence),
        command,
    }
}
fn accepted<T>(value: Result<T, ExecutionReport>) -> Result<T, io::Error> {
    value.map_err(|r| io::Error::other(format!("unexpected report: {r:?}")))
}
fn start(session: &mut Session, sequence: u64) -> TestResult<Start<'_>> {
    Ok(accepted(
        accepted(session.begin(request(sequence, "id".into())))?
            .created(201, &serde_json::to_vec(&json!({"Id":"b".repeat(64)}))?),
    )?)
}
fn capture(session: &mut Session, sequence: u64) -> TestResult<Capture<'_>> {
    Ok(accepted(start(session, sequence)?.attached(101))?)
}
fn inspect(session: &mut Session, sequence: u64) -> TestResult<Inspect<'_>> {
    Ok(accepted(capture(session, sequence)?.eof())?)
}
fn observation(code: i64, running: bool, removable: bool) -> Value {
    json!({"ID":"b".repeat(64),"ContainerID":"a".repeat(64),"Running":running,"CanRemove":removable,"ExitCode":code})
}
fn frame(channel: u8, bytes: &[u8]) -> TestResult<Vec<u8>> {
    let mut output = vec![channel, 0, 0, 0];
    output.extend(u32::try_from(bytes.len())?.to_be_bytes());
    output.extend(bytes);
    Ok(output)
}
fn finished(value: Inspection<'_>) -> TestResult<ExecutionReport> {
    match value {
        Inspection::Finished(report) => Ok(report),
        Inspection::Pending(_) => Err(TestError::fail("unexpected pending inspection")),
    }
}

proptest! {
    #[test]
    fn create_preserves_command_as_one_argument_and_never_changes_binding(command in any::<String>(), sequence in any::<u64>()) {
        let mut session = session()?;
        let valid = !command.contains('\0') && command.len() <= 1024;
        let attempt = session.begin(request(sequence, command.clone()));
        if !valid {
            let report = attempt.err().ok_or_else(|| TestError::fail("invalid command accepted"))?;
            prop_assert_eq!(report.session_state(), SessionState::Ready);
        } else {
            let attempt = accepted(attempt)?;
            prop_assert_eq!(attempt.path(), format!("/containers/{}/exec", "a".repeat(64)));
            let value: Value = serde_json::from_slice(attempt.body())?;
            prop_assert_eq!(value, json!({"Cmd":["/bin/sh","-c",command],"User":"1234:2345","WorkingDir":"/work",
                "Env":["EMPTY=","PATH=/bin"],"AttachStdin":false,"AttachStdout":true,"AttachStderr":true,"Tty":false,"Privileged":false}));
        }
    }

    #[test]
    fn multiplexing_is_chunk_invariant_and_keeps_only_each_stream_prefix(
        frames in prop::collection::vec((0u8..3, prop::collection::vec(any::<u8>(), 0..96)), 0..12),
        stdout_limit in 0usize..100, stderr_limit in 0usize..100, chunk in 1usize..80,
    ) {
        let mut wire = Vec::new();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for (channel, bytes) in frames {
            wire.extend(frame(channel, &bytes)?);
            if channel == 2 { stderr.extend(bytes); } else { stdout.extend(bytes); }
        }
        let mut decoder = stream::Decoder::new(stdout_limit, stderr_limit);
        for bytes in wire.chunks(chunk) { decoder.feed(bytes)?; }
        prop_assert!(decoder.at_boundary());
        let output = decoder.into_output();
        prop_assert_eq!(output.stdout.truncated, stdout.len() > stdout_limit);
        prop_assert_eq!(output.stderr.truncated, stderr.len() > stderr_limit);
        stdout.truncate(stdout_limit);
        stderr.truncate(stderr_limit);
        prop_assert_eq!(output.stdout.bytes, stdout);
        prop_assert_eq!(output.stderr.bytes, stderr);
    }

    #[test]
    fn arbitrary_stream_bytes_never_exceed_capture_bounds(bytes in prop::collection::vec(any::<u8>(), 0..1024), chunk in 1usize..40) {
        let mut decoder = stream::Decoder::new(7, 11);
        for bytes in bytes.chunks(chunk) { if decoder.feed(bytes).is_err() { break; } }
        let output = decoder.into_output();
        prop_assert!(output.stdout.bytes.len() <= 7);
        prop_assert!(output.stderr.bytes.len() <= 11);
    }

    #[test]
    fn abandoning_any_stage_permanently_prevents_reuse(stage in 0u8..5, sequence in any::<u64>(), later in any::<String>()) {
        let mut session = session()?;
        match stage {
            0 => { drop(accepted(session.begin(request(sequence, "id".into())))?); }
            1 => { drop(start(&mut session, sequence)?); }
            2 => { drop(capture(&mut session, sequence)?); }
            3 => { drop(inspect(&mut session, sequence)?); }
            _ => { drop(accepted(inspect(&mut session, sequence)?.observed(200, &serde_json::to_vec(&observation(0, true, false))?))?); }
        }
        prop_assert_eq!(session.state(), SessionState::Unusable);
        let report = session.begin(request(sequence, later)).err().ok_or_else(|| TestError::fail("reused abandoned session"))?;
        prop_assert_eq!(report.outcome, ExecutionOutcome::NotStarted(StartFailure::SessionUnusable));
        prop_assert_eq!(session.state(), SessionState::Unusable);
    }

    #[test]
    fn only_correlated_stopped_inspection_after_eof_restores_readiness(
        exec_matches in any::<bool>(), container_matches in any::<bool>(), running in any::<bool>(), removable in any::<bool>(),
        code in -2i64..258, sequence in any::<u64>(),
    ) {
        let mut session = session()?;
        let mut value = observation(code, running, removable);
        if !exec_matches { *value.get_mut("ID").ok_or_else(|| TestError::fail("missing ID"))? = json!("c".repeat(64)); }
        if !container_matches { *value.get_mut("ContainerID").ok_or_else(|| TestError::fail("missing container"))? = json!("d".repeat(64)); }
        let result = inspect(&mut session, sequence)?.observed(200, &serde_json::to_vec(&value)?);
        let should_finish = exec_matches && container_matches && !running && removable && (0..=255).contains(&code);
        match result {
            Ok(Inspection::Finished(report)) => {
                prop_assert!(should_finish);
                prop_assert_eq!(report.sequence, CommandSequence::new(sequence));
                prop_assert_eq!(report.outcome, ExecutionOutcome::Completed { output: CommandOutput::default(),
                    completion: ProcessCompletion::RuntimeStatus { code: u8::try_from(code)?, source: CompletionSource::ParentObserved }, session_state: SessionState::Ready });
            }
            Ok(Inspection::Pending(_)) => { prop_assert!(!should_finish); }
            Err(report) => { prop_assert!(!should_finish); prop_assert_eq!(report.session_state(), SessionState::Unusable); }
        }
        prop_assert_eq!(session.state() == SessionState::Ready, should_finish);
    }

    #[test]
    fn transport_deadline_and_unwind_failures_never_restore_readiness(stage in 0u8..4, failure in 0u8..3, sequence in any::<u64>()) {
        let mut session = session()?;
        let cause = match failure { 0 => Failure::Transport, 1 => Failure::Deadline, _ => Failure::DependencyPanicked };
        let report = match stage {
            0 => accepted(session.begin(request(sequence, "id".into())))?.fail(cause),
            1 => start(&mut session, sequence)?.fail(cause),
            2 => capture(&mut session, sequence)?.fail(cause),
            _ => inspect(&mut session, sequence)?.fail(cause),
        };
        prop_assert_eq!(report.sequence, CommandSequence::new(sequence));
        prop_assert_eq!(report.session_state(), SessionState::Unusable);
        prop_assert_eq!(session.state(), SessionState::Unusable);
        prop_assert_eq!(matches!(report.outcome, ExecutionOutcome::NotStarted(_)), stage == 0);
        prop_assert_eq!(matches!(report.outcome, ExecutionOutcome::DeadlineExceeded { .. }), stage != 0 && failure == 1);
    }

    #[test]
    fn runtime_codes_preserve_ambiguity_through_protocol_and_presentation(code in any::<u8>()) {
        let mut session = session()?;
        let report = finished(accepted(inspect(&mut session, 1)?.observed(200, &serde_json::to_vec(&observation(i64::from(code), false, true))?))?)?;
        let bytes = crate::command_protocol::encode_report(&report, 4096)?;
        prop_assert_eq!(crate::command_protocol::decode_report(&bytes, 4096, report.sequence)?, report.clone());
        let view = crate::harness::presentation::command_report_view(&report);
        prop_assert_eq!(view.pointer("/outcome/completion"), Some(&json!({"kind":"runtime_status","code":code,"source":"parent_observed"})));
    }

    #[test]
    fn create_json_limit_is_exact_even_when_command_requires_escaping(command in any::<String>()) {
        let mut config = Config::new(settings()?)?;
        let bytes = wire::create(&config, &command)?;
        config.limits.json = bytes.len();
        prop_assert_eq!(wire::create(&config, &command)?, bytes.clone());
        config.limits.json = bytes.len().checked_sub(1).ok_or_else(|| TestError::fail("empty encoding"))?;
        prop_assert!(matches!(wire::create(&config, &command), Err(Error::BodyTooLarge { .. })), "oversized JSON accepted");
    }

    #[test]
    fn full_container_ids_are_the_only_accepted_selectors(id in prop_oneof!["[0-9a-f]{64}", any::<String>()]) {
        let mut settings = settings()?;
        settings.container_id = id.clone();
        let valid = id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        prop_assert_eq!(Config::new(settings).is_ok(), valid);
    }
}

#[test]
fn validation_rejects_unsafe_bindings_and_preserves_explicit_environment() -> TestResult {
    for (field, value) in [
        (0, "relative"),
        (0, "/bad\0sock"),
        (1, "latest"),
        (1, "../other"),
        (2, "sh"),
        (3, "relative"),
    ] {
        let mut input = settings()?;
        match field {
            0 => input.socket_path = value.into(),
            1 => input.container_id = value.into(),
            2 => input.shell = value.into(),
            _ => input.workdir = value.into(),
        }
        assert!(Config::new(input).is_err());
    }
    let mut input = settings()?;
    input.socket_path = format!("/{}", "a".repeat(107));
    assert!(Config::new(input).is_err());
    for (key, value) in [("", "v"), ("1bad", "v"), ("BAD=KEY", "v"), ("BAD", "\0")] {
        let mut input = settings()?;
        input.environment.insert(key.into(), value.into());
        assert!(Config::new(input).is_err());
    }
    let mut input = settings()?;
    input
        .environment
        .insert("SPECIAL".into(), "$(id)\n=λ".into());
    let config = Config::new(input)?;
    let body: Value = serde_json::from_slice(&wire::create(&config, "id")?)?;
    assert_eq!(
        body.get("Env"),
        Some(&json!(["EMPTY=", "PATH=/bin", "SPECIAL=$(id)\n=λ"]))
    );
    Ok(())
}

#[test]
fn rejections_preserve_readiness_but_never_revive_a_failed_session() -> TestResult {
    let mut session = session()?;
    for command in ["\0".into(), "x".repeat(1025), "\u{1}".repeat(1024)] {
        let report = session
            .begin(request(1, command))
            .err()
            .ok_or_else(|| TestError::fail("accepted invalid or oversized command"))?;
        assert_eq!(report.session_state(), SessionState::Ready);
        assert_eq!(session.state(), SessionState::Ready);
    }
    drop(accepted(session.begin(request(1, "id".into())))?);
    assert_eq!(session.state(), SessionState::Unusable);
    assert_eq!(
        session
            .begin(request(2, "\0".into()))
            .err()
            .ok_or_else(|| TestError::fail("accepted"))?
            .outcome,
        ExecutionOutcome::NotStarted(StartFailure::SessionUnusable)
    );
    Ok(())
}

#[test]
fn partial_frames_and_runtime_errors_preserve_captured_output_and_end_session() -> TestResult {
    for tail in [
        vec![1, 0],
        vec![1, 0, 0, 0, 0, 0, 0, 2, 42],
        vec![3, 0, 0, 0, 0, 0, 0, 0],
        vec![4, 0, 0, 0, 0, 0, 0, 0],
        vec![1, 1, 0, 0, 0, 0, 0, 0],
    ] {
        let mut session = session()?;
        let reading = accepted(capture(&mut session, 1)?.feed(&frame(2, b"retained")?))?;
        let report = match reading.feed(&tail) {
            Err(report) => report,
            Ok(reading) => reading
                .eof()
                .err()
                .ok_or_else(|| TestError::fail("accepted partial EOF"))?,
        };
        match report.outcome {
            ExecutionOutcome::Unknown { output, .. } => {
                assert_eq!(output.stderr.bytes, b"retained")
            }
            _ => return Err(TestError::fail("expected uncertain completion")),
        }
        assert_eq!(session.state(), SessionState::Unusable);
    }
    Ok(())
}

#[test]
fn huge_announced_frames_do_not_allocate_the_announced_length() -> TestResult {
    let mut decoder = stream::Decoder::new(3, 3);
    decoder.feed(&[1, 0, 0, 0, 255, 255, 255, 255])?;
    decoder.feed(b"abcdef")?;
    assert!(!decoder.at_boundary());
    assert_eq!(
        decoder.into_output().stdout,
        CapturedOutput {
            bytes: b"abc".to_vec(),
            truncated: true
        }
    );
    Ok(())
}

#[test]
fn malformed_create_and_inspect_responses_cannot_bypass_validation() -> TestResult {
    for bytes in [
        b"[]".as_slice(),
        b"{}",
        b"{\"Id\":\"short\"}",
        b"{\"Id\":null}",
        b"{\"Id\":\"a\",\"ID\":\"b\"}",
    ] {
        let mut session = session()?;
        let report = accepted(session.begin(request(1, "id".into())))?
            .created(201, bytes)
            .err()
            .ok_or_else(|| TestError::fail("accepted create"))?;
        assert!(matches!(
            report.outcome,
            ExecutionOutcome::NotStarted(StartFailure::Failed(_))
        ));
        assert_eq!(session.state(), SessionState::Unusable);
    }
    let valid = observation(0, false, true);
    let fields = valid.as_object().ok_or_else(|| TestError::fail("object"))?;
    let mut variants = vec![json!([]), json!(null)];
    for key in fields.keys() {
        let mut missing = fields.clone();
        missing.remove(key);
        variants.push(Value::Object(missing));
        let mut wrong = fields.clone();
        wrong.insert(key.clone(), Value::Null);
        variants.push(Value::Object(wrong));
    }
    for value in variants {
        let mut session = session()?;
        let report = inspect(&mut session, 1)?
            .observed(200, &serde_json::to_vec(&value)?)
            .err()
            .ok_or_else(|| TestError::fail("accepted inspect"))?;
        assert!(matches!(report.outcome, ExecutionOutcome::Unknown { .. }));
        assert_eq!(session.state(), SessionState::Unusable);
    }
    let valid = serde_json::to_string(&valid)?;
    for body in [
        format!("{valid} {{}}"),
        valid.replacen(
            "\"Running\":false",
            "\"Running\":false,\"Running\":false",
            1,
        ),
    ] {
        let mut session = session()?;
        assert!(
            inspect(&mut session, 1)?
                .observed(200, body.as_bytes())
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn pending_inspection_can_finish_without_restarting_and_next_command_can_begin() -> TestResult {
    let mut session = session()?;
    let reading = accepted(capture(&mut session, 7)?.feed(&frame(1, &[0, 255, 42])?))?;
    let observing = accepted(reading.eof())?;
    let observing = match accepted(
        observing.observed(200, &serde_json::to_vec(&observation(0, true, false))?),
    )? {
        Inspection::Pending(next) => next,
        Inspection::Finished(_) => return Err(TestError::fail("premature completion")),
    };
    let report = finished(accepted(
        observing.observed(200, &serde_json::to_vec(&observation(137, false, true))?),
    )?)?;
    assert!(matches!(
        report.outcome,
        ExecutionOutcome::Completed {
            completion: ProcessCompletion::RuntimeStatus { code: 137, .. },
            ..
        }
    ));
    assert_eq!(session.state(), SessionState::Ready);
    assert!(session.begin(request(8, "next".into())).is_ok());
    Ok(())
}
