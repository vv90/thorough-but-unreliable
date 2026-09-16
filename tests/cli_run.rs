//! Execute the installed CLI contract against scripted endpoints, with real
//! temporary manifest files, bounded waits/output, and child cleanup on failure.

mod support;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use support::*;
use tokio::{io::AsyncReadExt, net::TcpListener, process::Command, sync::oneshot, time::timeout};

struct Directory(PathBuf);
impl Directory {
    fn new() -> TestResult<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        for _ in 0..32 {
            let id = NEXT
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| "temporary name space exhausted")?;
            let path =
                std::env::temp_dir().join(format!("harness-cli-{}-{id}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not create a unique test directory".into())
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("manifest.json"));
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn initial() -> Vec<Value> {
    vec![
        json!({"role":"system","content":"system"}),
        json!({"role":"user","content":"task"}),
    ]
}
fn call(name: &str, arguments: Value) -> Value {
    json!({"id":"call","type":"function","function":{"name":name,"arguments":arguments.to_string()}})
}
fn assistant(calls: Vec<Value>) -> Value {
    json!({"role":"assistant","content":null,"tool_calls":calls})
}
fn model(messages: Vec<Value>, response: Value) -> Exchange {
    Exchange::Inference {
        messages,
        reply: Reply::Json(
            json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":response}]}),
        ),
    }
}
fn command_request() -> Value {
    json!({"sequence":1,"command":"printf 'λ\\n'"})
}
fn command_call() -> Value {
    call("execute_target_command", json!({"command":"printf 'λ\\n'"}))
}
fn broker_report() -> Value {
    json!({"sequence":1,"outcome":{"kind":"completed","output":{"stdout":{"hex":"00ff0a","truncated":true},"stderr":{"hex":"","truncated":false}},"completion":{"kind":"exited","code":7,"source":"guest_reported"},"session_state":"ready"}})
}
fn target_view() -> Value {
    json!({"sequence":1,"session_state":"ready","outcome":{"kind":"completed","output":{"stdout":{"encoding":"hex","data":"00ff0a","truncated":true},"stderr":{"encoding":"utf8","data":"","truncated":false}},"completion":{"kind":"exited","code":7,"source":"guest_reported"}}})
}

async fn scenario(
    script: Vec<Exchange>,
    command: &str,
    change: impl FnOnce(&mut Value) -> TestResult,
) -> TestResult<Output> {
    let inference = TcpListener::bind("127.0.0.1:0").await?;
    let broker = TcpListener::bind("127.0.0.1:0").await?;
    let mut manifest = json!({"version":1,"run_id":"cli-λ","system_prompt":"system","task":"task","max_model_turns":3,
        "inference":{"completion_url":format!("http://{}/v1/chat/completions",inference.local_addr()?),"model":"fixture-model","max_tokens":256,"connect_timeout_ms":1000,"request_timeout_ms":2000,"max_request_bytes":LIMIT,"max_response_bytes":LIMIT},
        "command":{"command_url":format!("http://{}/v1/command",broker.local_addr()?),"connect_timeout_ms":1000,"request_timeout_ms":2000,"max_request_bytes":LIMIT,"max_response_bytes":LIMIT}});
    change(&mut manifest)?;
    let directory = Directory::new()?;
    let path = directory.0.join("manifest.json");
    std::fs::write(&path, serde_json::to_vec(&manifest)?)?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args([command, "--manifest"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let result = timeout(Duration::from_secs(10), async {
        let stdout = child.stdout.take().ok_or("missing stdout")?;
        let stderr = child.stderr.take().ok_or("missing stderr")?;
        let read = async |stream: tokio::process::ChildStdout| {
            let mut bytes = Vec::new();
            stream.take(1048577).read_to_end(&mut bytes).await?;
            if bytes.len() > 1048576 {
                return Err("stdout exceeded test bound".into());
            }
            Ok::<_, TestError>(bytes)
        };
        let read_stderr = async {
            let mut bytes = Vec::new();
            stderr.take(65537).read_to_end(&mut bytes).await?;
            if bytes.len() > 65536 {
                return Err("stderr exceeded test bound".into());
            }
            Ok::<_, TestError>(bytes)
        };
        let (done, received) = oneshot::channel();
        let wait = async {
            let status = child.wait().await?;
            done.send(()).map_err(|_| "fake endpoints stopped early")?;
            Ok::<_, TestError>(status)
        };
        let (status, stdout, stderr, ()) = tokio::try_join!(
            wait,
            read(stdout),
            read_stderr,
            serve(inference, broker, script.into(), received)
        )?;
        Ok::<_, TestError>(Output {
            status,
            stdout,
            stderr,
        })
    })
    .await;
    // Reap explicitly after timeout/server failure; kill_on_drop is the fallback
    // if the test itself unwinds. Never leave a hung child running between tests.
    if child.try_wait()?.is_none() {
        child.kill().await?;
    }
    result?
}

fn set(value: &mut Value, path: &str, replacement: Value) -> TestResult {
    *value.pointer_mut(path).ok_or("missing manifest field")? = replacement;
    Ok(())
}
fn report(output: &Output, success: bool) -> TestResult<Value> {
    assert_eq!(
        output.status.code(),
        Some(if success { 0 } else { 1 }),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.ends_with(b"\n"));
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[tokio::test]
async fn executable_loads_manifest_executes_command_and_exports_complete_report() -> TestResult {
    let batch = assistant(vec![command_call()]);
    let mut messages = initial();
    messages.push(batch.clone());
    messages.push(json!({"role":"tool","tool_call_id":"call","content":target_view().to_string()}));
    let output = scenario(
        vec![
            model(initial(), batch),
            Exchange::Command {
                request: command_request(),
                reply: Reply::Json(broker_report()),
            },
            model(
                messages,
                assistant(vec![call("submit", json!({"answer":"done\nλ"}))]),
            ),
        ],
        "run",
        |_| Ok(()),
    )
    .await?;
    assert_eq!(
        report(&output, true)?,
        json!({"version":1,"run_id":"cli-λ","outcome":{"kind":"submitted","answer":"done\nλ"},"history":[
            {"role":"system","content":"system"},{"role":"user","content":"task"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call","tool":{"kind":"execute_target_command","command":"printf 'λ\\n'"}}]},
            {"role":"tool","call_id":"call","result":{"kind":"execution_report","report":target_view()}},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call","tool":{"kind":"submit","answer":"done\nλ"}}]}
        ]})
    );
    Ok(())
}

#[tokio::test]
async fn non_submission_outcomes_export_reports_and_exit_nonzero() -> TestResult {
    let early = scenario(vec![Exchange::Inference { messages: initial(), reply: Reply::Json(json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"stopped","tool_calls":[]}}]})) }], "run", |_| Ok(())).await?;
    assert_eq!(
        report(&early, false)?.pointer("/outcome/kind"),
        Some(&json!("early_termination"))
    );
    let limit = scenario(
        vec![
            model(initial(), assistant(vec![command_call()])),
            Exchange::Command {
                request: command_request(),
                reply: Reply::Json(broker_report()),
            },
        ],
        "run",
        |m| set(m, "/max_model_turns", json!(1)),
    )
    .await?;
    let value = report(&limit, false)?;
    assert_eq!(
        value.pointer("/outcome/kind"),
        Some(&json!("model_turn_limit"))
    );
    assert_eq!(
        value.pointer("/history/3/result/report"),
        Some(&target_view())
    );
    let mut submission = call("submit", json!({"answer":"done"}));
    set(&mut submission, "/id", json!("submission"))?;
    let invalid = scenario(
        vec![model(
            initial(),
            assistant(vec![submission, command_call()]),
        )],
        "run",
        |_| Ok(()),
    )
    .await?;
    assert_eq!(
        report(&invalid, false)?.pointer("/outcome/kind"),
        Some(&json!("protocol_error"))
    );
    Ok(())
}

#[tokio::test]
async fn transport_failures_preserve_history_and_stop_dispatch() -> TestResult {
    let model_failure = scenario(
        vec![Exchange::Inference {
            messages: initial(),
            reply: Reply::Unavailable,
        }],
        "run",
        |_| Ok(()),
    )
    .await?;
    let value = report(&model_failure, false)?;
    assert_eq!(value.get("history"), Some(&json!(initial())));
    assert_eq!(
        value.pointer("/outcome/error/category"),
        Some(&json!({"kind":"http_status","status":503}))
    );
    let command_failure = scenario(
        vec![
            model(initial(), assistant(vec![command_call()])),
            Exchange::Command {
                request: command_request(),
                reply: Reply::Disconnect,
            },
        ],
        "run",
        |_| Ok(()),
    )
    .await?;
    let value = report(&command_failure, false)?;
    assert_eq!(
        value.pointer("/outcome/kind"),
        Some(&json!("command_client_failure"))
    );
    assert_eq!(
        value.pointer("/outcome/error/category/kind"),
        Some(&json!("transport"))
    );
    assert_eq!(
        value.pointer("/outcome/error/completion_unknown"),
        Some(&json!(true))
    );
    assert_eq!(
        value.pointer("/outcome/error"),
        value.pointer("/history/3/result/error")
    );
    Ok(())
}

#[tokio::test]
async fn unusable_target_keeps_partial_output_and_returns_failure() -> TestResult {
    let wire = json!({"sequence":1,"outcome":{"kind":"unknown","output":{"stdout":{"hex":"ff00","truncated":true},"stderr":{"hex":"","truncated":false}},"error":{"kind":"transport","diagnostic":"lost guest"}}});
    let output = scenario(
        vec![
            model(initial(), assistant(vec![command_call()])),
            Exchange::Command {
                request: command_request(),
                reply: Reply::Json(wire),
            },
        ],
        "run",
        |_| Ok(()),
    )
    .await?;
    let value = report(&output, false)?;
    assert_eq!(
        value.get("outcome"),
        Some(&json!({"kind":"target_session_unusable","call_id":"call"}))
    );
    assert_eq!(
        value.pointer("/history/3/result/report/outcome/output/stdout"),
        Some(&json!({"encoding":"hex","data":"ff00","truncated":true}))
    );
    Ok(())
}

#[tokio::test]
async fn configuration_and_usage_failures_do_not_contact_endpoints() -> TestResult {
    for command in ["run", "check-config"] {
        let output = scenario(vec![], command, |m| set(m, "/task", json!(""))).await?;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8(output.stderr)?.contains("task"));
    }
    let output = scenario(vec![], "invalid-command", |_| Ok(())).await?;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("usage:"));
    let output = scenario(vec![], "check-config", |_| Ok(())).await?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout)?,
        "harness config: manifest validated run_id=\"cli-λ\"\n"
    );
    assert!(output.stderr.is_empty());
    Ok(())
}

#[tokio::test]
async fn manifest_request_limits_are_applied_before_network_dispatch() -> TestResult {
    let output = scenario(vec![], "run", |m| {
        set(m, "/inference/max_request_bytes", json!(1))
    })
    .await?;
    assert_eq!(
        report(&output, false)?.pointer("/outcome/error/category"),
        Some(&json!({"kind":"request_too_large","limit":1}))
    );
    let output = scenario(
        vec![model(initial(), assistant(vec![command_call()]))],
        "run",
        |m| set(m, "/command/max_request_bytes", json!(1)),
    )
    .await?;
    assert_eq!(
        report(&output, false)?.pointer("/outcome/error/category"),
        Some(&json!({"kind":"request_too_large","limit":1}))
    );
    Ok(())
}
