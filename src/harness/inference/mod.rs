//! Asynchronous inference round trips and integration with the async loop driver.

mod http;
pub mod wire;

pub use http::{InferenceClient, InferenceConfig};

use std::{collections::TryReserveError, fmt};

use crate::harness::{
    async_driver,
    types::{AssistantResponse, Message, ModelFailure, ModelFailureKind},
};

impl async_driver::ModelClient for InferenceClient {
    async fn respond(&mut self, history: &[Message]) -> Result<AssistantResponse, ModelFailure> {
        self.complete(history).await.map_err(ModelFailure::from)
    }
}

impl From<InferenceError> for ModelFailure {
    fn from(error: InferenceError) -> Self {
        let diagnostic = Some(error.to_string());
        let kind = match error {
            InferenceError::Configuration(_) => ModelFailureKind::Configuration,
            InferenceError::InvalidHistory(_) => ModelFailureKind::InvalidHistory,
            InferenceError::Json(_) => ModelFailureKind::Json,
            InferenceError::InvalidResponse(_) => ModelFailureKind::InvalidResponse,
            InferenceError::UnsupportedTool(name) => ModelFailureKind::UnsupportedTool(name),
            InferenceError::TruncatedResponse => ModelFailureKind::TokenLimit,
            InferenceError::ClientBuild(_) => ModelFailureKind::ClientBuild,
            InferenceError::Transport(_) => ModelFailureKind::Transport,
            InferenceError::HttpStatus(status) => ModelFailureKind::HttpStatus(status),
            InferenceError::RequestTooLarge { limit } => {
                ModelFailureKind::RequestTooLarge { limit }
            }
            InferenceError::ResponseTooLarge { limit } => {
                ModelFailureKind::ResponseTooLarge { limit }
            }
            InferenceError::Allocation(_) => ModelFailureKind::Allocation,
            InferenceError::DependencyPanicked => ModelFailureKind::Panicked,
        };
        Self { kind, diagnostic }
    }
}

#[derive(Debug)]
pub enum InferenceError {
    Configuration(String),
    InvalidHistory(&'static str),
    Json(serde_json::Error),
    InvalidResponse(&'static str),
    UnsupportedTool(String),
    TruncatedResponse,
    ClientBuild(reqwest::Error),
    /// Dispatch or body-read failure: the server may already have processed it.
    Transport(reqwest::Error),
    HttpStatus(u16),
    RequestTooLarge {
        limit: usize,
    },
    ResponseTooLarge {
        limit: usize,
    },
    Allocation(TryReserveError),
    /// Unwinding dependency panic; completion may be uncertain after dispatch.
    DependencyPanicked,
}

impl fmt::Display for InferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => write!(f, "invalid inference configuration: {error}"),
            Self::InvalidHistory(error) => write!(f, "invalid inference history: {error}"),
            Self::Json(error) => write!(f, "invalid inference JSON: {error}"),
            Self::InvalidResponse(error) => write!(f, "invalid inference response: {error}"),
            Self::UnsupportedTool(name) => write!(f, "unsupported inference tool: {name}"),
            Self::TruncatedResponse => write!(f, "inference response reached the token limit"),
            Self::ClientBuild(error) => write!(f, "could not build inference client: {error}"),
            Self::Transport(error) => {
                write!(f, "inference transport failed; completion unknown: {error}")
            }
            Self::HttpStatus(status) => write!(f, "inference HTTP status {status}"),
            Self::RequestTooLarge { limit } => write!(f, "inference request exceeds {limit} bytes"),
            Self::ResponseTooLarge { limit } => {
                write!(f, "inference response exceeds {limit} bytes")
            }
            Self::Allocation(error) => {
                write!(f, "could not allocate inference response buffer: {error}")
            }
            Self::DependencyPanicked => write!(
                f,
                "inference dependency panicked; completion may be unknown"
            ),
        }
    }
}

impl std::error::Error for InferenceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::ClientBuild(error) | Self::Transport(error) => Some(error),
            Self::Allocation(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for InferenceError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
mod tests;
