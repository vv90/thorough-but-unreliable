//! Podman exec adapter with a pure core and a Unix-socket transport. Trusted
//! setup prepares the container; external supervision owns target teardown.
//! Uses the unversioned Docker-compatible exec endpoints supported by Podman.

mod config;
mod session;
mod stream;
mod transport;
mod wire;

pub use config::{Config, Limits, Settings};
pub use session::{Capture, Create, Failure, Inspect, Inspection, Session, Start};
pub use transport::{PodmanTargetSession, SupervisionEvent};

use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
};

#[derive(Debug)]
pub enum Error {
    Configuration(&'static str),
    BodyTooLarge { limit: usize },
    Json(serde_json::Error),
    Allocation(std::collections::TryReserveError),
    InvalidResponse(&'static str),
    HttpStatus(u16),
    RuntimeFailure,
    DependencyPanicked,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(field) => write!(f, "invalid Podman configuration: {field}"),
            Self::BodyTooLarge { limit } => write!(f, "Podman JSON exceeds {limit} bytes"),
            Self::Json(error) => write!(f, "invalid Podman JSON: {error}"),
            Self::Allocation(error) => write!(f, "Podman buffer allocation failed: {error}"),
            Self::InvalidResponse(reason) => write!(f, "invalid Podman response: {reason}"),
            Self::HttpStatus(status) => write!(f, "unexpected Podman HTTP status {status}"),
            Self::RuntimeFailure => f.write_str("Podman reported an execution stream error"),
            Self::DependencyPanicked => f.write_str("Podman dependency panicked"),
        }
    }
}
impl std::error::Error for Error {}

fn guard<T>(f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            Err(Error::DependencyPanicked)
        }
    }
}

#[cfg(test)]
mod tests;
