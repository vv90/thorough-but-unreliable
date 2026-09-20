//! One prepared target, one session owner, bounded HTTP connections. No target
//! creation or authentication. Supervision must await shutdown rather than drop
//! this server: dropping it cannot confirm that remote execution stopped.

mod http;
pub mod service;
mod state;

use crate::{command_protocol::SequenceTracker, target::*};
use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use std::{
    fmt, future::Future, num::NonZeroUsize, panic::AssertUnwindSafe, sync::Arc, time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::timeout,
};

/// Immutable validated bounds. The adapter watchdog includes its execution,
/// cleanup and reporting allowance; it is not proof of target termination.
#[derive(Clone, Debug)]
pub struct Config {
    request_bytes: usize,
    response_bytes: usize,
    connections: usize,
    read_timeout: Duration,
    adapter_timeout: Duration,
    write_timeout: Duration,
    connection_timeout: Duration,
}
impl Config {
    pub fn new(
        request_bytes: NonZeroUsize,
        response_bytes: NonZeroUsize,
        connections: NonZeroUsize,
        read_timeout: Duration,
        adapter_timeout: Duration,
        write_timeout: Duration,
    ) -> Result<Self, Error> {
        let connection_timeout = read_timeout
            .checked_add(adapter_timeout)
            .and_then(|d| d.checked_add(write_timeout))
            .ok_or(Error::Configuration)?;
        // Small fixed ceiling avoids platform-clock overflow and accidental
        // effectively unbounded watchdogs without reading a clock during validation.
        if [read_timeout, adapter_timeout, write_timeout]
            .iter()
            .any(Duration::is_zero)
            || connection_timeout > Duration::from_secs(86400)
            || request_bytes.get() > isize::MAX as usize
            || response_bytes.get() > isize::MAX as usize
        {
            return Err(Error::Configuration);
        }
        Ok(Self {
            request_bytes: request_bytes.get(),
            response_bytes: response_bytes.get(),
            connections: connections.get(),
            read_timeout,
            adapter_timeout,
            write_timeout,
            connection_timeout,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopReason {
    Shutdown,
    SessionUnusable,
    SequenceExhausted,
    DeliveryFailed,
    InvalidReport,
}

#[derive(Debug)]
pub enum Error {
    Configuration,
    Listener(std::io::Error),
    DependencyPanicked,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration => f.write_str("invalid broker bounds"),
            Self::Listener(e) => write!(f, "broker listener failed: {e}"),
            Self::DependencyPanicked => {
                f.write_str("broker dependency panicked; target execution may continue")
            }
        }
    }
}
impl std::error::Error for Error {}

struct Submission {
    request: CommandRequest,
    response: oneshot::Sender<http::Reply>,
    delivered: oneshot::Receiver<bool>,
    // Admission capacity is reserved before enqueueing; no second command can
    // queue behind execution or undelivered output.
    _permit: OwnedSemaphorePermit,
}

/// Connections may disappear without cancelling target execution. Shutdown
/// closes admission, drains accepted work under the watchdog, then returns.
/// Every terminal return requires discarding this session. The caller supervises
/// the target's lifecycle, including possible continued execution after failure.
pub async fn serve<S: TargetSession + Send>(
    listener: TcpListener,
    session: S,
    config: Config,
    shutdown: impl Future<Output = ()>,
) -> Result<StopReason, Error> {
    match AssertUnwindSafe(run(listener, session, config, shutdown))
        .catch_unwind()
        .await
    {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            Err(Error::DependencyPanicked)
        }
    }
}

async fn run<S: TargetSession + Send>(
    listener: TcpListener,
    session: S,
    config: Config,
    shutdown: impl Future<Output = ()>,
) -> Result<StopReason, Error> {
    let (send, receive) = mpsc::channel(1);
    let gate = Arc::new(Semaphore::new(1));
    let mut send = Some(send);
    let mut connections = FuturesUnordered::new();
    let owner = own(session, receive, config.clone());
    tokio::pin!(owner, shutdown);
    let mut listener_error = None;
    loop {
        tokio::select! {
            result = &mut owner => {
                gate.close();
                // Dropping remaining connections cannot cancel the now-finished owner.
                return match listener_error { Some(error) => Err(Error::Listener(error)), None => Ok(result) };
            }
            () = &mut shutdown, if send.is_some() => { gate.close(); send = None; }
            accepted = listener.accept(), if send.is_some() && connections.len() < config.connections => {
                match accepted {
                    Ok((socket, _)) => {
                        if let Some(send) = &send {
                            connections.push(http::connection(socket, send.clone(), gate.clone(), config.clone()));
                        }
                    }
                    Err(error) => { listener_error = Some(error); gate.close(); send = None; }
                }
            }
            _ = connections.next(), if !connections.is_empty() => {}
        }
    }
}

async fn own<S: TargetSession + Send>(
    mut session: S,
    mut requests: mpsc::Receiver<Submission>,
    config: Config,
) -> StopReason {
    let mut sequences = SequenceTracker::new();
    while let Some(submission) = requests.recv().await {
        let Submission {
            request,
            response,
            delivered,
            _permit,
        } = submission;
        if sequences.begin(request.sequence).is_err() {
            let _ = response.send(http::Reply::status(409));
            continue;
        }
        if response.is_closed() {
            return StopReason::DeliveryFailed;
        }
        let sequence = request.sequence;
        let operation = AssertUnwindSafe(async { session.execute(request).await }).catch_unwind();
        let report = match timeout(config.adapter_timeout, operation).await {
            Ok(Ok(report)) => report,
            Ok(Err(payload)) => {
                std::mem::forget(payload);
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
            Err(_) => ExecutionReport {
                sequence,
                outcome: ExecutionOutcome::DeadlineExceeded {
                    output: CommandOutput::default(),
                    execution_state: ExecutionState::MayStillBeRunning,
                },
            },
        };
        let prepared = state::prepare(&report, sequence, config.response_bytes);
        let (reply, invalid) = match prepared {
            Ok(body) => (http::Reply { status: 200, body }, false),
            Err(_) => (http::Reply::status(500), true),
        };
        if response.send(reply).is_err() {
            return StopReason::DeliveryFailed;
        }
        if !matches!(timeout(config.write_timeout, delivered).await, Ok(Ok(true))) {
            return StopReason::DeliveryFailed;
        }
        if invalid {
            return StopReason::InvalidReport;
        }
        if sequences.finish(&report).is_err() {
            return StopReason::InvalidReport;
        }
        if report.session_state() == SessionState::Unusable {
            return StopReason::SessionUnusable;
        }
        if sequences.next_sequence().is_none() {
            return StopReason::SequenceExhausted;
        }
        // Drop the admission permit only after confirmed local response write.
    }
    StopReason::Shutdown
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn configuration_requires_positive_timeouts_with_a_bounded_total(
            read in 0u64..100000, adapter in 0u64..100000, write in 0u64..100000,
        ) {
            let result = Config::new(NonZeroUsize::MIN, NonZeroUsize::MIN, NonZeroUsize::MIN,
                Duration::from_secs(read), Duration::from_secs(adapter), Duration::from_secs(write));
            prop_assert_eq!(result.is_ok(), read > 0 && adapter > 0 && write > 0 && read + adapter + write <= 86400);
        }
    }
}
