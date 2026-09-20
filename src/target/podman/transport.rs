//! HTTP connections are driven alongside their one request, never spawned.
//! Dropping an execution closes owned sockets, poisons the core, and notifies
//! supervision. It does not establish that target execution stopped.
use std::{future::Future, panic::AssertUnwindSafe, time::Duration};

use futures_util::FutureExt;
use http_body_util::{BodyExt, Full};
use hyper::{
    HeaderMap, Method, Request, Response, Version,
    body::{Bytes, Incoming},
    client::conn::http1,
    header,
};
use hyper_util::rt::TokioIo;
use tokio::{
    io::AsyncReadExt,
    net::UnixStream,
    sync::oneshot,
    time::{Instant, sleep, timeout_at},
};

use super::{Config, Error, Failure, Inspection, Session};
use crate::target::*;

/// Exactly one terminal notice per adapter. The receiver must remain alive
/// throughout the trial; receiver closure prevents new commands. Notices
/// request cleanup, never assert that the target or its descendants stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum SupervisionEvent {
    SessionUnusable { sequence: CommandSequence },
    ExecutionCancelled { sequence: CommandSequence },
    SessionDropped,
}

pub struct PodmanTargetSession {
    core: Session,
    supervisor: Option<oneshot::Sender<SupervisionEvent>>,
}
impl PodmanTargetSession {
    /// No IO at construction. Socket ownership, container identity, resource
    /// policy and isolation must already have been established by trusted setup.
    pub fn new(config: Config, supervisor: oneshot::Sender<SupervisionEvent>) -> Self {
        Self {
            core: Session::new(config),
            supervisor: Some(supervisor),
        }
    }
}
impl Drop for PodmanTargetSession {
    fn drop(&mut self) {
        notify(&mut self.supervisor, SupervisionEvent::SessionDropped);
    }
}

struct Notice<'a> {
    sender: &'a mut Option<oneshot::Sender<SupervisionEvent>>,
    sequence: CommandSequence,
    completed: bool,
}
impl Notice<'_> {
    fn finish(mut self, report: &ExecutionReport) {
        self.completed = true;
        if report.session_state() == SessionState::Unusable {
            notify(
                self.sender,
                SupervisionEvent::SessionUnusable {
                    sequence: self.sequence,
                },
            );
        }
    }
}
impl Drop for Notice<'_> {
    fn drop(&mut self) {
        if !self.completed {
            notify(
                self.sender,
                SupervisionEvent::ExecutionCancelled {
                    sequence: self.sequence,
                },
            );
        }
    }
}
fn notify(sender: &mut Option<oneshot::Sender<SupervisionEvent>>, event: SupervisionEvent) {
    if let Some(sender) = sender.take() {
        // A foreign task's waker may unwind while it is notified. No notification
        // mechanism can guarantee delivery after the receiver itself disappears.
        if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| sender.send(event))) {
            std::mem::forget(payload);
        }
    }
}

impl TargetSession for PodmanTargetSession {
    async fn execute(&mut self, request: CommandRequest) -> ExecutionReport {
        let sequence = request.sequence;
        let available = self.supervisor.as_ref().is_some_and(|s| !s.is_closed());
        let notice = Notice {
            sender: &mut self.supervisor,
            sequence,
            completed: false,
        };
        let core = &mut self.core;
        let result = AssertUnwindSafe(async {
            if !available {
                core.invalidate();
            }
            let socket = core.config().socket_path().to_owned();
            let limit = core.config().limits().json_bytes();
            let deadline = Instant::now().checked_add(core.config().limits().deadline());
            let mut report = run(core, request, &socket, limit, deadline).await;
            // Bounded pure decoding is synchronous: a timer cannot preempt it.
            // Do not restore readiness if final decoding crossed the deadline.
            if deadline.is_some_and(|end| Instant::now() >= end)
                && let ExecutionOutcome::Completed { output, .. } = &mut report.outcome
            {
                let output = std::mem::take(output);
                core.invalidate();
                report.outcome = ExecutionOutcome::DeadlineExceeded {
                    output,
                    execution_state: ExecutionState::MayStillBeRunning,
                };
            }
            report
        })
        .catch_unwind()
        .await;
        let report = match result {
            Ok(report) => report,
            Err(payload) => {
                std::mem::forget(payload);
                core.invalidate();
                ExecutionReport {
                    sequence,
                    outcome: ExecutionOutcome::Unknown {
                        output: CommandOutput::default(),
                        error: ExecutionError {
                            kind: ExecutionErrorKind::DependencyPanicked,
                            diagnostic: None,
                        },
                    },
                }
            }
        };
        notice.finish(&report);
        report
    }
}

async fn run(
    core: &mut Session,
    request: CommandRequest,
    socket_path: &str,
    limit: usize,
    deadline: Option<Instant>,
) -> ExecutionReport {
    let creating = match core.begin(request) {
        Ok(stage) => stage,
        Err(report) => return report,
    };
    let Some(deadline) = deadline else {
        return creating.fail(Failure::Deadline);
    };
    let body = match bounded(
        deadline,
        json(
            socket_path,
            Method::POST,
            creating.path(),
            creating.body(),
            201,
            limit,
        ),
    )
    .await
    {
        Ok(body) => body,
        Err(error) => return creating.fail(error),
    };
    let starting = match creating.created(201, &body) {
        Ok(stage) => stage,
        Err(report) => return report,
    };
    let (mut socket, initial) = match bounded(
        deadline,
        upgrade(socket_path, starting.path(), starting.body()),
    )
    .await
    {
        Ok(parts) => parts,
        Err(error) => return starting.fail(error),
    };
    let mut capture = match starting.attached(101) {
        Ok(stage) => stage,
        Err(report) => return report,
    };
    capture = match capture.feed(&initial) {
        Ok(stage) => stage,
        Err(report) => return report,
    };
    let mut buffer = [0u8; 8192];
    loop {
        let read = bounded(deadline, async {
            socket
                .read(&mut buffer)
                .await
                .map_err(|_| Failure::Transport)
        })
        .await;
        match read {
            Ok(0) => break,
            Ok(count) => {
                let Some(bytes) = buffer.get(..count) else {
                    return capture.fail(invalid("socket read exceeded buffer"));
                };
                capture = match capture.feed(bytes) {
                    Ok(stage) => stage,
                    Err(report) => return report,
                };
            }
            Err(error) => return capture.fail(error),
        }
    }
    drop(socket);
    let mut inspect = match capture.eof() {
        Ok(stage) => stage,
        Err(report) => return report,
    };
    loop {
        let body = match bounded(
            deadline,
            json(socket_path, Method::GET, inspect.path(), &[], 200, limit),
        )
        .await
        {
            Ok(body) => body,
            Err(error) => return inspect.fail(error),
        };
        match inspect.observed(200, &body) {
            Ok(Inspection::Finished(report)) => return report,
            Ok(Inspection::Pending(next)) => inspect = next,
            Err(report) => return report,
        }
        // Poll only inspection, never re-create or restart. All polls and delays
        // share the command's original deadline.
        if let Err(error) = bounded(deadline, async {
            sleep(Duration::from_millis(25)).await;
            Ok(())
        })
        .await
        {
            return inspect.fail(error);
        }
    }
}

async fn bounded<T>(
    deadline: Instant,
    operation: impl Future<Output = Result<T, Failure>>,
) -> Result<T, Failure> {
    match AssertUnwindSafe(async {
        // Tokio may poll an already-ready operation before noticing expiration.
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        timeout_at(deadline, operation)
            .await
            .map_err(|_| Failure::Deadline)?
    })
    .catch_unwind()
    .await
    {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            Err(Failure::DependencyPanicked)
        }
    }
}

type Body = Full<Bytes>;
fn request(
    method: Method,
    path: &str,
    body: &[u8],
    upgrade: bool,
) -> Result<Request<Body>, Failure> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(body.len())
        .map_err(|e| Failure::Response(Error::Allocation(e)))?;
    bytes.extend_from_slice(body);
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::CONNECTION,
            if upgrade { "Upgrade" } else { "close" },
        );
    if upgrade {
        builder = builder.header(header::UPGRADE, "tcp");
    }
    builder
        .body(Full::new(Bytes::from(bytes)))
        .map_err(|_| invalid("invalid HTTP request"))
}
async fn connect(
    path: &str,
) -> Result<
    (
        http1::SendRequest<Body>,
        http1::Connection<TokioIo<UnixStream>, Body>,
    ),
    Failure,
> {
    let socket = UnixStream::connect(path)
        .await
        .map_err(|_| Failure::Transport)?;
    // Hyper's buffer minimum is 8192. Fixed, validated constants avoid its
    // builder precondition panic; the effect boundary catches other unwinds.
    http1::Builder::new()
        .max_headers(32)
        .max_buf_size(16384)
        .handshake(TokioIo::new(socket))
        .await
        .map_err(http_error)
}

async fn json(
    socket: &str,
    method: Method,
    path: &str,
    body: &[u8],
    expected: u16,
    limit: usize,
) -> Result<Vec<u8>, Failure> {
    let request = request(method, path, body, false)?;
    let (mut send, connection) = connect(socket).await?;
    let exchange = async move {
        let response = send.send_request(request).await.map_err(http_error)?;
        drop(send);
        if response.status().as_u16() != expected {
            return Err(Failure::Response(Error::HttpStatus(
                response.status().as_u16(),
            )));
        }
        validate_json(&response, limit)?;
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let data = frame
                .map_err(http_error)?
                .into_data()
                .map_err(|_| invalid("unexpected JSON trailers"))?;
            append(&mut bytes, &data, limit)?;
        }
        Ok(bytes)
    };
    let drive = async { connection.without_shutdown().await.map_err(http_error) };
    let (body, _) = tokio::try_join!(exchange, drive)?;
    Ok(body)
}

async fn upgrade(socket: &str, path: &str, body: &[u8]) -> Result<(UnixStream, Bytes), Failure> {
    let request = request(Method::POST, path, body, true)?;
    let (mut send, connection) = connect(socket).await?;
    let exchange = async move {
        let response = send.send_request(request).await.map_err(http_error)?;
        drop(send);
        validate_upgrade(&response)?;
        Ok(())
    };
    let drive = async { connection.without_shutdown().await.map_err(http_error) };
    let ((), parts) = tokio::try_join!(exchange, drive)?;
    // HTTP can read the first stream frames with the 101 headers. Preserve them.
    Ok((parts.io.into_inner(), parts.read_buf))
}

fn one_header(headers: &HeaderMap, name: header::HeaderName, value: &[u8]) -> bool {
    let mut values = headers.get_all(name).iter();
    values
        .next()
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(value))
        && values.next().is_none()
}
fn validate_json(response: &Response<Incoming>, limit: usize) -> Result<(), Failure> {
    if response.version() != Version::HTTP_11
        || !one_header(
            response.headers(),
            header::CONTENT_TYPE,
            b"application/json",
        )
        || response.headers().contains_key(header::CONTENT_ENCODING)
    {
        return Err(invalid("expected unencoded HTTP/1.1 JSON response"));
    }
    if let Some(value) = response.headers().get(header::CONTENT_LENGTH) {
        let length = value
            .to_str()
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| invalid("invalid content length"))?;
        if length > limit as u64 {
            return Err(Failure::Response(Error::BodyTooLarge { limit }));
        }
    }
    Ok(())
}
fn validate_upgrade(response: &Response<Incoming>) -> Result<(), Failure> {
    if response.status().as_u16() != 101 {
        return Err(Failure::Response(Error::HttpStatus(
            response.status().as_u16(),
        )));
    }
    let headers = response.headers();
    let mut has_upgrade = false;
    for value in headers.get_all(header::CONNECTION) {
        let value = value
            .to_str()
            .map_err(|_| invalid("invalid connection header"))?;
        for token in value.split(',').map(str::trim) {
            if token.eq_ignore_ascii_case("close") {
                return Err(invalid("upgrade also requested close"));
            }
            has_upgrade |= token.eq_ignore_ascii_case("upgrade");
        }
    }
    if response.version() != Version::HTTP_11
        || !has_upgrade
        || !one_header(headers, header::UPGRADE, b"tcp")
        || [
            header::CONTENT_LENGTH,
            header::TRANSFER_ENCODING,
            header::CONTENT_ENCODING,
        ]
        .iter()
        .any(|h| headers.contains_key(h))
    {
        return Err(invalid("invalid TCP upgrade response"));
    }
    Ok(())
}
fn append(bytes: &mut Vec<u8>, data: &[u8], limit: usize) -> Result<(), Failure> {
    if bytes
        .len()
        .checked_add(data.len())
        .is_none_or(|size| size > limit)
    {
        return Err(Failure::Response(Error::BodyTooLarge { limit }));
    }
    bytes
        .try_reserve_exact(data.len())
        .map_err(|e| Failure::Response(Error::Allocation(e)))?;
    bytes.extend_from_slice(data);
    Ok(())
}
fn invalid(reason: &'static str) -> Failure {
    Failure::Response(Error::InvalidResponse(reason))
}
fn http_error(error: hyper::Error) -> Failure {
    if error.is_parse() {
        invalid("malformed HTTP framing")
    } else {
        Failure::Transport
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn json_accumulation_is_bounded_and_rejection_preserves_bytes(
            original in prop::collection::vec(any::<u8>(), 0..128),
            chunk in prop::collection::vec(any::<u8>(), 0..128), limit in 0usize..256,
        ) {
            let mut bytes = original.clone();
            let accepted = original.len() + chunk.len() <= limit;
            prop_assert_eq!(append(&mut bytes, &chunk, limit).is_ok(), accepted);
            prop_assert_eq!(bytes, if accepted { [original, chunk].concat() } else { original });
        }
    }

    #[tokio::test]
    async fn effect_unwinds_are_errors_and_expired_effects_are_never_polled()
    -> Result<(), Box<dyn std::error::Error>> {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(1))
            .ok_or("clock overflow")?;
        for suspend in [false, true] {
            let result: Result<(), Failure> = bounded(deadline, async {
                if suspend {
                    tokio::task::yield_now().await;
                }
                // Inject a foreign unwind without changing the global panic hook.
                std::panic::resume_unwind(Box::new("injected IO unwind"))
            })
            .await;
            assert!(matches!(result, Err(Failure::DependencyPanicked)));
        }
        let polled = std::cell::Cell::new(false);
        let result = bounded(Instant::now(), async {
            polled.set(true);
            Ok(())
        })
        .await;
        assert!(matches!(result, Err(Failure::Deadline)));
        assert!(!polled.get());
        Ok(())
    }
}
