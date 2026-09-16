//! Bounded harness-to-broker HTTP transport. One client belongs to one run.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    time::{Duration, Instant},
};

use futures_util::FutureExt;
use reqwest::{Client, Url, header};

use crate::{
    command_protocol::{self as protocol, ProtocolError, SequenceTracker},
    harness::{
        async_driver::CommandExecutor,
        types::{CommandCall, CommandClientError, CommandClientErrorKind as Kind},
    },
    target::{CommandSequence, ExecutionReport},
};

pub struct CommandConfig {
    /// Full HTTP(S) /v1/command endpoint, without credentials, query, or fragment.
    pub command_url: String,
    pub connect_timeout: Duration,
    /// Covers dispatch through the last response byte. Must allow the broker's
    /// separately configured execution deadline, cleanup, and reporting.
    pub request_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
}

/// No Clone or reset: replacing this client within a run would lose sequencing.
pub struct CommandClient {
    client: Client,
    endpoint: Url,
    config: CommandConfig,
    sequences: SequenceTracker,
}

impl CommandClient {
    /// Requires a Tokio runtime with IO and time enabled when executing commands.
    pub fn new(config: CommandConfig) -> Result<Self, CommandClientError> {
        guard(catch_unwind(AssertUnwindSafe(|| Self::build(config))))
    }

    fn build(config: CommandConfig) -> Result<Self, CommandClientError> {
        let endpoint =
            Url::parse(&config.command_url).map_err(|error| failure(Kind::Configuration, error))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.path() != protocol::PATH
        {
            return Err(failure(
                Kind::Configuration,
                "expected an HTTP(S) /v1/command URL without credentials, query, or fragment",
            ));
        }
        let now = Instant::now();
        if config.max_request_bytes == 0
            || config.max_response_bytes == 0
            || config.connect_timeout.is_zero()
            || config.request_timeout.is_zero()
            || now.checked_add(config.connect_timeout).is_none()
            || now.checked_add(config.request_timeout).is_none()
        {
            return Err(failure(
                Kind::Configuration,
                "positive, representable limits are required",
            ));
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .http1_only()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| failure(Kind::Configuration, error))?;
        Ok(Self {
            client,
            endpoint,
            config,
            sequences: SequenceTracker::new(),
        })
    }

    async fn round_trip(&mut self, command: &str) -> Result<ExecutionReport, CommandClientError> {
        let sequence = self.sequences.next_sequence().ok_or_else(|| {
            failure(
                Kind::SessionUnavailable,
                "session is unusable, outstanding, or exhausted",
            )
        })?;
        let body = protocol::encode_command(sequence, command, self.config.max_request_bytes)
            .map_err(|error| protocol_failure(error, Direction::Request))?;
        // No suspension before reservation. Cancellation from any subsequent
        // await leaves Awaiting, which permanently prevents further dispatch.
        self.sequences
            .begin(sequence)
            .map_err(|error| failure(Kind::SessionUnavailable, error))?;
        let report = self.send(body, sequence).await?;
        self.sequences
            .finish(&report)
            .map_err(|error| failure(Kind::SessionUnavailable, error))?;
        Ok(report)
    }

    async fn send(
        &self,
        body: Vec<u8>,
        sequence: CommandSequence,
    ) -> Result<ExecutionReport, CommandClientError> {
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(header::CONTENT_TYPE, protocol::CONTENT_TYPE)
            .header(header::ACCEPT, protocol::CONTENT_TYPE)
            .body(body)
            .send()
            .await
            .map_err(|error| failure(Kind::Transport, error))?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(failure(
                Kind::HttpStatus(response.status().as_u16()),
                "expected HTTP 200 execution report",
            ));
        }
        validate_content_type(response.headers())?;
        let limit = self.config.max_response_bytes;
        if response
            .content_length()
            .is_some_and(|length| usize::try_from(length).map_or(true, |length| length > limit))
        {
            return Err(failure(
                Kind::ResponseTooLarge { limit },
                "response Content-Length exceeds limit",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| failure(Kind::Transport, error))?
        {
            append_chunk(&mut bytes, &chunk, limit)?;
        }
        protocol::decode_report(&bytes, limit, sequence)
            .map_err(|error| protocol_failure(error, Direction::Response))
    }
}

impl CommandExecutor for CommandClient {
    /// One attempt only. Every error ends this client's session, including local
    /// rejection. Dropping a polled, pending future also prevents reuse. Neither
    /// action stops remote execution; supervision must handle that separately.
    async fn execute(&mut self, call: &CommandCall) -> Result<ExecutionReport, CommandClientError> {
        let result = guard(
            AssertUnwindSafe(self.round_trip(&call.command))
                .catch_unwind()
                .await,
        );
        if result.is_err() {
            self.sequences.abandon();
        }
        result
    }
}

fn validate_content_type(headers: &header::HeaderMap) -> Result<(), CommandClientError> {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let valid = values
        .next()
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(protocol::CONTENT_TYPE));
    if !valid || values.next().is_some() {
        return Err(failure(
            Kind::InvalidResponse,
            "expected one Content-Type: application/json without parameters",
        ));
    }
    Ok(())
}

/// Pure accumulation policy: rejected chunks leave the buffer unchanged.
fn append_chunk(bytes: &mut Vec<u8>, chunk: &[u8], limit: usize) -> Result<(), CommandClientError> {
    let total = bytes
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| failure(Kind::ResponseTooLarge { limit }, "response size overflow"))?;
    if total > limit {
        return Err(failure(
            Kind::ResponseTooLarge { limit },
            "response body exceeds limit",
        ));
    }
    if total > bytes.capacity() {
        let capacity = bytes
            .capacity()
            .max(64)
            .saturating_mul(2)
            .min(limit)
            .max(total);
        bytes
            .try_reserve_exact(capacity.saturating_sub(bytes.len()))
            .map_err(|error| failure(Kind::Allocation, error))?;
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

#[derive(Clone, Copy)]
enum Direction {
    Request,
    Response,
}

fn protocol_failure(error: ProtocolError, direction: Direction) -> CommandClientError {
    let kind = match &error {
        ProtocolError::BodyTooLarge { limit } => match direction {
            Direction::Request => Kind::RequestTooLarge { limit: *limit },
            Direction::Response => Kind::ResponseTooLarge { limit: *limit },
        },
        ProtocolError::Json(_) => match direction {
            Direction::Request => Kind::InvalidRequest,
            Direction::Response => Kind::InvalidResponse,
        },
        ProtocolError::Allocation(_) => Kind::Allocation,
        ProtocolError::SequenceMismatch { expected, received } => Kind::SequenceMismatch {
            expected: *expected,
            received: *received,
        },
        ProtocolError::DependencyPanicked => Kind::Panicked,
    };
    failure(kind, error)
}

fn failure(kind: Kind, diagnostic: impl std::fmt::Display) -> CommandClientError {
    CommandClientError {
        kind,
        diagnostic: Some(diagnostic.to_string()),
    }
}

fn guard<T>(
    result: std::thread::Result<Result<T, CommandClientError>>,
) -> Result<T, CommandClientError> {
    match result {
        Ok(result) => result,
        Err(payload) => {
            // A foreign panic payload's destructor may also panic.
            std::mem::forget(payload);
            Err(CommandClientError::panicked())
        }
    }
}

#[cfg(test)]
mod tests;
