use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    time::{Duration, Instant},
};

use futures_util::FutureExt;
use reqwest::{Client, Url};

use super::{InferenceError, wire};
use crate::harness::types::{AssistantResponse, Message};

pub struct InferenceConfig {
    /// Full endpoint, e.g. http://10.99.1.1:11434/v1/chat/completions.
    pub completion_url: String,
    pub model: String,
    pub max_tokens: u32,
    pub connect_timeout: Duration,
    /// Total HTTP deadline, including response body reads.
    pub request_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
}

pub struct InferenceClient {
    client: Client,
    endpoint: Url,
    config: InferenceConfig,
}

impl InferenceClient {
    pub fn new(config: InferenceConfig) -> Result<Self, InferenceError> {
        guard(catch_unwind(AssertUnwindSafe(|| Self::build(config))))
    }

    fn build(config: InferenceConfig) -> Result<Self, InferenceError> {
        let endpoint = Url::parse(&config.completion_url)
            .map_err(|error| InferenceError::Configuration(error.to_string()))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
            || endpoint.query().is_some()
            || endpoint.path() != "/v1/chat/completions"
        {
            return Err(InferenceError::Configuration("expected an HTTP(S) /v1/chat/completions URL without credentials, query, or fragment".into()));
        }
        if config.model.trim().is_empty()
            || config.max_tokens == 0
            || config.max_request_bytes == 0
            || config.max_response_bytes == 0
            || config.connect_timeout.is_zero()
            || config.request_timeout.is_zero()
            || Instant::now().checked_add(config.connect_timeout).is_none()
            || Instant::now().checked_add(config.request_timeout).is_none()
        {
            return Err(InferenceError::Configuration(
                "model and positive, representable limits are required".into(),
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
            .map_err(InferenceError::ClientBuild)?;
        Ok(Self {
            client,
            endpoint,
            config,
        })
    }

    /// Requires a Tokio runtime with IO/time enabled. It performs one request,
    /// including bounded body reading; it never retries. Discard after panic.
    pub async fn complete(&self, history: &[Message]) -> Result<AssistantResponse, InferenceError> {
        guard(
            AssertUnwindSafe(self.round_trip(history))
                .catch_unwind()
                .await,
        )
    }

    async fn round_trip(&self, history: &[Message]) -> Result<AssistantResponse, InferenceError> {
        let body = wire::encode_request(&self.config.model, self.config.max_tokens, history)?;
        if body.len() > self.config.max_request_bytes {
            return Err(InferenceError::RequestTooLarge {
                limit: self.config.max_request_bytes,
            });
        }
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(InferenceError::Transport)?;
        if !response.status().is_success() {
            return Err(InferenceError::HttpStatus(response.status().as_u16()));
        }
        let limit = self.config.max_response_bytes;
        if let Some(length) = response.content_length()
            && usize::try_from(length).map_or(true, |length| length > limit)
        {
            return Err(InferenceError::ResponseTooLarge { limit });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(InferenceError::Transport)? {
            append_chunk(&mut bytes, &chunk, limit)?;
        }
        wire::decode_response(&bytes)
    }
}

/// Check before appending, even when Content-Length is absent.
pub(super) fn append_chunk(
    bytes: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
) -> Result<(), InferenceError> {
    let total = bytes
        .len()
        .checked_add(chunk.len())
        .ok_or(InferenceError::ResponseTooLarge { limit })?;
    if total > limit {
        return Err(InferenceError::ResponseTooLarge { limit });
    }
    bytes
        .try_reserve_exact(chunk.len())
        .map_err(InferenceError::Allocation)?;
    bytes.extend_from_slice(chunk);
    Ok(())
}

fn guard<T>(result: std::thread::Result<Result<T, InferenceError>>) -> Result<T, InferenceError> {
    match result {
        Ok(result) => result,
        Err(payload) => {
            // As with the core driver, a foreign payload's Drop may panic too.
            std::mem::forget(payload);
            Err(InferenceError::DependencyPanicked)
        }
    }
}
