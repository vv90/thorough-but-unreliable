//! Live inference mode of the paired VM runner. No command executes in the builder.
use super::*;

const COMMAND: &str = "id -u; id -g; pwd; printf 'paired-smoke\\n'";
const ANSWER: &str = "1000\n1000\n/work\npaired-smoke";
const PREFIX: &str = "HARNESS_REPORT:";

pub(super) fn forward(socket: &str) -> TestResult<String> {
    // This value crosses both QEMU's comma-separated options and its command
    // parser. Accept a deliberately narrow path alphabet, never shell syntax.
    if !socket.starts_with('/')
        || socket.len() > 107
        || !socket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
    {
        return Err("inference socket must be an absolute path of at most 107 ASCII letters, digits, / . _ -".into());
    }
    Ok(format!("nc -U {socket}"))
}

pub(super) fn manifest(model: &str) -> TestResult<String> {
    let mut manifest: Value = serde_json::from_str(PAIRED_MANIFEST)?;
    for (pointer, value) in [
        ("/run_id", json!("local-inference")),
        (
            "/system_prompt",
            json!(
                "Use the provided tools to perform the task. Commands run in a disposable target container. After observing the command result, submit the requested answer using the submit tool."
            ),
        ),
        (
            "/task",
            json!(format!(
                "Call execute_target_command exactly once with this exact command: {COMMAND}\nThen call submit exactly once with the command's stdout, removing only its final newline. Do not submit before receiving the command result."
            )),
        ),
        ("/inference/model", json!(model)),
        ("/inference/max_tokens", json!(4096)),
        ("/inference/request_timeout_ms", json!(300000)),
        ("/inference/max_request_bytes", json!(1048576)),
        ("/inference/max_response_bytes", json!(1048576)),
    ] {
        *manifest
            .pointer_mut(pointer)
            .ok_or("missing manifest field")? = value;
    }
    let encoded = serde_json::to_string_pretty(&manifest)?;
    thorough_but_unreliable::manifest::Manifest::from_slice(encoded.as_bytes())?;
    Ok(encoded)
}

fn extract_report(console: &[u8]) -> TestResult<Value> {
    let text = std::str::from_utf8(console)?;
    let mut reports = text.lines().filter_map(|line| line.strip_prefix(PREFIX));
    let report = reports
        .next()
        .ok_or("missing guest report; see harness/console.log")?;
    if reports.next().is_some() || report.len() > 1048576 {
        return Err("duplicate or oversized guest report".into());
    }
    Ok(serde_json::from_str(report)?)
}

fn verify_report(report: &Value) -> TestResult {
    if report.get("version") != Some(&json!(1))
        || report.get("run_id") != Some(&json!("local-inference"))
        || report.get("outcome") != Some(&json!({"kind":"submitted", "answer":ANSWER}))
    {
        return Err("trial did not submit the expected target output".into());
    }
    let history = report
        .get("history")
        .and_then(Value::as_array)
        .ok_or("missing history")?;
    let [system, user, assistant, result, submission] = history.as_slice() else {
        return Err("expected command, result, and submission in two model turns".into());
    };
    for (message, role) in [
        (system, "system"),
        (user, "user"),
        (assistant, "assistant"),
        (result, "tool"),
        (submission, "assistant"),
    ] {
        if message.get("role").and_then(Value::as_str) != Some(role) {
            return Err("unexpected conversation ordering".into());
        }
    }
    let single_call = |message: &Value| -> TestResult<Value> {
        let calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .ok_or("missing tool calls")?;
        let [call] = calls.as_slice() else {
            return Err("expected exactly one tool call per turn".into());
        };
        if call
            .get("id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err("missing tool call ID".into());
        }
        Ok(call.clone())
    };
    let command = single_call(assistant)?;
    let submit = single_call(submission)?;
    if command.get("tool") != Some(&json!({"kind":"execute_target_command","command":COMMAND}))
        || submit.get("tool") != Some(&json!({"kind":"submit","answer":ANSWER}))
    {
        return Err("unexpected tool calls".into());
    }
    let id = command
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("missing command ID")?;
    if result.get("call_id").and_then(Value::as_str) != Some(id) {
        return Err("command result correlation failed".into());
    }
    let expected: Value = serde_json::from_str(PAIRED_REPORT)?;
    if result.get("result") != expected.pointer("/history/3/result") {
        return Err("target output, completion provenance, or session state differed".into());
    }
    Ok(())
}

pub(super) async fn drive_live(mut child: Child, directory: &Path) -> TestResult {
    let mut console = Vec::new();
    let mut stderr = Vec::new();
    let result = timeout(Duration::from_secs(900), async {
        let out = child.stdout.take().ok_or("missing harness stdout")?;
        let err = child.stderr.take().ok_or("missing harness stderr")?;
        let (status, (), ()) = tokio::try_join!(
            async { Ok::<_, TestError>(child.wait().await?) },
            capture(out, &mut console, 4194304, None),
            capture(err, &mut stderr, 65536, None),
        )?;
        if !status.success() {
            return Err("harness QEMU failed".into());
        }
        Ok::<_, TestError>(())
    })
    .await;
    let cleanup = async {
        if child.try_wait()?.is_none() {
            child.kill().await?;
        }
        child.wait().await?;
        Ok::<_, TestError>(())
    }
    .await;
    std::fs::write(directory.join("console.log"), &console)?;
    std::fs::write(directory.join("stderr.log"), &stderr)?;
    cleanup?;
    result??;
    let report = extract_report(&console)?;
    std::fs::write(
        directory.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    if !String::from_utf8_lossy(&console)
        .lines()
        .any(|line| line == "harness local run: COMPLETE")
    {
        return Err("guest run or report permissions failed".into());
    }
    verify_report(&report)
}

#[tokio::test]
#[ignore = "requires KVM and the host inference socket; use scripts/run-harness-local.sh"]
async fn local_inference() -> TestResult {
    let socket = std::env::var("INFERENCE_SOCKET")?;
    forward(&socket)?;
    let model = std::env::var("INFERENCE_MODEL")?;
    run(Inference::Local { socket, model }).await?;
    println!("local inference VM run: PASS (real inference, target command, submission, cleanup)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn valid_report() -> TestResult<Value> {
        let mut report: Value = serde_json::from_str(PAIRED_REPORT)?;
        for (pointer, value) in [
            ("/run_id", json!("local-inference")),
            ("/outcome/answer", json!(ANSWER)),
            ("/history/2/tool_calls/0/tool/command", json!(COMMAND)),
            ("/history/4/tool_calls/0/tool/answer", json!(ANSWER)),
        ] {
            *report.pointer_mut(pointer).ok_or("missing fixture field")? = value;
        }
        Ok(report)
    }

    proptest! {
        #[test]
        fn correlation_is_independent_of_model_call_id(id in ".{1,80}") {
            let mut report = valid_report().map_err(|e| TestCaseError::fail(e.to_string()))?;
            *report.pointer_mut("/history/2/tool_calls/0/id").ok_or_else(|| TestCaseError::fail("missing ID"))? = json!(id);
            *report.pointer_mut("/history/3/call_id").ok_or_else(|| TestCaseError::fail("missing ID"))? = json!(id);
            prop_assert!(verify_report(&report).is_ok());
            *report.pointer_mut("/history/3/call_id").ok_or_else(|| TestCaseError::fail("missing ID"))? = json!(format!("{id}different"));
            prop_assert!(verify_report(&report).is_err());
        }

        #[test]
        fn altered_stdout_cannot_pass(stdout in ".{0,100}") {
            prop_assume!(stdout != format!("{ANSWER}\n"));
            let mut report = valid_report().map_err(|e| TestCaseError::fail(e.to_string()))?;
            *report.pointer_mut("/history/3/result/report/outcome/output/stdout/data").ok_or_else(|| TestCaseError::fail("missing stdout"))? = json!(stdout);
            prop_assert!(verify_report(&report).is_err());
        }

        #[test]
        fn socket_paths_preserve_arguments_and_reject_parser_metacharacters(
            path in "/[a-zA-Z0-9_./-]{1,100}",
            metachar in prop::sample::select(vec![' ', ',', ';', '\n', '\'', '"', '$', '`', '\\']),
        ) {
            let command = forward(&path).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(command, format!("nc -U {path}"));
            let injected = format!("{path}{metachar}x");
            prop_assert!(forward(&injected).is_err());
        }
    }

    #[test]
    fn report_transport_rejects_duplicates_and_preserves_json() -> TestResult {
        let report = valid_report()?;
        let line = format!("{PREFIX}{}\r\n", serde_json::to_string(&report)?);
        assert_eq!(
            extract_report(format!("boot log\r\n{line}shutdown\r\n").as_bytes())?,
            report
        );
        assert!(extract_report(format!("{line}{line}").as_bytes()).is_err());
        verify_report(&report)
    }

    #[test]
    fn manifest_uses_production_validation_and_rejects_empty_model() -> TestResult {
        manifest("qwen3.5:9b-q4_K_M")?;
        assert!(manifest("").is_err());
        Ok(())
    }

    #[test]
    fn submission_before_command_result_is_rejected() -> TestResult {
        let mut report = valid_report()?;
        let history = report
            .get_mut("history")
            .and_then(Value::as_array_mut)
            .ok_or("missing history")?;
        if let [_, _, _, result, submission] = history.as_mut_slice() {
            std::mem::swap(result, submission);
        } else {
            return Err("unexpected fixture history".into());
        }
        assert!(verify_report(&report).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_retains_reports_on_success_and_semantic_failure() -> TestResult {
        for succeeds in [true, false] {
            let directory = artifact_directory().await?;
            let mut report = valid_report()?;
            if !succeeds {
                *report
                    .pointer_mut("/outcome/answer")
                    .ok_or("missing answer")? = json!("wrong");
            }
            let transcript = directory.join("input.txt");
            std::fs::write(
                &transcript,
                format!(
                    "{PREFIX}{}\nharness local run: COMPLETE\n",
                    serde_json::to_string(&report)?
                ),
            )?;
            let mut command = Command::new("cat");
            command.arg(&transcript);
            let result = drive_live(spawn(command)?, &directory).await;
            assert_eq!(result.is_ok(), succeeds);
            assert_eq!(
                serde_json::from_slice::<Value>(&std::fs::read(directory.join("report.json"))?)?,
                report
            );
            assert!(!std::fs::read(directory.join("console.log"))?.is_empty());
        }
        Ok(())
    }
}
