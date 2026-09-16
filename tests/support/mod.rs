use serde_json::{Value, json};
use std::{collections::VecDeque, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};

pub type TestError = Box<dyn std::error::Error + Send + Sync>;
pub type TestResult<T = ()> = Result<T, TestError>;
pub const LIMIT: usize = 65536;

pub enum Reply {
    Json(Value),
    Disconnect,
    Unavailable,
}

pub enum Exchange {
    Inference { messages: Vec<Value>, reply: Reply },
    Command { request: Value, reply: Reply },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endpoint {
    Inference,
    Command,
}

impl Exchange {
    fn endpoint(&self) -> Endpoint {
        match self {
            Self::Inference { .. } => Endpoint::Inference,
            Self::Command { .. } => Endpoint::Command,
        }
    }
    fn check(self, body: &Value) -> TestResult<Reply> {
        match self {
            Self::Inference { messages, reply } => {
                assert_eq!(body.get("messages"), Some(&json!(messages)));
                assert_eq!(body.get("model"), Some(&json!("fixture-model")));
                assert_eq!(body.get("stream"), Some(&json!(false)));
                assert_eq!(body.get("n"), Some(&json!(1)));
                assert_eq!(body.get("max_tokens"), Some(&json!(256)));
                let names: Vec<_> = body
                    .get("tools")
                    .and_then(Value::as_array)
                    .ok_or("missing tools")?
                    .iter()
                    .map(|tool| tool.pointer("/function/name"))
                    .collect();
                assert_eq!(
                    names,
                    vec![
                        Some(&json!("execute_target_command")),
                        Some(&json!("submit"))
                    ]
                );
                Ok(reply)
            }
            Self::Command { request, reply } => {
                assert_eq!(body, &request);
                Ok(reply)
            }
        }
    }
}

// One coordinator observes both listeners, so the script constrains global
// ordering. No task is detached: try_join drops all work if either side fails.
pub async fn serve(
    inference: TcpListener,
    command: TcpListener,
    mut script: VecDeque<Exchange>,
    mut done: oneshot::Receiver<()>,
) -> TestResult {
    loop {
        let (endpoint, socket) = tokio::select! {
            socket = inference.accept() => (Endpoint::Inference, socket?.0),
            socket = command.accept() => (Endpoint::Command, socket?.0),
            result = &mut done => {
                result?;
                if !script.is_empty() { return Err(format!("loop ended with {} expected requests missing", script.len()).into()); }
                // Keep both listeners open after completion to catch queued or
                // delayed extra requests, including a retry after submission.
                let extra = timeout(Duration::from_millis(100), async {
                    tokio::select! {
                        result = inference.accept() => result.map(|_| Endpoint::Inference),
                        result = command.accept() => result.map(|_| Endpoint::Command),
                    }
                }).await;
                return match extra {
                    Err(_) => Ok(()),
                    Ok(Ok(endpoint)) => Err(format!("unexpected {endpoint:?} connection after completion").into()),
                    Ok(Err(error)) => Err(error.into()),
                };
            }
        };
        let expected = script
            .pop_front()
            .ok_or_else(|| format!("unexpected {endpoint:?} request after script ended"))?;
        if endpoint != expected.endpoint() {
            return Err(
                format!("expected {:?}, received {endpoint:?}", expected.endpoint()).into(),
            );
        }
        let mut socket = BufReader::new(socket);
        let body = read_request(&mut socket, endpoint).await?;
        match expected.check(&body)? {
            Reply::Disconnect => {}
            Reply::Json(value) => {
                let body = serde_json::to_vec(&value)?;
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await?;
                socket.write_all(&body).await?;
                socket.shutdown().await?;
            }
            Reply::Unavailable => {
                socket.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
                socket.shutdown().await?;
            }
        }
    }
}

async fn read_request(socket: &mut BufReader<TcpStream>, endpoint: Endpoint) -> TestResult<Value> {
    let path = match endpoint {
        Endpoint::Inference => "/v1/chat/completions",
        Endpoint::Command => "/v1/command",
    };
    let mut line = String::new();
    // A read cap keeps even a broken client from making the fixture allocate
    // indefinitely; the enclosing scenario timeout bounds all waiting.
    (&mut *socket).take(16384).read_line(&mut line).await?;
    assert_eq!(line, format!("POST {path} HTTP/1.1\r\n"));
    let mut remaining = 16384u64;
    let mut length = None;
    let mut content_type = None;
    let mut accept = None;
    loop {
        let mut line = String::new();
        let read = (&mut *socket).take(remaining).read_line(&mut line).await?;
        if read == 0 || !line.ends_with("\r\n") {
            return Err("incomplete or oversized request headers".into());
        }
        remaining = remaining
            .checked_sub(u64::try_from(read)?)
            .ok_or("header limit exceeded")?;
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').ok_or("invalid header")?;
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.trim().parse::<usize>()?);
        }
        if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.trim().to_owned());
        }
        if name.eq_ignore_ascii_case("accept") {
            accept = Some(value.trim().to_owned());
        }
    }
    assert_eq!(content_type.as_deref(), Some("application/json"));
    assert_eq!(accept.as_deref(), Some("application/json"));
    let length = length.ok_or("missing Content-Length")?;
    if length > LIMIT {
        return Err("request body too large".into());
    }
    let mut body = Vec::new();
    body.try_reserve_exact(length)?;
    body.resize(length, 0);
    socket.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}
