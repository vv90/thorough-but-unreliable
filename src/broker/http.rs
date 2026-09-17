//! HTTP framing is delegated to Hyper; application bodies and concurrency have
//! separate bounds. One request per connection makes local write completion
//! explicit. This is not an acknowledgment that the peer received the report.
use std::{convert::Infallible, panic::AssertUnwindSafe, sync::Arc};

use futures_util::FutureExt;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response, StatusCode, Version,
    body::{Bytes, Incoming},
    header,
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::{
    net::TcpStream,
    sync::{Mutex, Semaphore, mpsc, oneshot},
    time::{Instant, timeout, timeout_at},
};

use super::{Config, Submission};
use crate::command_protocol::{self, ProtocolError};

pub(super) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}
impl Reply {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
        }
    }
    fn response(self) -> Response<Full<Bytes>> {
        let mut response = Response::new(Full::new(Bytes::from(self.body)));
        *response.status_mut() = match StatusCode::from_u16(self.status) {
            Ok(status) => status,
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static(command_protocol::CONTENT_TYPE),
        );
        response.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("close"),
        );
        response
    }
}

pub(super) async fn connection(
    socket: TcpStream,
    send: mpsc::Sender<Submission>,
    gate: Arc<Semaphore>,
    config: Config,
) {
    let (delivered, receive) = oneshot::channel();
    let receive = Arc::new(Mutex::new(Some(receive)));
    let operation = async {
        let Some(read_deadline) = Instant::now().checked_add(config.read_timeout) else {
            return false;
        };
        let service = service_fn(|request| {
            let receive = receive.clone();
            let send = send.clone();
            let gate = gate.clone();
            let config = config.clone();
            async move {
                let reply = match handle(request, send, gate, receive, &config, read_deadline).await
                {
                    Ok(reply) => reply,
                    Err(status) => Reply::status(status),
                };
                Ok::<_, Infallible>(reply.response())
            }
        });
        let mut builder = http1::Builder::new();
        builder
            .keep_alive(false)
            .max_headers(32)
            .max_buf_size(16384)
            .timer(TokioTimer::new())
            .header_read_timeout(config.read_timeout);
        matches!(
            timeout(
                config.connection_timeout,
                builder.serve_connection(TokioIo::new(socket), service)
            )
            .await,
            Ok(Ok(()))
        )
    };
    let success = match AssertUnwindSafe(operation).catch_unwind().await {
        Ok(success) => success,
        Err(payload) => {
            std::mem::forget(payload);
            false
        }
    };
    let _ = delivered.send(success);
}

async fn handle(
    request: Request<Incoming>,
    send: mpsc::Sender<Submission>,
    gate: Arc<Semaphore>,
    delivered: Arc<Mutex<Option<oneshot::Receiver<bool>>>>,
    config: &Config,
    read_deadline: Instant,
) -> Result<Reply, u16> {
    if request.version() != Version::HTTP_11 {
        return Err(505);
    }
    if request.method() != command_protocol::METHOD {
        return Err(405);
    }
    if request.uri().path_and_query().map(|p| p.as_str()) != Some(command_protocol::PATH) {
        return Err(404);
    }
    let mut types = request.headers().get_all(header::CONTENT_TYPE).iter();
    if !types.next().is_some_and(|v| {
        v.as_bytes()
            .eq_ignore_ascii_case(command_protocol::CONTENT_TYPE.as_bytes())
    }) || types.next().is_some()
    {
        return Err(415);
    }
    if request.headers().contains_key(header::CONTENT_ENCODING) {
        return Err(415);
    }
    if request.headers().contains_key(header::EXPECT) {
        return Err(417);
    }
    if let Some(length) = request.headers().get(header::CONTENT_LENGTH) {
        let length = length
            .to_str()
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or(400u16)?;
        if length > config.request_bytes as u64 {
            return Err(413);
        }
    }
    let bytes = timeout_at(
        read_deadline,
        read_body(request.into_body(), config.request_bytes),
    )
    .await
    .map_err(|_| 408u16)??;
    let request = command_protocol::decode_request(&bytes, config.request_bytes).map_err(
        |error| match error {
            ProtocolError::BodyTooLarge { .. } => 413u16,
            ProtocolError::Json(_) | ProtocolError::SequenceMismatch { .. } => 400,
            ProtocolError::Allocation(_) | ProtocolError::DependencyPanicked => 500,
        },
    )?;
    let permit = gate.try_acquire_owned().map_err(|_| 409u16)?;
    let delivered = delivered.lock().await.take().ok_or(409u16)?;
    let (response, receive) = oneshot::channel();
    send.try_send(Submission {
        request,
        response,
        delivered,
        _permit: permit,
    })
    .map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => 409u16,
        mpsc::error::TrySendError::Closed(_) => 503,
    })?;
    receive.await.map_err(|_| 503)
}

async fn read_body(mut body: Incoming, limit: usize) -> Result<Vec<u8>, u16> {
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let data = frame.map_err(|_| 400u16)?.into_data().map_err(|_| 400u16)?;
        append(&mut bytes, &data, limit)?;
    }
    Ok(bytes)
}

fn append(bytes: &mut Vec<u8>, data: &[u8], limit: usize) -> Result<(), u16> {
    let size = bytes.len().checked_add(data.len()).ok_or(413u16)?;
    if size > limit {
        return Err(413);
    }
    bytes.try_reserve_exact(data.len()).map_err(|_| 500u16)?;
    bytes.extend_from_slice(data);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn body_bound_is_exact_and_rejection_preserves_buffer(
            original in prop::collection::vec(any::<u8>(), 0..512),
            chunk in prop::collection::vec(any::<u8>(), 0..512), limit in 0usize..1024,
        ) {
            let mut bytes = original.clone();
            let result = append(&mut bytes, &chunk, limit);
            if original.len() + chunk.len() <= limit {
                prop_assert!(result.is_ok());
                prop_assert_eq!(bytes, [original, chunk].concat());
            } else {
                prop_assert_eq!(result, Err(413));
                prop_assert_eq!(bytes, original);
            }
        }
    }
}
